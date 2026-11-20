//! Multi-device Modbus TCP simulator with a built-in MCP server.

mod mcp;
mod model;
mod net;
mod sim;
mod templates;
mod ui;

use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use model::Project;
use sim::{Config, Sim};

#[derive(Parser)]
#[command(name = env!("CARGO_PKG_NAME"), version, about = "Multi-device Modbus TCP simulator with a built-in MCP server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run as an MCP server on stdin/stdout (this is what Claude Desktop and other MCP clients start).
    Mcp {
        #[command(flatten)]
        opts: Opts,
        /// Project file to load at startup.
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Run the devices of a project file until Ctrl+C. No AI client needed.
    Run {
        /// Project file (YAML).
        project: PathBuf,
        #[command(flatten)]
        opts: Opts,
    },
    /// List the built-in device templates.
    Templates,
}

#[derive(Args)]
struct Opts {
    /// Address to listen on. Use 0.0.0.0 to accept connections from other machines.
    #[arg(long, default_value = "127.0.0.1")]
    bind: IpAddr,
    /// First port handed out to devices that do not ask for a specific one.
    #[arg(long, default_value_t = 5020)]
    base_port: u16,
    /// Upper limit on simultaneously running devices.
    #[arg(long, default_value_t = 1000)]
    max_devices: usize,
    /// How often signals are updated, in milliseconds.
    #[arg(long, default_value_t = 200)]
    tick_ms: u64,
    /// Folder for projects saved through MCP. Default: ~/.fieldpuppet/projects
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Port of the web dashboard. The next free port is used if this one is taken.
    #[arg(long, default_value_t = 5080)]
    ui_port: u16,
    /// Do not start the web dashboard.
    #[arg(long)]
    no_ui: bool,
}

impl Opts {
    fn config(self) -> Config {
        let data_dir = self.data_dir.unwrap_or_else(|| {
            let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from);
            home.unwrap_or_else(|| PathBuf::from(".")).join(concat!(".", env!("CARGO_PKG_NAME"))).join("projects")
        });
        Config {
            bind: self.bind,
            base_port: self.base_port,
            max_devices: self.max_devices,
            tick_ms: self.tick_ms,
            data_dir,
            ui_url: None,
        }
    }
}

/// Start the dashboard unless it was switched off. A dashboard that cannot start never stops the simulator.
async fn start_ui(shared: &sim::Shared, port: u16, disabled: bool) -> Option<String> {
    if disabled {
        return None;
    }
    match ui::start(shared.clone(), port).await {
        Ok(url) => Some(url),
        Err(e) => {
            eprintln!("dashboard not started: {:#}", e);
            None
        }
    }
}

fn read_project(path: &PathBuf) -> Result<Project> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    serde_yaml::from_str(&text).with_context(|| format!("{} is not a valid project file", path.display()))
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Templates => {
            use std::io::Write;
            // Build the text first and ignore a closed pipe (for example when piped into head).
            let mut text = String::new();
            for t in templates::TEMPLATES {
                text.push_str(&format!("{:<24} {}\n", t.name, t.description));
                for p in (t.points)() {
                    let unit = if p.unit.is_empty() { String::new() } else { format!(" [{}]", p.unit) };
                    text.push_str(&format!("    {:<20} {:?} {} ({:?}){}\n", p.name, p.table, p.address, p.data_type, unit));
                }
            }
            let _ = std::io::stdout().write_all(text.as_bytes());
            Ok(())
        }

        Command::Mcp { opts, project } => {
            // stdout carries the protocol, so everything for humans goes to stderr.
            let (ui_port, no_ui) = (opts.ui_port, opts.no_ui);
            let shared = Sim::new(opts.config());
            sim::spawn_ticker(shared.clone());
            if let Some(url) = start_ui(&shared, ui_port, no_ui).await {
                eprintln!("dashboard: {}", url);
            }
            if let Some(path) = project {
                let created = sim::load_project(&shared, read_project(&path)?).await?;
                eprintln!("loaded {} device(s) from {}", created.len(), path.display());
            }
            eprintln!("{} {} ready (MCP on stdio)", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            mcp::serve_stdio(shared).await
        }

        Command::Run { project, opts } => {
            let (ui_port, no_ui) = (opts.ui_port, opts.no_ui);
            let shared = Sim::new(opts.config());
            sim::spawn_ticker(shared.clone());
            let dashboard = start_ui(&shared, ui_port, no_ui).await;
            let created = sim::load_project(&shared, read_project(&project)?).await?;
            if created.is_empty() {
                anyhow::bail!("{} contains no devices", project.display());
            }
            println!("{} device(s) running:", created.len());
            for d in &created {
                println!(
                    "  {:<24} {}:{}  unit {}  ({} points)",
                    d["id"].as_str().unwrap_or(""),
                    d["host"].as_str().unwrap_or(""),
                    d["port"],
                    d["unit_id"],
                    d["points"]
                );
            }
            shared.lock().unwrap().note(None, format!("Loaded {} device(s) from {}", created.len(), project.display()));
            if let Some(url) = dashboard {
                println!("Dashboard: {}", url);
            }
            println!("Press Ctrl+C to stop.");
            tokio::signal::ctrl_c().await?;
            Ok(())
        }
    }
}
