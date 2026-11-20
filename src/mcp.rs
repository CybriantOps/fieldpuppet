//! MCP server over stdio (newline-delimited JSON-RPC 2.0). Only the `tools` capability is offered.

use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::model::*;
use crate::sim::{self, connect_host, Shared};
use crate::templates;

/// Revisions without a handshake: every request names its version in `_meta`.
const MODERN_PROTOCOLS: &[&str] = &["2026-07-28"];
/// Revisions that open with `initialize`.
const LEGACY_PROTOCOLS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
const ERR_UNSUPPORTED_VERSION: i64 = -32022;

pub async fn serve_stdio(shared: Shared) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(line) {
            Ok(msg) => handle_message(&shared, msg).await,
            Err(e) => Some(error_reply(Value::Null, -32700, &format!("parse error: {}", e))),
        };
        if let Some(reply) = reply {
            let mut text = serde_json::to_string(&reply)?;
            text.push('\n');
            out.write_all(text.as_bytes()).await?;
            out.flush().await?;
        }
    }
    Ok(())
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn server_info() -> Value {
    json!({ "name": env!("CARGO_PKG_NAME"), "version": env!("CARGO_PKG_VERSION") })
}

async fn tool_call_result(shared: &Shared, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let outcome = call_tool(shared, &name, args.clone()).await;
    if let Some((device, text)) = describe_call(&name, &args, &outcome) {
        shared.lock().unwrap().note(device.as_deref(), text);
    }
    match outcome {
        Ok(value) => json!({
            "content": [{ "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() }],
            "isError": false,
        }),
        Err(e) => json!({
            "content": [{ "type": "text", "text": format!("{:#}", e) }],
            "isError": true,
        }),
    }
}

/// Activity-feed line for a tool call that changes something. Read-only tools give nothing.
fn describe_call(name: &str, args: &Value, outcome: &Result<Value>) -> Option<(Option<String>, String)> {
    const CHANGING: &[&str] = &[
        "create_device", "clone_device", "remove_device", "remove_all_devices", "set_point", "set_signal",
        "add_point", "remove_point", "write_registers", "apply_scenario", "stop_scenario", "inject_fault",
        "clear_fault", "save_project", "load_project",
    ];
    if !CHANGING.contains(&name) {
        return None;
    }
    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("?").to_string();
    let device = args.get("device_id").and_then(Value::as_str).map(str::to_string);
    let result = match outcome {
        Ok(v) => v,
        Err(e) => return Some((device, format!("Request '{}' failed: {:#}", name, e))),
    };
    let text = match name {
        "create_device" => {
            let id = result["id"].as_str().unwrap_or("?").to_string();
            let kind = args.get("template").and_then(Value::as_str).map(|t| format!(" from template '{}'", t)).unwrap_or_default();
            return Some((
                Some(id),
                format!("Device created{} on port {}, unit {} ({} points)", kind, result["port"], result["unit_id"], result["points"]),
            ));
        }
        "clone_device" => format!("Created {} copies of '{}'", result["created"], s("source_id")),
        "remove_device" => "Device removed".to_string(),
        "remove_all_devices" => format!("All devices removed ({})", result["removed"]),
        "set_point" => format!("Set {} = {}", s("point"), args["value"]),
        "set_signal" => match args.get("signal") {
            Some(Value::Null) | None => format!("Stopped signal on {}", s("point")),
            Some(sig) => format!("Started {} signal on {}", sig["type"].as_str().unwrap_or("a"), s("point")),
        },
        "add_point" => format!("Added point {}", args["point"]["name"].as_str().unwrap_or("?")),
        "remove_point" => format!("Removed point {}", s("point")),
        "write_registers" => format!("Wrote {} raw value(s) to {} address {}", result["written"], s("table"), args["address"]),
        "apply_scenario" => {
            let sc = &args["scenario"];
            let n = sc["steps"].as_array().map(Vec::len).unwrap_or(0);
            match sc["name"].as_str().filter(|n| !n.is_empty()) {
                Some(name) => format!("Scenario '{}' started ({} steps)", name, n),
                None => format!("Scenario started ({} steps)", n),
            }
        }
        "stop_scenario" => "Scenario stopped".to_string(),
        "inject_fault" => format!("Fault injected: {}", result["fault"]["text"].as_str().unwrap_or("?")),
        "clear_fault" => "Fault cleared".to_string(),
        "save_project" => format!("Project '{}' saved ({} devices)", s("name"), result["devices"]),
        "load_project" => format!("Project '{}' loaded", s("name")),
        _ => return None,
    };
    Some((device, text))
}

/// Serves both protocol eras on the same pipe: a request that names its version in `_meta`
/// (or asks for `server/discover`) is answered statelessly; `initialize` selects the older handshake.
async fn handle_message(shared: &Shared, msg: Value) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str)?.to_string();
    // No id means a notification: nothing to answer.
    let id = msg.get("id").cloned()?;
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let declared = params.get("_meta").and_then(|m| m.get(META_VERSION)).and_then(Value::as_str).map(str::to_string);

    if declared.is_some() || method == "server/discover" {
        if let Some(v) = &declared {
            if !MODERN_PROTOCOLS.contains(&v.as_str()) {
                let supported: Vec<&str> = MODERN_PROTOCOLS.iter().chain(LEGACY_PROTOCOLS).copied().collect();
                return Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": ERR_UNSUPPORTED_VERSION,
                        "message": "Unsupported protocol version",
                        "data": { "supported": supported, "requested": v },
                    }
                }));
            }
        }
        let mut result = match method.as_str() {
            "server/discover" => json!({
                "supportedVersions": MODERN_PROTOCOLS.iter().chain(LEGACY_PROTOCOLS).collect::<Vec<_>>(),
                "capabilities": { "tools": {} },
                "instructions": INSTRUCTIONS,
                "ttlMs": 3_600_000,
                "cacheScope": "public",
            }),
            // The tool set never changes while the process runs, so clients may cache it.
            "tools/list" => json!({ "tools": tool_list(), "ttlMs": 3_600_000, "cacheScope": "public" }),
            "tools/call" => tool_call_result(shared, &params).await,
            _ => return Some(error_reply(id, -32601, &format!("method not found: {}", method))),
        };
        result["resultType"] = json!("complete");
        result["_meta"] = json!({ META_SERVER_INFO: server_info() });
        return Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    let result = match method.as_str() {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let version = if LEGACY_PROTOCOLS.contains(&asked) { asked } else { LEGACY_PROTOCOLS[0] };
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": server_info(),
                "instructions": INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tool_list() }),
        "tools/call" => tool_call_result(shared, &params).await,
        _ => return Some(error_reply(id, -32601, &format!("method not found: {}", method))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

const INSTRUCTIONS: &str = "This server runs simulated Modbus TCP devices on this computer for testing SCADA, \
gateway and PLC-integration software. Nothing here talks to real equipment. Typical flow: list_templates, \
create_device (returns the host, port and unit id a Modbus client should connect to), then set_point / \
set_signal / apply_scenario / inject_fault to shape behaviour, and get_traffic_log to see what the software \
under test is asking for. Addresses are zero-based protocol addresses (holding register 40001 is address 0).";

const SIGNAL_HELP: &str = "Signal object. One of: \
{\"type\":\"constant\",\"value\":N} | \
{\"type\":\"ramp\",\"from\":N,\"to\":N,\"period_s\":N,\"repeat\":true} | \
{\"type\":\"sine\",\"min\":N,\"max\":N,\"period_s\":N} | \
{\"type\":\"random\",\"min\":N,\"max\":N} | \
{\"type\":\"random_walk\",\"min\":N,\"max\":N,\"step\":N} | \
{\"type\":\"counter\",\"per_second\":N,\"rollover\":N (optional)} | \
{\"type\":\"square\",\"low\":N,\"high\":N,\"period_s\":N}. Values are engineering values (after scaling).";

const FAULT_HELP: &str = "Fault object. One of: {\"type\":\"delay\",\"ms\":N} (slow answers) | \
{\"type\":\"no_response\"} (client times out) | {\"type\":\"exception\",\"code\":N} (Modbus exception, e.g. 4 device \
failure, 6 busy) | {\"type\":\"disconnect\"} (drop the TCP connection on every request). Add \"duration_s\":N to clear \
it automatically.";

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required },
    })
}

fn point_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "table": { "type": "string", "enum": ["coil", "discrete", "holding", "input"] },
            "address": { "type": "integer", "minimum": 0, "maximum": 65535, "description": "Zero-based protocol address." },
            "data_type": { "type": "string", "enum": ["bool", "u16", "i16", "u32", "i32", "f32"], "description": "Default u16. 32-bit types use two registers." },
            "scale": { "type": "number", "description": "Engineering value = raw * scale. Default 1." },
            "unit": { "type": "string" },
            "word_swap": { "type": "boolean", "description": "For 32-bit types: low word first." },
            "value": { "type": "number", "description": "Initial engineering value." },
            "signal": { "type": "object", "description": SIGNAL_HELP },
            "description": { "type": "string" }
        },
        "required": ["name", "table", "address"]
    })
}

fn scenario_schema() -> Value {
    json!({
        "type": "object",
        "description": "Timeline of steps for one device.",
        "properties": {
            "name": { "type": "string" },
            "repeat": { "type": "boolean", "description": "Start over after the last step." },
            "steps": {
                "type": "array",
                "items": {
                    "type": "object",
                    "description": "at_s is seconds after the scenario starts. action is one of: \
set_point (point, value) | set_signal (point, signal) | clear_signal (point) | fault (fault) | clear_fault.",
                    "properties": {
                        "at_s": { "type": "number", "minimum": 0 },
                        "action": { "type": "string", "enum": ["set_point", "set_signal", "clear_signal", "fault", "clear_fault"] },
                        "point": { "type": "string" },
                        "value": { "type": "number" },
                        "signal": { "type": "object", "description": SIGNAL_HELP },
                        "fault": { "type": "object", "description": FAULT_HELP }
                    },
                    "required": ["at_s", "action"]
                }
            }
        },
        "required": ["steps"]
    })
}

fn tool_list() -> Value {
    let dev = json!({ "type": "string", "description": "Device id as returned by create_device or list_devices." });
    json!([
        tool("list_templates", "List the built-in device templates and the points each one provides.", json!({}), &[]),
        tool(
            "create_device",
            "Start a simulated Modbus TCP device. Without a port it gets the next free port; give a port that is already in use by this simulator together with a different unit_id to put several devices behind one port. Returns the host, port and unit id to connect to.",
            json!({
                "template": { "type": "string", "description": "Built-in template name (see list_templates). Optional." },
                "id": { "type": "string", "description": "Optional id: letters, digits, '-' and '_'." },
                "name": { "type": "string", "description": "Optional display name." },
                "port": { "type": "integer", "minimum": 1, "maximum": 65535, "description": "Optional TCP port. Omit for the next free port." },
                "unit_id": { "type": "integer", "minimum": 0, "maximum": 255, "description": "Modbus unit id. Default 1." },
                "strict": { "type": "boolean", "description": "Answer 'illegal data address' for addresses no point covers. Default false (unmapped addresses read 0)." },
                "points": { "type": "array", "items": point_schema(), "description": "Extra points, or replacements for template points with the same name." },
                "scenario": scenario_schema()
            }),
            &[],
        ),
        tool(
            "clone_device",
            "Create several copies of an existing device, each on its own port (for example 50 identical meters).",
            json!({
                "source_id": dev,
                "count": { "type": "integer", "minimum": 1, "maximum": 1000 },
                "start_port": { "type": "integer", "minimum": 1, "maximum": 65535, "description": "Optional first port; copies use consecutive ports. Omit for the next free ports." },
                "id_prefix": { "type": "string", "description": "Optional id prefix; copies are named <prefix>-1, <prefix>-2, ..." }
            }),
            &["source_id", "count"],
        ),
        tool("list_devices", "List running devices with their address, fault and scenario state.", json!({}), &[]),
        tool("get_device", "Full detail of one device, including every point and its current value.", json!({ "device_id": dev }), &["device_id"]),
        tool("remove_device", "Stop one device and free its port.", json!({ "device_id": dev }), &["device_id"]),
        tool("remove_all_devices", "Stop every device.", json!({}), &[]),
        tool(
            "read_points",
            "Read current engineering values of a device's points (all points when 'points' is omitted).",
            json!({ "device_id": dev, "points": { "type": "array", "items": { "type": "string" } } }),
            &["device_id"],
        ),
        tool(
            "set_point",
            "Set a point to a fixed engineering value. Stops any signal running on that point.",
            json!({ "device_id": dev, "point": { "type": "string" }, "value": { "type": "number" } }),
            &["device_id", "point", "value"],
        ),
        tool(
            "set_signal",
            "Make a point change over time (sine, ramp, random, counter, ...). Pass signal null to stop the signal and keep the current value.",
            json!({ "device_id": dev, "point": { "type": "string" }, "signal": { "type": ["object", "null"], "description": SIGNAL_HELP } }),
            &["device_id", "point", "signal"],
        ),
        tool("add_point", "Add a point to a running device.", json!({ "device_id": dev, "point": point_schema() }), &["device_id", "point"]),
        tool("remove_point", "Remove a point from a running device.", json!({ "device_id": dev, "point": { "type": "string" } }), &["device_id", "point"]),
        tool(
            "read_registers",
            "Read raw values straight from a Modbus table, exactly as a client would receive them.",
            json!({
                "device_id": dev,
                "table": { "type": "string", "enum": ["coil", "discrete", "holding", "input"] },
                "address": { "type": "integer", "minimum": 0, "maximum": 65535 },
                "quantity": { "type": "integer", "minimum": 1, "maximum": 125 }
            }),
            &["device_id", "table", "address", "quantity"],
        ),
        tool(
            "write_registers",
            "Write raw values into a Modbus table (0-65535 for registers, 0 or 1 for coils and discrete inputs). Unlike a Modbus client this can also write the read-only tables.",
            json!({
                "device_id": dev,
                "table": { "type": "string", "enum": ["coil", "discrete", "holding", "input"] },
                "address": { "type": "integer", "minimum": 0, "maximum": 65535 },
                "values": { "type": "array", "items": { "type": "integer" }, "minItems": 1, "maxItems": 125 }
            }),
            &["device_id", "table", "address", "values"],
        ),
        tool(
            "apply_scenario",
            "Play a timeline of steps on one device, starting now. Replaces any scenario already running on it.",
            json!({ "device_id": dev, "scenario": scenario_schema() }),
            &["device_id", "scenario"],
        ),
        tool("stop_scenario", "Stop the scenario on a device. Values, signals and faults stay as they are.", json!({ "device_id": dev }), &["device_id"]),
        tool(
            "inject_fault",
            "Make a device misbehave on the wire: slow answers, no answer, exception replies or dropped connections.",
            json!({ "device_id": dev, "fault": { "type": "object", "description": FAULT_HELP } }),
            &["device_id", "fault"],
        ),
        tool("clear_fault", "Return a device to normal communication.", json!({ "device_id": dev }), &["device_id"]),
        tool(
            "get_traffic_log",
            "Most recent Modbus requests a device received (function code, address, quantity, result). Useful to see what the software under test is polling.",
            json!({ "device_id": dev, "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Default 50." } }),
            &["device_id"],
        ),
        tool(
            "save_project",
            "Save all running devices (points, current values, signals, ports, running scenarios) to a named project file.",
            json!({ "name": { "type": "string", "description": "Letters, digits, '-' and '_'." } }),
            &["name"],
        ),
        tool(
            "load_project",
            "Start the devices stored in a saved project.",
            json!({ "name": { "type": "string" }, "replace": { "type": "boolean", "description": "Remove the currently running devices first. Default false." } }),
            &["name"],
        ),
        tool("list_projects", "List saved projects.", json!({}), &[]),
    ])
}

fn parse<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T> {
    serde_json::from_value(args).map_err(|e| anyhow!("invalid arguments: {}", e))
}

#[derive(Deserialize)]
struct DevArg {
    device_id: String,
}

fn project_path(shared: &Shared, name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("project name may only contain letters, digits, '-' and '_' (max 64 characters)");
    }
    let dir = shared.lock().unwrap().cfg.data_dir.clone();
    Ok(dir.join(format!("{}.yaml", name)))
}

async fn call_tool(shared: &Shared, name: &str, args: Value) -> Result<Value> {
    match name {
        "list_templates" => Ok(Value::Array(
            templates::TEMPLATES
                .iter()
                .map(|t| {
                    let points: Vec<Value> = (t.points)()
                        .iter()
                        .map(|p| json!({ "name": p.name, "table": p.table, "address": p.address, "data_type": p.data_type, "unit": p.unit, "description": p.description }))
                        .collect();
                    json!({ "name": t.name, "description": t.description, "points": points })
                })
                .collect(),
        )),

        "create_device" => sim::create_device(shared, parse::<DeviceSpec>(args)?).await,

        "clone_device" => {
            #[derive(Deserialize)]
            struct A {
                source_id: String,
                count: u32,
                start_port: Option<u16>,
                id_prefix: Option<String>,
            }
            let a: A = parse(args)?;
            if a.count == 0 || a.count > 1000 {
                bail!("count must be between 1 and 1000");
            }
            let template = shared.lock().unwrap().device(&a.source_id)?.export();
            let prefix = a.id_prefix.unwrap_or_else(|| a.source_id.clone());
            let mut created = Vec::new();
            for i in 0..a.count {
                let mut spec = template.clone();
                spec.id = Some(format!("{}-{}", prefix, i + 1));
                spec.name = None;
                spec.port = match a.start_port {
                    Some(p) => Some(
                        u16::try_from(p as u32 + i).map_err(|_| anyhow!("port range runs past 65535 after {} copies", i))?,
                    ),
                    None => None,
                };
                match sim::create_device(shared, spec).await {
                    Ok(v) => created.push(json!({ "id": v["id"], "port": v["port"], "unit_id": v["unit_id"] })),
                    Err(e) => {
                        return Err(e.context(format!("created {} of {} copies before failing", created.len(), a.count)))
                    }
                }
            }
            Ok(json!({ "created": created.len(), "devices": created }))
        }

        "list_devices" => {
            let sim = shared.lock().unwrap();
            let now = sim.now();
            let bind = sim.cfg.bind;
            let list: Vec<Value> = sim.devices.values().map(|d| d.summary(bind, now)).collect();
            Ok(json!({ "count": list.len(), "host": connect_host(bind), "dashboard": sim.cfg.ui_url, "devices": list }))
        }

        "get_device" => {
            let a: DevArg = parse(args)?;
            let mut sim = shared.lock().unwrap();
            let (now, bind) = (sim.now(), sim.cfg.bind);
            Ok(sim.device(&a.device_id)?.detail(bind, now))
        }

        "remove_device" => {
            let a: DevArg = parse(args)?;
            sim::remove_device(shared, &a.device_id).await
        }

        "remove_all_devices" => Ok(json!({ "removed": sim::remove_all(shared).await })),

        "read_points" => {
            #[derive(Deserialize)]
            struct A {
                device_id: String,
                points: Option<Vec<String>>,
            }
            let a: A = parse(args)?;
            let mut sim = shared.lock().unwrap();
            let d = sim.device(&a.device_id)?;
            if let Some(names) = &a.points {
                for n in names {
                    if !d.points.iter().any(|p| &p.def.name == n) {
                        bail!("device '{}' has no point '{}'", d.id, n);
                    }
                }
            }
            Ok(d.points_json(a.points.as_deref()))
        }

        "set_point" => {
            #[derive(Deserialize)]
            struct A {
                device_id: String,
                point: String,
                value: f64,
            }
            let a: A = parse(args)?;
            let mut sim = shared.lock().unwrap();
            let d = sim.device(&a.device_id)?;
            d.set_point(&a.point, a.value)?;
            Ok(d.points_json(Some(&[a.point])))
        }

        "set_signal" => {
            #[derive(Deserialize)]
            struct A {
                device_id: String,
                point: String,
                signal: Option<Signal>,
            }
            let a: A = parse(args)?;
            let mut sim = shared.lock().unwrap();
            let now = sim.now();
            let d = sim.device(&a.device_id)?;
            match a.signal {
                Some(s) => d.set_signal(&a.point, s, now)?,
                None => d.clear_signal(&a.point)?,
            }
            Ok(d.points_json(Some(&[a.point])))
        }

        "add_point" => {
            #[derive(Deserialize)]
            struct A {
                device_id: String,
                point: Point,
            }
            let a: A = parse(args)?;
            let mut sim = shared.lock().unwrap();
            let now = sim.now();
            let d = sim.device(&a.device_id)?;
            let name = a.point.name.clone();
            d.add_point(a.point, now)?;
            Ok(d.points_json(Some(&[name])))
        }

        "remove_point" => {
            #[derive(Deserialize)]
            struct A {
                device_id: String,
                point: String,
            }
            let a: A = parse(args)?;
            shared.lock().unwrap().device(&a.device_id)?.remove_point(&a.point)?;
            Ok(json!({ "removed": a.point }))
        }

        "read_registers" => {
            #[derive(Deserialize)]
            struct A {
                device_id: String,
                table: Table,
                address: u16,
                quantity: u16,
            }
            let a: A = parse(args)?;
            if a.quantity == 0 || a.quantity > 125 || a.address as u32 + a.quantity as u32 > 65536 {
                bail!("quantity must be 1-125 and stay within addresses 0-65535");
            }
            let mut sim = shared.lock().unwrap();
            let d = sim.device(&a.device_id)?;
            let values: Vec<u16> = (0..a.quantity)
                .map(|i| if a.table.is_bit() { d.bit(a.table, a.address + i) as u16 } else { d.word(a.table, a.address + i) })
                .collect();
            Ok(json!({ "table": a.table, "address": a.address, "values": values }))
        }

        "write_registers" => {
            #[derive(Deserialize)]
            struct A {
                device_id: String,
                table: Table,
                address: u16,
                values: Vec<i64>,
            }
            let a: A = parse(args)?;
            if a.values.is_empty() || a.values.len() > 125 || a.address as usize + a.values.len() > 65536 {
                bail!("give 1-125 values that stay within addresses 0-65535");
            }
            let limit = if a.table.is_bit() { 1 } else { 65535 };
            if a.values.iter().any(|v| *v < 0 || *v > limit) {
                bail!("values must be between 0 and {}", limit);
            }
            let mut sim = shared.lock().unwrap();
            let d = sim.device(&a.device_id)?;
            for (i, v) in a.values.iter().enumerate() {
                let addr = a.address + i as u16;
                if a.table.is_bit() {
                    d.set_bit(a.table, addr, *v != 0);
                } else {
                    d.set_word(a.table, addr, *v as u16);
                }
            }
            Ok(json!({ "written": a.values.len(), "table": a.table, "address": a.address }))
        }

        "apply_scenario" => {
            #[derive(Deserialize)]
            struct A {
                device_id: String,
                scenario: Scenario,
            }
            let a: A = parse(args)?;
            let mut sim = shared.lock().unwrap();
            let (now, bind) = (sim.now(), sim.cfg.bind);
            let d = sim.device(&a.device_id)?;
            d.apply_scenario(a.scenario, now)?;
            Ok(d.summary(bind, now))
        }

        "stop_scenario" => {
            let a: DevArg = parse(args)?;
            let mut sim = shared.lock().unwrap();
            let d = sim.device(&a.device_id)?;
            let was_running = d.scenario.take().is_some();
            Ok(json!({ "device_id": a.device_id, "stopped": was_running }))
        }

        "inject_fault" => {
            #[derive(Deserialize)]
            struct A {
                device_id: String,
                fault: Fault,
            }
            let a: A = parse(args)?;
            let mut sim = shared.lock().unwrap();
            let (now, bind) = (sim.now(), sim.cfg.bind);
            let d = sim.device(&a.device_id)?;
            d.inject_fault(a.fault, now)?;
            Ok(d.summary(bind, now))
        }

        "clear_fault" => {
            let a: DevArg = parse(args)?;
            let mut sim = shared.lock().unwrap();
            let d = sim.device(&a.device_id)?;
            let had_fault = d.fault.take().is_some();
            Ok(json!({ "device_id": a.device_id, "cleared": had_fault }))
        }

        "get_traffic_log" => {
            #[derive(Deserialize)]
            struct A {
                device_id: String,
                limit: Option<usize>,
            }
            let a: A = parse(args)?;
            let limit = a.limit.unwrap_or(50).clamp(1, 500);
            let mut sim = shared.lock().unwrap();
            let d = sim.device(&a.device_id)?;
            let skip = d.log.len().saturating_sub(limit);
            let entries: Vec<&sim::LogEntry> = d.log.iter().skip(skip).collect();
            Ok(json!({ "device_id": a.device_id, "requests_served": d.requests, "entries": entries }))
        }

        "save_project" => {
            #[derive(Deserialize)]
            struct A {
                name: String,
            }
            let a: A = parse(args)?;
            let path = project_path(shared, &a.name)?;
            let project = shared.lock().unwrap().export();
            if project.devices.is_empty() {
                bail!("there are no devices to save");
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
            }
            std::fs::write(&path, serde_yaml::to_string(&project)?).with_context(|| format!("cannot write {}", path.display()))?;
            Ok(json!({ "saved": a.name, "file": path.display().to_string(), "devices": project.devices.len() }))
        }

        "load_project" => {
            #[derive(Deserialize)]
            struct A {
                name: String,
                #[serde(default)]
                replace: bool,
            }
            let a: A = parse(args)?;
            let path = project_path(shared, &a.name)?;
            let text = std::fs::read_to_string(&path).with_context(|| format!("cannot read project '{}' ({})", a.name, path.display()))?;
            let project: Project = serde_yaml::from_str(&text).with_context(|| format!("{} is not a valid project file", path.display()))?;
            if a.replace {
                sim::remove_all(shared).await;
            }
            let created = sim::load_project(shared, project).await?;
            Ok(json!({ "loaded": a.name, "devices": created }))
        }

        "list_projects" => {
            let dir = shared.lock().unwrap().cfg.data_dir.clone();
            let mut names = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.extension().is_some_and(|x| x == "yaml") {
                        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                            names.push(stem.to_string());
                        }
                    }
                }
            }
            names.sort();
            Ok(json!({ "directory": dir.display().to_string(), "projects": names }))
        }

        other => bail!("unknown tool '{}'", other),
    }
}
