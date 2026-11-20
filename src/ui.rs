//! Read-only web dashboard: shows the running devices, their points and what the simulator has been doing.
//! A deliberately tiny HTTP server, so the program stays a single small executable.

use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::sim::{connect_host, Shared};

const PAGE: &str = include_str!("ui.html");
const PORT_ATTEMPTS: u16 = 20;
const MAX_REQUEST_BYTES: usize = 8192;
const EVENTS_SHOWN: usize = 150;
const REQUESTS_SHOWN: usize = 25;

/// Start listening and return the address to open in a browser.
pub async fn start(shared: Shared, port: u16) -> Result<String> {
    let bind = shared.lock().unwrap().cfg.bind;
    let mut candidate = port;
    let listener = loop {
        match TcpListener::bind((bind, candidate)).await {
            Ok(l) => break l,
            Err(e) => {
                if candidate - port >= PORT_ATTEMPTS || candidate == u16::MAX {
                    bail!("no free port between {} and {} ({})", port, candidate, e);
                }
                candidate += 1;
            }
        }
    };
    let host = if bind.is_unspecified() {
        "127.0.0.1".to_string()
    } else if bind.is_ipv6() {
        format!("[{}]", bind)
    } else {
        bind.to_string()
    };
    let url = format!("http://{}:{}", host, candidate);
    shared.lock().unwrap().cfg.ui_url = Some(url.clone());

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(serve(shared.clone(), stream));
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    });
    Ok(url)
}

async fn serve(shared: Shared, mut stream: TcpStream) {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    // Read the request head; give up on slow or oversized requests.
    let head_complete = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return false,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        return true;
                    }
                    if buf.len() > MAX_REQUEST_BYTES {
                        return false;
                    }
                }
            }
        }
    })
    .await
    .unwrap_or(false);
    if !head_complete {
        return;
    }

    let head = String::from_utf8_lossy(&buf);
    let mut parts = head.lines().next().unwrap_or("").split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let path = target.split('?').next().unwrap_or("");

    let (status, content_type, body) = if method != "GET" {
        ("405 Method Not Allowed", "text/plain; charset=utf-8", "This dashboard is read-only.".to_string())
    } else {
        route(&shared, path)
    };
    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        status,
        content_type,
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn route(shared: &Shared, path: &str) -> (&'static str, &'static str, String) {
    const JSON: &str = "application/json; charset=utf-8";
    match path {
        "/" | "/index.html" => ("200 OK", "text/html; charset=utf-8", PAGE.to_string()),
        "/api/state" => ("200 OK", JSON, state(shared).to_string()),
        _ => match path.strip_prefix("/api/device/").and_then(|id| device(shared, id)) {
            Some(v) => ("200 OK", JSON, v.to_string()),
            None => ("404 Not Found", "text/plain; charset=utf-8", "Not found".to_string()),
        },
    }
}

/// Everything the overview needs: one short record per device plus the activity feed.
fn state(shared: &Shared) -> Value {
    let sim = shared.lock().unwrap();
    let (now, bind) = (sim.now(), sim.cfg.bind);
    let devices: Vec<Value> = sim.devices.values().map(|d| d.summary(bind, now)).collect();
    let skip = sim.events.len().saturating_sub(EVENTS_SHOWN);
    let events: Vec<_> = sim.events.iter().skip(skip).collect();
    json!({
        "name": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_s": now.round(),
        "host": connect_host(bind),
        "max_devices": sim.cfg.max_devices,
        "devices": devices,
        "events": events,
    })
}

fn device(shared: &Shared, id: &str) -> Option<Value> {
    let sim = shared.lock().unwrap();
    let (now, bind) = (sim.now(), sim.cfg.bind);
    let d = sim.devices.get(id)?;
    let mut v = d.detail(bind, now);
    let skip = d.log.len().saturating_sub(REQUESTS_SHOWN);
    v["recent_requests"] = json!(d.log.iter().skip(skip).collect::<Vec<_>>());
    Some(v)
}
