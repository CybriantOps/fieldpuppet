# fieldpuppet

[![fieldpuppet MCP server – quality and maintenance score on Glama](https://glama.ai/mcp/servers/CybriantOps/fieldpuppet/badges/score.svg)](https://glama.ai/mcp/servers/CybriantOps/fieldpuppet)

A Modbus TCP device simulator that an AI assistant can drive.

Start one small program and it can impersonate hundreds of Modbus devices at once, each on its own port. Through its built-in [MCP](https://modelcontextprotocol.io) server you tell an assistant such as Claude what you need in plain language — *"set up three pumps and an energy meter, then make pump 2 stop answering for thirty seconds"* — and the assistant creates the devices, plays the scenario and reports what your software asked them for.

It is meant for testing SCADA systems, gateways, historians, HMIs and any other software that talks Modbus, without real hardware on the desk.

> **Simulation only.** This program never connects to real equipment. It only answers Modbus requests that are sent to it.

Developed and maintained by the team behind [PLCGateway](https://plcgateway.com).

## What it does

- **Many devices in one process.** Each device gets its own TCP port automatically, or several devices share one port under different unit ids, like stations behind a serial gateway.
- **Ready-made templates.** Energy meter, variable frequency drive, temperature controller, pump, tank — or define your own points one by one.
- **Values that move.** Sine, ramp, random, random walk, counter and square-wave signals, in engineering units with scaling, for 16-bit, 32-bit and float registers.
- **Scenarios.** A timeline of steps per device: change a value at 10 s, start a ramp at 30 s, break communication at 60 s.
- **Communication faults.** Slow answers, no answer at all, Modbus exception replies, dropped connections — per device, with an optional time limit.
- **Traffic log.** See exactly which registers your software polled and what it got back.
- **Live dashboard.** A web page that shows every device, its points and current values, running scenarios and faults, and a plain-language feed of everything the assistant changed.
- **Works without AI too.** Describe devices in a YAML file and run them from the command line.
- **Small and quick.** One executable of about 2 MB with nothing to install. On a laptop-class machine, 1,000 devices with ten changing values each used about 11 MB of memory and under 1 % of one CPU core.

## Install

Download the file for your system from the [Releases](../../releases) page and unpack it. There is nothing else to install.

| System | File |
|---|---|
| Windows (64-bit) | `fieldpuppet-windows-x86_64.zip` |
| macOS (Apple silicon) | `fieldpuppet-macos-aarch64.tar.gz` |
| macOS (Intel) | `fieldpuppet-macos-x86_64.tar.gz` |
| Linux (64-bit) | `fieldpuppet-linux-x86_64.tar.gz` |
| Linux (ARM 64-bit) | `fieldpuppet-linux-aarch64.tar.gz` |

Remember where you put the program; you need its full path in the next step.

**One file for Claude Desktop:** every release also has an `.mcpb` bundle per system, for example `fieldpuppet-windows-x86_64.mcpb`. Claude Desktop can install such a bundle from its Extensions settings, with no config file to edit.

Building from source instead needs [Rust](https://rustup.rs): `cargo install --path .`

## Connect it to Claude Desktop

1. Open Claude Desktop. From the **Claude** menu in the menu bar choose **Settings…**, open the **Developer** tab and click **Edit Config**. This opens a file called `claude_desktop_config.json`.
2. Add the simulator to it, using the full path to the program you downloaded.

   Windows:

   ```json
   {
     "mcpServers": {
       "fieldpuppet": {
         "command": "C:\\Tools\\fieldpuppet.exe",
         "args": ["mcp"]
       }
     }
   }
   ```

   macOS and Linux:

   ```json
   {
     "mcpServers": {
       "fieldpuppet": {
         "command": "/Users/you/tools/fieldpuppet",
         "args": ["mcp"]
       }
     }
   }
   ```

   If the file already lists other servers, add `"fieldpuppet": { ... }` next to them inside `mcpServers`.
3. Save the file, then quit Claude Desktop completely and open it again.
4. Start a new chat and try: *"List the Modbus device templates you can simulate."*

Claude Desktop starts the simulator when it opens and stops it when it closes, so the simulated devices exist only while Claude Desktop is running.

On macOS the first start may be blocked because the file was downloaded from the internet. Allow it once under **System Settings → Privacy & Security**.

### Other MCP clients

The simulator speaks MCP over standard input and output, so any client that can start a local server works. For Claude Code:

```
claude mcp add fieldpuppet -- /full/path/to/fieldpuppet mcp
```

## Dashboard

While the simulator runs, open **http://127.0.0.1:5080** in a browser. The page updates by itself and shows:

- every running device with its address, and whether a scenario or fault is active on it;
- the points of the selected device: current value, address, data type and how the value is changing;
- the steps of its scenario and which of them have already run;
- the most recent Modbus requests it received and how they were answered;
- an activity feed listing, in order, everything that was changed: devices created, values set, scenarios started, faults injected and cleared.

The dashboard only displays; it cannot change anything. If port 5080 is taken the next free port is used, and asking the assistant *"where is the dashboard?"* gives the address. Start with `--no-ui` to switch it off.

## Things to ask

- *"Create an energy meter and a pump. Which addresses do I connect to?"*
- *"Set up 50 energy meters starting at port 6000."*
- *"Make the tank level rise from 20 % to 95 % over two minutes, then switch the high level alarm on."*
- *"Pump 2 should answer with a 'device busy' exception for the next 30 seconds."*
- *"Which registers has my SCADA been reading from the drive in the last minute?"*
- *"Here is the register list from a device manual. Build a simulated device that matches it."*
- *"Save this setup as `water-station` so I can load it again tomorrow."*

Then point your own software at `127.0.0.1` and the port the assistant reports.

## Try it with PLCGateway

fieldpuppet works with any Modbus TCP client. If you use [PLCGateway](https://plcgateway.com), add a Modbus TCP device with host `127.0.0.1`, the port the assistant reported and unit id `1`, and the simulated values show up like those of a real device. A 30-day trial is available on the website.

## Use it without AI

Describe the devices in a YAML file and run it:

```
fieldpuppet run examples/water-station.yaml
```

```yaml
devices:
  - id: pump-1
    template: pump
    port: 5020

  - id: dosing-skid
    port: 5021
    points:
      - name: chlorine_ppm
        table: input
        address: 0
        scale: 0.01
        unit: ppm
        value: 0.8
        signal: { type: random_walk, min: 0.5, max: 1.2, step: 0.02 }
```

[`examples/water-station.yaml`](examples/water-station.yaml) shows scenarios, shared ports and strict mode. `fieldpuppet templates` lists the built-in templates and their points.

## Reference

### Addresses

Addresses are zero-based protocol addresses: holding register 40001 is address `0`. Function codes 1, 2, 3, 4, 5, 6, 15 and 16 are supported.

| Table | Read with | Written with |
|---|---|---|
| `coil` | 01 | 05, 15 |
| `discrete` | 02 | – |
| `holding` | 03 | 06, 16 |
| `input` | 04 | – |

### Points

| Field | Meaning |
|---|---|
| `name` | Name used in tools and scenarios |
| `table` | `coil`, `discrete`, `holding` or `input` |
| `address` | Zero-based address |
| `data_type` | `u16` (default), `i16`, `u32`, `i32`, `f32`. 32-bit types take two registers, high word first. |
| `word_swap` | `true` to send the low word first |
| `scale` | Engineering value = raw value × scale. Default `1`. |
| `unit`, `description` | Free text |
| `value` | Starting value, in engineering units |
| `signal` | Optional, see below |

Without `strict: true` on the device, addresses that no point covers read as zero and accept writes. With it, they answer *illegal data address*.

### Signals

| Type | Fields | Behaviour |
|---|---|---|
| `constant` | `value` | Holds one value |
| `ramp` | `from`, `to`, `period_s`, `repeat` | Straight line; starts over unless `repeat: false` |
| `sine` | `min`, `max`, `period_s` | Sine wave |
| `random` | `min`, `max` | New random value every update |
| `random_walk` | `min`, `max`, `step` | Drifts by at most `step` per update |
| `counter` | `per_second`, `rollover` | Counts up; optional wrap to zero |
| `square` | `low`, `high`, `period_s` | Alternates, half a period each |

Signals are evaluated every 200 ms (`--tick-ms`). A value written by a Modbus client stays until a signal on the same point overwrites it.

### Faults

| Type | Fields | What the client sees |
|---|---|---|
| `delay` | `ms` | Correct answers, late |
| `no_response` | – | A timeout |
| `exception` | `code` | A Modbus exception (4 = device failure, 6 = busy, …) |
| `disconnect` | – | The connection closes on every request |

Add `duration_s` to any of them to have it clear itself.

### Scenario steps

Each step has `at_s` (seconds after the scenario starts) and an `action`:

| Action | Fields |
|---|---|
| `set_point` | `point`, `value` |
| `set_signal` | `point`, `signal` |
| `clear_signal` | `point` |
| `fault` | `fault` |
| `clear_fault` | – |

Set `repeat: true` on the scenario to loop it.

### MCP tools

`list_templates`, `create_device`, `clone_device`, `list_devices`, `get_device`, `remove_device`, `remove_all_devices`, `read_points`, `set_point`, `set_signal`, `add_point`, `remove_point`, `read_registers`, `write_registers`, `apply_scenario`, `stop_scenario`, `inject_fault`, `clear_fault`, `get_traffic_log`, `save_project`, `load_project`, `list_projects`.

Both the current MCP revision (2026-07-28) and the earlier handshake-based revisions are supported.

### Command-line options

| Option | Default | Meaning |
|---|---|---|
| `--bind` | `127.0.0.1` | Address to listen on. `0.0.0.0` accepts connections from other machines. |
| `--base-port` | `5020` | First port given to devices that do not ask for one |
| `--max-devices` | `1000` | Upper limit on running devices |
| `--tick-ms` | `200` | How often signals update |
| `--data-dir` | `~/.fieldpuppet/projects` | Where `save_project` keeps its files |
| `--ui-port` | `5080` | Port of the web dashboard |
| `--no-ui` | – | Do not start the dashboard |
| `--project` (`mcp` only) | – | Project file to load at startup |

Options go after the command, for example `"args": ["mcp", "--base-port", "6000"]`.

## Safety notes

- By default the simulator listens on this computer only. Use `--bind 0.0.0.0` only on a network where nobody could mistake a simulated device for a real one.
- Port 502, the standard Modbus port, needs administrator rights on Linux and macOS. The default range starting at 5020 does not.
- Modbus has no authentication. Anyone who can reach a simulated device can read and write it.
- The dashboard has no login either. It listens on the same address as the devices and is read-only.

## Limits in this version

- Modbus TCP only; no serial RTU or ASCII.
- Devices do not model process behaviour on their own. A pump does not stop because its run coil was cleared; a scenario or the assistant has to change the values.
- State lives in memory. Save a project to keep it.

## Development

```
cargo test                                            # unit tests
cargo build --release
python3 tests/e2e.py target/release/fieldpuppet     # end-to-end: MCP calls plus real Modbus requests
```

## License

Licensed under either of the Apache License, Version 2.0 or the MIT license, at your option.
