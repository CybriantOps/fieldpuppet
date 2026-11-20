"""Pack one built fieldpuppet program into an MCPB bundle (a zip with a manifest.json).

Usage: python3 scripts/make_mcpb.py <program-file> <platform-name> <version>
  platform-name is the release name, for example windows-x86_64 or macos-aarch64.
Writes fieldpuppet-<platform-name>.mcpb into the current folder.
"""
import json
import os
import sys
import zipfile

TOOLS = [
    ("list_templates", "List the built-in device templates"),
    ("create_device", "Start a simulated Modbus TCP device"),
    ("clone_device", "Create several copies of a device, each on its own port"),
    ("list_devices", "List running devices"),
    ("get_device", "Show one device with all its points and values"),
    ("remove_device", "Stop one device"),
    ("remove_all_devices", "Stop every device"),
    ("read_points", "Read current values of a device's points"),
    ("set_point", "Set a point to a fixed value"),
    ("set_signal", "Make a point change over time"),
    ("add_point", "Add a point to a running device"),
    ("remove_point", "Remove a point from a running device"),
    ("read_registers", "Read raw values from a Modbus table"),
    ("write_registers", "Write raw values into a Modbus table"),
    ("apply_scenario", "Play a timeline of steps on a device"),
    ("stop_scenario", "Stop the scenario on a device"),
    ("inject_fault", "Make a device misbehave on the wire"),
    ("clear_fault", "Return a device to normal communication"),
    ("get_traffic_log", "Show the most recent Modbus requests a device received"),
    ("save_project", "Save all running devices to a project file"),
    ("load_project", "Start the devices stored in a saved project"),
    ("list_projects", "List saved projects"),
]

PLATFORMS = {"windows": "win32", "macos": "darwin", "linux": "linux"}


def main():
    program, platform_name, version = sys.argv[1], sys.argv[2], sys.argv[3].lstrip("v")
    system = platform_name.split("-")[0]
    inside = "server/fieldpuppet" + (".exe" if system == "windows" else "")
    manifest = {
        "manifest_version": "0.3",
        "name": "fieldpuppet",
        "display_name": "fieldpuppet",
        "version": version,
        "description": "Multi-device Modbus TCP simulator that an AI assistant can drive.",
        "long_description": (
            "Simulates many Modbus TCP devices at once for testing SCADA systems, gateways and other "
            "Modbus software without hardware. Create devices from templates, make values change over "
            "time, play scenarios and inject communication faults. A read-only dashboard on "
            "http://127.0.0.1:5080 shows what is running. Simulation only: it never connects to real equipment."
        ),
        "author": {"name": "PLCGateway", "url": "https://plcgateway.com"},
        "repository": {"type": "git", "url": "https://github.com/CybriantOps/fieldpuppet"},
        "homepage": "https://github.com/CybriantOps/fieldpuppet",
        "documentation": "https://github.com/CybriantOps/fieldpuppet#readme",
        "support": "https://github.com/CybriantOps/fieldpuppet/issues",
        "license": "MIT OR Apache-2.0",
        "keywords": ["modbus", "simulator", "scada", "industrial", "testing"],
        "server": {
            "type": "binary",
            "entry_point": inside,
            # The host replaces the placeholder with the folder the bundle was unpacked into.
            "mcp_config": {"command": "$" + "{__dirname}/" + inside, "args": ["mcp"]},
        },
        "tools": [{"name": n, "description": d} for n, d in TOOLS],
        "compatibility": {"platforms": [PLATFORMS[system]]},
    }
    out = "fieldpuppet-" + platform_name + ".mcpb"
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as bundle:
        bundle.writestr("manifest.json", json.dumps(manifest, indent=2) + "\n")
        # Store the program as executable; a zip made on Windows would otherwise lose that flag.
        info = zipfile.ZipInfo(inside)
        info.external_attr = (0o100755 << 16)
        info.compress_type = zipfile.ZIP_DEFLATED
        with open(program, "rb") as f:
            bundle.writestr(info, f.read())
    print(out, os.path.getsize(out), "bytes")


if __name__ == "__main__":
    main()
