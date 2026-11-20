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

/// `kind` sets the behaviour hints clients show to users:
/// "read" changes nothing, "change" edits simulator state, "remove" deletes or overwrites something.
fn tool(name: &str, title: &str, kind: &str, idempotent: bool, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required },
        "annotations": {
            "title": title,
            "readOnlyHint": kind == "read",
            "destructiveHint": kind == "remove",
            "idempotentHint": idempotent,
            // Everything happens inside this simulator; no outside system is touched.
            "openWorldHint": false,
        },
    })
}

fn point_schema() -> Value {
    json!({
        "type": "object",
        "description": "Definition of one point: a named value stored at a Modbus address.",
        "properties": {
            "name": { "type": "string", "description": "Point name, unique within the device. Used by set_point, set_signal and read_points." },
            "table": { "type": "string", "enum": ["coil", "discrete", "holding", "input"], "description": "Modbus table: coil and discrete hold bits, holding and input hold 16-bit registers." },
            "address": { "type": "integer", "minimum": 0, "maximum": 65535, "description": "Zero-based protocol address (holding register 40001 is address 0)." },
            "data_type": { "type": "string", "enum": ["bool", "u16", "i16", "u32", "i32", "f32"], "description": "Default u16. 32-bit types use two registers." },
            "scale": { "type": "number", "description": "Engineering value = raw * scale. Default 1." },
            "unit": { "type": "string", "description": "Engineering unit shown next to the value, for example kW or bar. Informational only." },
            "word_swap": { "type": "boolean", "description": "For 32-bit types: low word first." },
            "value": { "type": "number", "description": "Initial engineering value." },
            "signal": { "type": "object", "description": SIGNAL_HELP },
            "description": { "type": "string", "description": "Free text describing the point. Informational only." }
        },
        "required": ["name", "table", "address"]
    })
}

fn scenario_schema() -> Value {
    json!({
        "type": "object",
        "description": "Timeline of steps for one device.",
        "properties": {
            "name": { "type": "string", "description": "Label shown in list_devices and on the dashboard. Optional." },
            "repeat": { "type": "boolean", "description": "Start over after the last step. Default false." },
            "steps": {
                "type": "array",
                "description": "Steps in any order; each runs once when its time is reached.",
                "items": {
                    "type": "object",
                    "description": "at_s is seconds after the scenario starts. action is one of: \
set_point (point, value) | set_signal (point, signal) | clear_signal (point) | fault (fault) | clear_fault.",
                    "properties": {
                        "at_s": { "type": "number", "minimum": 0, "description": "Seconds after the scenario starts at which this step runs." },
                        "action": { "type": "string", "enum": ["set_point", "set_signal", "clear_signal", "fault", "clear_fault"], "description": "What the step does. set_point needs point and value; set_signal needs point and signal; clear_signal needs point; fault needs fault; clear_fault needs nothing else." },
                        "point": { "type": "string", "description": "Point name, for set_point, set_signal and clear_signal." },
                        "value": { "type": "number", "description": "Engineering value, for set_point." },
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
    let dev = json!({ "type": "string", "description": "Id of a running device, as returned by create_device, clone_device or list_devices." });
    let point = json!({ "type": "string", "description": "Name of an existing point on that device (see get_device or read_points)." });
    let table = json!({ "type": "string", "enum": ["coil", "discrete", "holding", "input"], "description": "Modbus table: coil and discrete hold bits, holding and input hold 16-bit registers." });
    json!([
        tool(
            "list_templates", "List device templates", "read", true,
            "List the built-in device templates (generic, energy_meter, vfd, temperature_controller, pump, tank) with every point each one provides: name, table, address, data type and unit. Call this before create_device to choose a template or to learn point names. Changes nothing.",
            json!({}), &[],
        ),
        tool(
            "create_device", "Create a simulated device", "change", false,
            "Start one simulated Modbus TCP device that real client software can connect to. Use clone_device instead when you need many copies of a device that already exists, and load_project to bring back a saved set. Without a port the device gets the next free port; give a port already used by this simulator together with a different unit_id to put several devices behind one port. Fails if the id is taken, the port is used by another program, or the device limit is reached. Returns the device id plus the host, port and unit id to connect to.",
            json!({
                "template": { "type": "string", "description": "Built-in template name (see list_templates). Omit for an empty device and supply points yourself." },
                "id": { "type": "string", "description": "Id for the new device: letters, digits, '-' and '_'. Omit to get <template>-N automatically." },
                "name": { "type": "string", "description": "Display name shown in list_devices and on the dashboard. Optional." },
                "port": { "type": "integer", "minimum": 1, "maximum": 65535, "description": "TCP port to listen on. Omit for the next free port." },
                "unit_id": { "type": "integer", "minimum": 0, "maximum": 255, "description": "Modbus unit id the device answers to. Default 1." },
                "strict": { "type": "boolean", "description": "true: answer 'illegal data address' for addresses no point covers. Default false: unmapped addresses read 0." },
                "points": { "type": "array", "items": point_schema(), "description": "Extra points, or replacements for template points with the same name." },
                "scenario": scenario_schema()
            }),
            &[],
        ),
        tool(
            "clone_device", "Clone a device many times", "change", false,
            "Create several copies of a running device, each on its own port, for example 50 identical meters for a load test. Copies start with the source's points, current values, signals and settings; later changes to the source do not reach them. Use create_device for a single new device. If a copy cannot be created the call stops there and the error says how many were made; those copies keep running. Returns the id, port and unit id of every copy.",
            json!({
                "source_id": { "type": "string", "description": "Id of the running device to copy." },
                "count": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "How many copies to create." },
                "start_port": { "type": "integer", "minimum": 1, "maximum": 65535, "description": "Port of the first copy; the others use the following ports. Omit for the next free ports." },
                "id_prefix": { "type": "string", "description": "Copies are named <prefix>-1, <prefix>-2, ... Default: the source id." }
            }),
            &["source_id", "count"],
        ),
        tool(
            "list_devices", "List running devices", "read", true,
            "List every running device with its id, host, port, unit id, active fault and scenario state, plus the dashboard address. Use it to find device ids or to check what is running; use get_device for one device's points and values. Changes nothing.",
            json!({}), &[],
        ),
        tool(
            "get_device", "Show one device in full", "read", true,
            "Return everything about one device: connection details, settings, active fault, scenario progress and every point with its address, data type and current value. Use read_points instead when you only need values. Changes nothing. Fails if the id is unknown.",
            json!({ "device_id": dev }), &["device_id"],
        ),
        tool(
            "remove_device", "Remove one device", "remove", false,
            "Stop one device and free its port. Its points, values, traffic log and running scenario are discarded and Modbus clients can no longer reach it. This cannot be undone unless the device was saved with save_project. Use remove_all_devices to clear everything. Fails if the id is unknown.",
            json!({ "device_id": dev }), &["device_id"],
        ),
        tool(
            "remove_all_devices", "Remove every device", "remove", true,
            "Stop every running device and free all their ports. All points, values, traffic logs and scenarios are discarded; saved project files are not touched. Use it to start from a clean state, and call save_project first if the current setup should be kept. Returns how many devices were removed (0 if none were running).",
            json!({}), &[],
        ),
        tool(
            "read_points", "Read point values", "read", true,
            "Read the current engineering values (after scaling) of a device's points, with their units. Use this to check what a device is reporting right now; use read_registers when you need the raw 16-bit words a Modbus client would receive. Changes nothing. Fails if the device or a named point does not exist.",
            json!({ "device_id": dev, "points": { "type": "array", "items": { "type": "string" }, "description": "Point names to read. Omit to read every point of the device." } }),
            &["device_id"],
        ),
        tool(
            "set_point", "Set a point to a fixed value", "change", true,
            "Set one point to a fixed engineering value (the simulator applies the point's scale and data type). Any signal running on that point is stopped, so the value stays until changed again. Use set_signal for a value that changes over time and write_registers to write raw words at an address. Returns the point with its new value. Fails if the device or point does not exist.",
            json!({ "device_id": dev, "point": point, "value": { "type": "number", "description": "New engineering value. For bool points use 0 or 1." } }),
            &["device_id", "point", "value"],
        ),
        tool(
            "set_signal", "Make a point change over time", "change", true,
            "Attach a signal to one point so its value changes by itself: sine, ramp, random, random walk, counter, square or constant. It replaces any signal already on that point and runs until replaced, stopped, or overwritten by set_point. Pass signal null to stop the signal and keep the current value. Use set_point for a fixed value and apply_scenario to change several things on a timeline. Returns the point with its current value.",
            json!({ "device_id": dev, "point": point, "signal": { "type": ["object", "null"], "description": SIGNAL_HELP } }),
            &["device_id", "point", "signal"],
        ),
        tool(
            "add_point", "Add a point to a device", "change", false,
            "Add one new named point (a coil, discrete input or register value) to a device that is already running, without restarting it or disturbing connected clients. Use it to extend a template device; to define points when a device is first created, pass them to create_device instead. The point takes its initial value, or starts its signal, immediately. Returns the new point with its current value. Fails if the device does not exist, the name is already used on that device, or the definition is invalid (for example a 32-bit type in a bit table).",
            json!({ "device_id": dev, "point": point_schema() }), &["device_id", "point"],
        ),
        tool(
            "remove_point", "Remove a point from a device", "remove", false,
            "Delete one named point from a running device, together with any signal on it. Only the definition is removed: the raw value last stored at its address stays readable by Modbus clients until overwritten (or is rejected if the device is strict and no other point covers the address). The device and its other points keep running. Use remove_device to remove the whole device. Fails if the device or point does not exist.",
            json!({ "device_id": dev, "point": point }), &["device_id", "point"],
        ),
        tool(
            "read_registers", "Read raw Modbus values", "read", true,
            "Read raw values straight from one Modbus table of a device, exactly as a client would receive them: 0 or 1 for coil and discrete, unscaled 16-bit words (0-65535) for holding and input. Use it to verify byte order, scaling or addresses; use read_points for engineering values by name. Addresses no point covers read 0, even on strict devices. Changes nothing and is not recorded in the traffic log. Returns the table, start address and list of values.",
            json!({
                "device_id": dev,
                "table": table,
                "address": { "type": "integer", "minimum": 0, "maximum": 65535, "description": "Zero-based address of the first value." },
                "quantity": { "type": "integer", "minimum": 1, "maximum": 125, "description": "How many consecutive values to read." }
            }),
            &["device_id", "table", "address", "quantity"],
        ),
        tool(
            "write_registers", "Write raw Modbus values", "change", true,
            "Write raw values into one Modbus table of a device, starting at an address: 0-65535 for holding and input registers, 0 or 1 for coils and discrete inputs. Unlike a Modbus client this can also write the read-only tables (discrete, input), which is how you fake sensor inputs at an address no point covers. Use set_point when the address belongs to a named point and you want scaling applied; a signal running on a point at that address will overwrite the value on its next update. Returns how many values were written.",
            json!({
                "device_id": dev,
                "table": table,
                "address": { "type": "integer", "minimum": 0, "maximum": 65535, "description": "Zero-based address of the first value." },
                "values": { "type": "array", "items": { "type": "integer" }, "minItems": 1, "maxItems": 125, "description": "Values for consecutive addresses, first value at 'address'." }
            }),
            &["device_id", "table", "address", "values"],
        ),
        tool(
            "apply_scenario", "Play a scenario on a device", "change", false,
            "Play a timeline of timed steps on one device, starting now: set values, start or stop signals, inject or clear faults at given seconds. Use it to rehearse a sequence such as a pump trip followed by recovery; use set_point, set_signal or inject_fault for a single immediate change. Replaces any scenario already running on that device. Values, signals and faults set by steps stay in place after the scenario ends. Returns the scenario state.",
            json!({ "device_id": dev, "scenario": scenario_schema() }),
            &["device_id", "scenario"],
        ),
        tool(
            "stop_scenario", "Stop a device's scenario", "change", true,
            "Stop the scenario running on one device so no further steps are played. Nothing is rolled back: values, signals and faults stay as the scenario left them (use clear_fault or set_point to reset them). Safe to call when no scenario is running. Fails if the device does not exist.",
            json!({ "device_id": dev }), &["device_id"],
        ),
        tool(
            "inject_fault", "Inject a communication fault", "change", true,
            "Make one device misbehave on the wire so you can test how client software copes: slow answers, no answer (timeout), Modbus exception replies, or dropped connections. The fault applies to every request the device receives, replaces any fault already active on it, and lasts until clear_fault is called or its optional duration_s runs out. Point values keep updating in the background. Use apply_scenario to schedule faults at set times.",
            json!({ "device_id": dev, "fault": { "type": "object", "description": FAULT_HELP } }),
            &["device_id", "fault"],
        ),
        tool(
            "clear_fault", "Clear a device's fault", "change", true,
            "Remove the active communication fault from one device so it answers Modbus requests normally again. Use it to end a fault started by inject_fault or by a scenario step before its duration runs out. It does not stop a running scenario, which may inject a fault again at a later step (use stop_scenario for that), and it does not change any values. Safe to call when no fault is active. Returns 'cleared': true if a fault was removed, false if there was none. Fails if the device does not exist.",
            json!({ "device_id": dev }), &["device_id"],
        ),
        tool(
            "get_traffic_log", "Show recent Modbus requests", "read", true,
            "Show the most recent Modbus requests a device received from client software, oldest first: function code, address, quantity and how it was answered, plus the total number of requests served. Use it to see what the software under test is polling or to confirm a fault took effect. The log keeps the last 500 requests per device and is lost when the device is removed. Changes nothing.",
            json!({ "device_id": dev, "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "How many of the latest requests to return. Default 50." } }),
            &["device_id"],
        ),
        tool(
            "save_project", "Save devices to a project", "remove", true,
            "Save every running device (points, current values, signals, ports, running scenarios) to a named project file on this machine so the same setup can be restored later with load_project. An existing project with the same name is overwritten without warning; call list_projects first to check. Running devices are not affected. Traffic logs and active faults are not saved. Fails if no devices are running. Returns the file path and device count.",
            json!({ "name": { "type": "string", "description": "Project name: letters, digits, '-' and '_', at most 64 characters." } }),
            &["name"],
        ),
        tool(
            "load_project", "Load a saved project", "remove", false,
            "Start the devices stored in a project saved earlier with save_project, on the ports recorded in it. By default they are added next to the devices already running, and the call fails part-way if an id or port is already in use (devices started before the failure keep running). With replace true, all running devices are removed first, which discards their unsaved state. Call list_projects to see the available names. Returns the devices that were started.",
            json!({
                "name": { "type": "string", "description": "Name of a saved project, exactly as shown by list_projects." },
                "replace": { "type": "boolean", "description": "true: remove all currently running devices before loading. Default false: keep them." }
            }),
            &["name"],
        ),
        tool(
            "list_projects", "List saved projects", "read", true,
            "List the names of the projects saved with save_project, sorted alphabetically, and the directory they are stored in. Use it to find a name for load_project or to check whether a name is taken before save_project overwrites it. Shows names only; load a project to see its devices. Changes nothing and returns an empty list when nothing has been saved.",
            json!({}), &[],
        ),
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
