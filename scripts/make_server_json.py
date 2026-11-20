"""Write server.json for the MCP Registry from the .mcpb files of one release.

Usage: python3 scripts/make_server_json.py <tag> <bundle.mcpb> [<bundle.mcpb> ...]
"""
import hashlib
import json
import os
import sys

REPO = "https://github.com/CybriantOps/fieldpuppet"


def main():
    tag, bundles = sys.argv[1], sorted(sys.argv[2:])
    packages = []
    for path in bundles:
        with open(path, "rb") as f:
            digest = hashlib.sha256(f.read()).hexdigest()
        packages.append({
            "registryType": "mcpb",
            "identifier": REPO + "/releases/download/" + tag + "/" + os.path.basename(path),
            "fileSha256": digest,
            "transport": {"type": "stdio"},
        })
    server = {
        "$schema": "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json",
        "name": "io.github.CybriantOps/fieldpuppet",
        "title": "fieldpuppet",
        "description": "Multi-device Modbus TCP simulator: create devices, run scenarios, inject faults.",
        "version": tag.lstrip("v"),
        "repository": {"url": REPO, "source": "github"},
        "packages": packages,
    }
    with open("server.json", "w") as f:
        json.dump(server, f, indent=2)
        f.write("\n")
    print("server.json written with", len(packages), "package(s)")


if __name__ == "__main__":
    main()
