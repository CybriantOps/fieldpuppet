"""End-to-end test: drives the MCP server over stdio and sends real Modbus TCP requests.

Usage: python3 tests/e2e.py path/to/fieldpuppet
"""
import json, socket, struct, subprocess, sys, time, tempfile, os

BIN = sys.argv[1]
data_dir = tempfile.mkdtemp()
proc = subprocess.Popen([BIN, "mcp", "--base-port", "15020", "--data-dir", data_dir],
                        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
_id = 0
def rpc(method, params=None):
    global _id; _id += 1
    proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": _id, "method": method, "params": params or {}}) + "\n"); proc.stdin.flush()
    r = json.loads(proc.stdout.readline()); assert r["id"] == _id, r
    return r
def call(_tool, **args):
    r = rpc("tools/call", {"name": _tool, "arguments": args})["result"]
    text = r["content"][0]["text"]
    return (json.loads(text) if not r["isError"] else text), r["isError"]
def ok(_tool, **args):
    v, err = call(_tool, **args); assert not err, (_tool, v); return v
def bad(_tool, **args):
    v, err = call(_tool, **args); assert err, (_tool, v); return v

class MB:
    def __init__(self, port, timeout=1.0):
        self.s = socket.create_connection(("127.0.0.1", port), timeout=timeout); self.tid = 0
    def req(self, unit, pdu):
        self.tid += 1
        self.s.sendall(struct.pack(">HHHB", self.tid, 0, len(pdu) + 1, unit) + pdu)
        h = self._read(7); tid, _, ln, u = struct.unpack(">HHHB", h); assert tid == self.tid and u == unit
        return self._read(ln - 1)
    def _read(self, n):
        b = b""
        while len(b) < n:
            c = self.s.recv(n - len(b))
            if not c: raise ConnectionError("closed")
            b += c
        return b
    def regs(self, unit, fc, addr, qty):
        r = self.req(unit, struct.pack(">BHH", fc, addr, qty))
        if r[0] & 0x80: return ("exc", r[1])
        return list(struct.unpack(">" + "H" * qty, r[2:]))

checks = []
def check(label, cond, detail=""):
    checks.append((label, bool(cond))); print(("PASS " if cond else "FAIL ") + label + (" :: " + str(detail) if not cond else ""))

init = rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}})
check("initialize echoes protocol version", init["result"]["protocolVersion"] == "2025-06-18", init)
proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n"); proc.stdin.flush()
tools = rpc("tools/list")["result"]["tools"]
check("tools/list returns 22 tools", len(tools) == 22, len(tools))
check("unknown method -> -32601", rpc("nope").get("error", {}).get("code") == -32601)

# --- modern (2026-07-28, no handshake) requests on the same pipe
M = {"_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientCapabilities": {}}}
dsc = rpc("server/discover", M)["result"]
check("server/discover result shape", dsc["resultType"] == "complete" and "2026-07-28" in dsc["supportedVersions"] and "tools" in dsc["capabilities"] and dsc["_meta"]["io.modelcontextprotocol/serverInfo"]["name"] and dsc["cacheScope"] == "public", dsc)
ml = rpc("tools/list", M)["result"]
check("modern tools/list carries resultType, ttlMs, cacheScope", ml["resultType"] == "complete" and ml["ttlMs"] > 0 and ml["cacheScope"] == "public" and len(ml["tools"]) == 22)
mc = rpc("tools/call", {"name": "list_devices", "arguments": {}, **M})["result"]
check("modern tools/call carries resultType", mc["resultType"] == "complete" and mc["isError"] is False)
uv = rpc("tools/list", {"_meta": {"io.modelcontextprotocol/protocolVersion": "2030-01-01"}})["error"]
check("unknown version -> -32022 with supported list", uv["code"] == -32022 and uv["data"]["requested"] == "2030-01-01" and "2026-07-28" in uv["data"]["supported"], uv)
check("ping is gone in the modern era", rpc("ping", M).get("error", {}).get("code") == -32601)
check("legacy ping still answered", rpc("ping").get("result") == {})
check("legacy tools/list has no modern-only fields", "resultType" not in rpc("tools/list")["result"])

# --- devices and ports
m = ok("create_device", template="energy_meter")
check("first device gets base port", m["port"] == 15020 and m["unit_id"] == 1, m)
v = ok("create_device", template="vfd", id="drive-a")
check("second device gets next port", v["port"] == 15021, v)
g = ok("create_device", template="pump", port=15021, unit_id=2, id="pump-b")
check("explicit port + other unit id shares the port", g["port"] == 15021 and g["unit_id"] == 2, g)
check("duplicate port+unit rejected", "already has a device" in bad("create_device", template="pump", port=15021, unit_id=2))
check("duplicate id rejected", "already exists" in bad("create_device", id="drive-a"))
check("unknown template rejected", "unknown template" in bad("create_device", template="nope"))

# occupy a port externally: auto assignment must skip it
blocker = socket.socket(); blocker.bind(("127.0.0.1", 15022)); blocker.listen(1)
t = ok("create_device", template="tank")
check("auto port skips a port taken by another program", t["port"] == 15023, t)
check("explicit busy port gives a clear error", "cannot listen" in bad("create_device", port=15022))
blocker.close()

# --- modbus reads
c = MB(15020)
r = c.regs(1, 4, 0, 2)
volt = struct.unpack(">f", struct.pack(">HH", *r))[0]
check("energy meter voltage_l1 reads ~230 V as f32", 224 < volt < 236, volt)
check("unit 255 routed to the only device on the port", c.regs(255, 4, 0, 2) != ("exc", 11))
d = MB(15021)
check("vfd speed_setpoint raw = 350 (35.0 Hz, scale 0.1)", d.regs(1, 3, 1, 1) == [350])
check("pump flow readable via unit 2 on shared port", isinstance(d.regs(2, 4, 0, 1), list))
check("unknown unit on shared port -> exception 0x0B", d.regs(9, 3, 0, 1) == ("exc", 11))
check("quantity 0 -> exception 3", d.regs(1, 3, 0, 0) == ("exc", 3))
check("quantity 126 -> exception 3", d.regs(1, 3, 0, 126) == ("exc", 3))
check("unsupported function -> exception 1", d.req(1, bytes([43, 14, 1, 0]))[:2] == bytes([43 | 0x80, 1]))

# --- client writes are visible through MCP
d.req(1, struct.pack(">BHH", 6, 1, 425))
pts = ok("read_points", device_id="drive-a", points=["speed_setpoint"])
check("FC6 write visible as 42.5 Hz via read_points", abs(pts[0]["value"] - 42.5) < 1e-6, pts)
d.req(1, struct.pack(">BHHBHH", 16, 2, 2, 4, 77, 88))
check("FC16 write stored", d.regs(1, 3, 2, 2) == [77, 88])
d.req(1, struct.pack(">BHH", 5, 1, 0xFF00))
bits = d.req(1, struct.pack(">BHH", 1, 0, 2))
check("FC5 write + FC1 read coils", bits == bytes([1, 1, 0b11]), bits)
d.req(1, struct.pack(">BHHBB", 15, 0, 2, 1, 0b00))
check("FC15 write coils", d.req(1, struct.pack(">BHH", 1, 0, 2)) == bytes([1, 1, 0]))

# --- set_point / set_signal
ok("set_point", device_id="drive-a", point="output_frequency", value=12.3)
check("set_point writes scaled raw value", d.regs(1, 4, 1, 1) == [123])
time.sleep(0.6)
check("set_point stops the signal", d.regs(1, 4, 1, 1) == [123])
ok("set_signal", device_id="drive-a", point="output_frequency", signal={"type": "ramp", "from": 0, "to": 50, "period_s": 5, "repeat": False})
time.sleep(1.2); a = d.regs(1, 4, 1, 1)[0]; time.sleep(1.0); b = d.regs(1, 4, 1, 1)[0]
check("ramp signal rises over time", 60 < a < 200 and b > a + 50, (a, b))
check("bad signal rejected", "period_s" in bad("set_signal", device_id="drive-a", point="output_frequency", signal={"type": "sine", "min": 0, "max": 1, "period_s": 0}))
check("unknown point lists valid names", "Points:" in bad("set_point", device_id="drive-a", point="x", value=1))
check("unknown device lists ids", "Devices:" in bad("get_device", device_id="zzz"))

# --- strict mode
s = ok("create_device", id="strict-1", strict=True, points=[{"name": "a", "table": "holding", "address": 10, "data_type": "u32", "value": 70000}])
e = MB(s["port"])
check("strict: mapped u32 reads 70000", e.regs(1, 3, 10, 2) == [1, 4464])
check("strict: unmapped address -> exception 2", e.regs(1, 3, 12, 1) == ("exc", 2))
ok("add_point", device_id="strict-1", point={"name": "b", "table": "holding", "address": 12, "data_type": "i16", "scale": 0.5, "value": -3})
check("add_point: negative scaled i16", e.regs(1, 3, 12, 1) == [65530])

# --- faults
ok("inject_fault", device_id="drive-a", fault={"type": "exception", "code": 6})
check("exception fault", d.regs(1, 3, 0, 1) == ("exc", 6))
check("fault is per device (unit 2 on same port still fine)", isinstance(d.regs(2, 4, 0, 1), list))
ok("inject_fault", device_id="drive-a", fault={"type": "delay", "ms": 400})
t0 = time.time(); r = d.regs(1, 3, 1, 1); dt = time.time() - t0
check("delay fault delays ~400 ms", r == [425] and 0.35 < dt < 0.9, dt)
ok("inject_fault", device_id="drive-a", fault={"type": "no_response", "duration_s": 1.0})
f = MB(15021, timeout=0.4)
try: f.regs(1, 3, 0, 1); timed_out = False
except socket.timeout: timed_out = True
check("no_response fault -> client timeout", timed_out)
time.sleep(1.1)
check("fault with duration clears itself", isinstance(MB(15021).regs(1, 3, 0, 1), list))
ok("inject_fault", device_id="drive-a", fault={"type": "disconnect"})
try: MB(15021).regs(1, 3, 0, 1); closed = False
except ConnectionError: closed = True
check("disconnect fault closes the connection", closed)
check("clear_fault", ok("clear_fault", device_id="drive-a")["cleared"] is True and isinstance(MB(15021).regs(1, 3, 0, 1), list))
check("bad exception code rejected", "exception code" in bad("inject_fault", device_id="drive-a", fault={"type": "exception", "code": 0}))

# --- scenario
ok("apply_scenario", device_id="tank-1", scenario={"name": "overflow", "steps": [
    {"at_s": 0, "action": "set_point", "point": "level", "value": 10},
    {"at_s": 0.6, "action": "set_point", "point": "level", "value": 95},
    {"at_s": 0.6, "action": "set_point", "point": "high_level", "value": 1},
    {"at_s": 1.2, "action": "fault", "fault": {"type": "exception", "code": 4}},
    {"at_s": 1.8, "action": "clear_fault"}]})
tk = MB(t["port"])
time.sleep(0.3); check("scenario step at t=0", tk.regs(1, 4, 0, 1) == [100])
time.sleep(0.6); check("scenario step at t=0.6 (value + discrete)", tk.regs(1, 4, 0, 1) == [950] and tk.req(1, struct.pack(">BHH", 2, 0, 1)) == bytes([2, 1, 1]))
time.sleep(0.6); check("scenario fault step", tk.regs(1, 4, 0, 1) == ("exc", 4))
time.sleep(0.7); check("scenario clear_fault step", tk.regs(1, 4, 0, 1) == [950])
st = ok("get_device", device_id="tank-1")["scenario"]
check("scenario reports finished", st["state"] == "finished", st)
check("scenario with unknown point rejected", "no point" in bad("apply_scenario", device_id="tank-1", scenario={"steps": [{"at_s": 0, "action": "set_point", "point": "nope", "value": 1}]}))

# --- traffic log
log = ok("get_traffic_log", device_id="tank-1", limit=10)
check("traffic log records requests and results", log["requests_served"] >= 5 and any(e["result"] == "exception 4" or "fault" in e["result"] for e in log["entries"]), log["entries"][-3:])

# --- clone
cl = ok("clone_device", source_id="energy-meter-1", count=20, start_port=16000, id_prefix="meter")
check("clone 20 devices on consecutive ports", cl["created"] == 20 and cl["devices"][19]["port"] == 16019, cl["created"])
rr = MB(16019).regs(1, 4, 14, 2)
check("clone serves data (frequency ~50 Hz)", 49.9 < struct.unpack(">f", struct.pack(">HH", *rr))[0] < 50.1)

# --- save / load
n_before = ok("list_devices")["count"]
sv = ok("save_project", name="demo")
check("save_project writes a file", os.path.exists(sv["file"]) and sv["devices"] == n_before, sv)
check("bad project name rejected", "project name" in bad("save_project", name="../x"))
ld = ok("load_project", name="demo", replace=True)
check("load_project(replace) restores every device", len(ld["devices"]) == n_before and ok("list_devices")["count"] == n_before)
check("restored device keeps port, unit and written value", MB(15021).regs(1, 3, 2, 2) == [77, 88] and isinstance(MB(15021).regs(2, 4, 0, 1), list))
check("list_projects", ok("list_projects")["projects"] == ["demo"])

# --- removal frees the port
ok("remove_device", device_id="strict-1")
try: socket.create_connection(("127.0.0.1", s["port"]), timeout=0.5); refused = False
except OSError: refused = True
check("remove_device closes the listener", refused)
check("remove_all_devices", ok("remove_all_devices")["removed"] == n_before - 1 and ok("list_devices")["count"] == 0)

proc.stdin.close(); rc = proc.wait(timeout=5)
check("process exits cleanly when stdin closes", rc == 0, rc)
failed = [l for l, c in checks if not c]
print(f"\n{len(checks) - len(failed)}/{len(checks)} passed"); sys.exit(1 if failed else 0)
