//! Simulator state: the set of running devices, their data tables, signals, scenarios and faults.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::model::*;
use crate::{net, templates};

const LOG_CAPACITY: usize = 500;
const EVENT_CAPACITY: usize = 300;
const AUTO_PORT_ATTEMPTS: u32 = 2000;

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: IpAddr,
    pub base_port: u16,
    pub max_devices: usize,
    pub tick_ms: u64,
    pub data_dir: PathBuf,
    /// Address of the web dashboard once it is listening.
    pub ui_url: Option<String>,
}

/// Small xorshift generator: good enough for noise, no dependency needed.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    pub fn next_f64(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

pub struct PointState {
    pub def: Point,
    /// Unrounded engineering value, so slow counters and walks do not stall on integer registers.
    pub cur: f64,
    pub signal_t0: f64,
}

pub struct RunningScenario {
    pub scenario: Scenario,
    pub t0: f64,
    pub next: usize,
    pub loops: u64,
    pub finished: bool,
}

pub struct ActiveFault {
    pub fault: Fault,
    pub until: Option<f64>,
}

/// One line of the activity feed: something the simulator did, in plain words.
#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub unix_ms: u64,
    pub device: Option<String>,
    pub text: String,
}

fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn describe_signal(s: &Signal) -> String {
    match s {
        Signal::Constant { value } => format!("constant {}", value),
        Signal::Ramp { from, to, period_s, repeat } => {
            format!("ramp {} to {} over {} s{}", from, to, period_s, if *repeat { ", repeating" } else { "" })
        }
        Signal::Sine { min, max, period_s } => format!("sine {} to {}, period {} s", min, max, period_s),
        Signal::Random { min, max } => format!("random {} to {}", min, max),
        Signal::RandomWalk { min, max, step } => format!("random walk {} to {}, step {}", min, max, step),
        Signal::Counter { per_second, .. } => format!("counter +{} per second", per_second),
        Signal::Square { low, high, period_s } => format!("square {} / {}, period {} s", low, high, period_s),
    }
}

fn describe_fault(f: &Fault) -> String {
    let kind = match f.kind {
        FaultKind::Delay { ms } => format!("answers delayed by {} ms", ms),
        FaultKind::NoResponse => "no response".to_string(),
        FaultKind::Exception { code } => format!("exception code {}", code),
        FaultKind::Disconnect => "connection dropped on every request".to_string(),
    };
    match f.duration_s {
        Some(d) => format!("{} for {} s", kind, d),
        None => kind,
    }
}

pub fn describe_action(a: &Action) -> String {
    match a {
        Action::SetPoint { point, value } => format!("set {} = {}", point, value),
        Action::SetSignal { point, signal } => format!("{}: {}", point, describe_signal(signal)),
        Action::ClearSignal { point } => format!("stop signal on {}", point),
        Action::Fault { fault } => format!("fault: {}", describe_fault(fault)),
        Action::ClearFault => "clear fault".to_string(),
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct LogEntry {
    pub unix_ms: u64,
    pub peer: String,
    pub function: u8,
    pub address: u16,
    pub quantity: u16,
    pub result: String,
}

pub struct Device {
    pub id: String,
    pub name: String,
    pub template: Option<String>,
    pub port: u16,
    pub unit_id: u8,
    pub strict: bool,
    pub points: Vec<PointState>,
    pub holding: HashMap<u16, u16>,
    pub input: HashMap<u16, u16>,
    pub coils: HashMap<u16, bool>,
    pub discrete: HashMap<u16, bool>,
    pub fault: Option<ActiveFault>,
    pub scenario: Option<RunningScenario>,
    pub log: VecDeque<LogEntry>,
    pub requests: u64,
    /// Things that happened on their own (scenario steps, expiring faults), waiting for the activity feed.
    pending: Vec<String>,
    rng: Rng,
}

impl Device {
    fn new(id: String, spec: &DeviceSpec, port: u16, points: Vec<Point>, now: f64) -> Self {
        let seed = id.bytes().fold(0x9e37_79b9_7f4a_7c15u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
        let mut d = Device {
            name: spec.name.clone().unwrap_or_else(|| id.clone()),
            id,
            template: spec.template.clone(),
            port,
            unit_id: spec.unit_id,
            strict: spec.strict,
            points: Vec::new(),
            holding: HashMap::new(),
            input: HashMap::new(),
            coils: HashMap::new(),
            discrete: HashMap::new(),
            fault: None,
            scenario: None,
            log: VecDeque::new(),
            requests: 0,
            pending: Vec::new(),
            rng: Rng::new(seed),
        };
        for def in points {
            let cur = def.value;
            d.points.push(PointState { def, cur, signal_t0: now });
            let idx = d.points.len() - 1;
            d.store(idx, cur);
        }
        d
    }

    // ---- raw table access -------------------------------------------------------------

    pub fn bit(&self, table: Table, addr: u16) -> bool {
        let t = if table == Table::Coil { &self.coils } else { &self.discrete };
        t.get(&addr).copied().unwrap_or(false)
    }

    pub fn word(&self, table: Table, addr: u16) -> u16 {
        let t = if table == Table::Holding { &self.holding } else { &self.input };
        t.get(&addr).copied().unwrap_or(0)
    }

    pub fn set_bit(&mut self, table: Table, addr: u16, v: bool) {
        let t = if table == Table::Coil { &mut self.coils } else { &mut self.discrete };
        t.insert(addr, v);
    }

    pub fn set_word(&mut self, table: Table, addr: u16, v: u16) {
        let t = if table == Table::Holding { &mut self.holding } else { &mut self.input };
        t.insert(addr, v);
    }

    /// True when some point covers this address (used by strict mode).
    pub fn mapped(&self, table: Table, addr: u16) -> bool {
        self.points.iter().any(|p| {
            p.def.table == table
                && addr >= p.def.address
                && (addr as u32) < p.def.address as u32 + p.def.data_type.words() as u32
        })
    }

    // ---- points -----------------------------------------------------------------------

    fn index(&self, name: &str) -> Result<usize> {
        self.points.iter().position(|p| p.def.name == name).ok_or_else(|| {
            let names: Vec<&str> = self.points.iter().map(|p| p.def.name.as_str()).collect();
            anyhow!("device '{}' has no point '{}'. Points: {}", self.id, name, names.join(", "))
        })
    }

    fn store(&mut self, idx: usize, value: f64) {
        let l = self.points[idx].def.layout();
        match encode(l, value) {
            Raw::Bit(b) => self.set_bit(l.table, l.address, b),
            Raw::One(w) => self.set_word(l.table, l.address, w),
            Raw::Two(a, b) => {
                self.set_word(l.table, l.address, a);
                self.set_word(l.table, l.address + 1, b);
            }
        }
    }

    /// Current engineering value as a Modbus client would see it.
    pub fn load(&self, idx: usize) -> f64 {
        let l = self.points[idx].def.layout();
        let raw = if l.table.is_bit() {
            Raw::Bit(self.bit(l.table, l.address))
        } else if l.data_type.words() == 2 {
            Raw::Two(self.word(l.table, l.address), self.word(l.table, l.address + 1))
        } else {
            Raw::One(self.word(l.table, l.address))
        };
        decode(l, raw)
    }

    pub fn set_point(&mut self, name: &str, value: f64) -> Result<()> {
        if !value.is_finite() {
            bail!("value must be a finite number");
        }
        let idx = self.index(name)?;
        self.points[idx].def.signal = None;
        self.points[idx].cur = value;
        self.store(idx, value);
        Ok(())
    }

    pub fn set_signal(&mut self, name: &str, signal: Signal, now: f64) -> Result<()> {
        signal.validate()?;
        let idx = self.index(name)?;
        // Counters and walks continue from what the client currently sees.
        self.points[idx].cur = self.load(idx);
        self.points[idx].def.signal = Some(signal);
        self.points[idx].signal_t0 = now;
        Ok(())
    }

    pub fn clear_signal(&mut self, name: &str) -> Result<()> {
        let idx = self.index(name)?;
        self.points[idx].def.signal = None;
        Ok(())
    }

    pub fn add_point(&mut self, mut def: Point, now: f64) -> Result<()> {
        def.normalize()?;
        if self.points.iter().any(|p| p.def.name == def.name) {
            bail!("device '{}' already has a point named '{}'", self.id, def.name);
        }
        let cur = def.value;
        self.points.push(PointState { def, cur, signal_t0: now });
        let idx = self.points.len() - 1;
        self.store(idx, cur);
        Ok(())
    }

    pub fn remove_point(&mut self, name: &str) -> Result<()> {
        let idx = self.index(name)?;
        self.points.remove(idx);
        Ok(())
    }

    // ---- faults and scenarios ---------------------------------------------------------

    pub fn inject_fault(&mut self, fault: Fault, now: f64) -> Result<()> {
        fault.validate()?;
        let until = fault.duration_s.map(|d| now + d);
        self.fault = Some(ActiveFault { fault, until });
        Ok(())
    }

    pub fn apply_scenario(&mut self, mut scenario: Scenario, now: f64) -> Result<()> {
        scenario.validate()?;
        for s in &scenario.steps {
            match &s.action {
                Action::SetPoint { point, .. } | Action::SetSignal { point, .. } | Action::ClearSignal { point } => {
                    self.index(point)?;
                }
                _ => {}
            }
        }
        scenario.steps.sort_by(|a, b| a.at_s.total_cmp(&b.at_s));
        self.scenario = Some(RunningScenario { scenario, t0: now, next: 0, loops: 0, finished: false });
        Ok(())
    }

    fn run_action(&mut self, action: Action, now: f64) {
        self.pending.push(format!("Scenario step: {}", describe_action(&action)));
        // Steps were validated when the scenario was applied; a point removed since then is skipped.
        let _ = match action {
            Action::SetPoint { point, value } => self.set_point(&point, value),
            Action::SetSignal { point, signal } => self.set_signal(&point, signal, now),
            Action::ClearSignal { point } => self.clear_signal(&point),
            Action::Fault { fault } => self.inject_fault(fault, now),
            Action::ClearFault => {
                self.fault = None;
                Ok(())
            }
        };
    }

    fn tick(&mut self, now: f64, dt: f64) {
        if matches!(&self.fault, Some(f) if f.until.is_some_and(|u| now >= u)) {
            self.fault = None;
            self.pending.push("Fault ended (time limit reached)".to_string());
        }

        // Collect the scenario steps that are due, then run them (keeps the borrow simple).
        let mut due = Vec::new();
        let mut finished = None;
        if let Some(rs) = &mut self.scenario {
            if !rs.finished {
                let elapsed = now - rs.t0;
                while rs.next < rs.scenario.steps.len() && rs.scenario.steps[rs.next].at_s <= elapsed {
                    due.push(rs.scenario.steps[rs.next].action.clone());
                    rs.next += 1;
                }
                if rs.next >= rs.scenario.steps.len() {
                    if rs.scenario.repeat {
                        rs.t0 = now;
                        rs.next = 0;
                        rs.loops += 1;
                    } else {
                        rs.finished = true;
                        finished = Some(rs.scenario.steps.len());
                    }
                }
            }
        }
        for a in due {
            self.run_action(a, now);
        }
        if let Some(steps) = finished {
            self.pending.push(format!("Scenario finished ({} steps)", steps));
        }

        for idx in 0..self.points.len() {
            let Some(signal) = self.points[idx].def.signal.clone() else { continue };
            let rnd = self.rng.next_f64();
            let p = &self.points[idx];
            let v = signal.eval(now - p.signal_t0, dt, p.cur, rnd);
            self.points[idx].cur = v;
            self.store(idx, v);
        }
    }

    pub fn log_request(&mut self, peer: &str, function: u8, address: u16, quantity: u16, result: String) {
        self.requests += 1;
        if self.log.len() >= LOG_CAPACITY {
            self.log.pop_front();
        }
        let unix_ms = unix_ms();
        self.log.push_back(LogEntry { unix_ms, peer: peer.to_string(), function, address, quantity, result });
    }

    // ---- reporting --------------------------------------------------------------------

    pub fn summary(&self, bind: IpAddr, now: f64) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "template": self.template,
            "host": connect_host(bind),
            "port": self.port,
            "unit_id": self.unit_id,
            "points": self.points.len(),
            "requests_served": self.requests,
            "fault": self.fault_json(now),
            "scenario": self.scenario_json(now),
        })
    }

    pub fn detail(&self, bind: IpAddr, now: f64) -> Value {
        let mut v = self.summary(bind, now);
        v["strict"] = json!(self.strict);
        v["points"] = self.points_json(None);
        v["scenario_steps"] = match &self.scenario {
            None => Value::Null,
            Some(rs) => Value::Array(
                rs.scenario
                    .steps
                    .iter()
                    .enumerate()
                    .map(|(i, s)| json!({ "at_s": s.at_s, "text": describe_action(&s.action), "done": rs.finished || i < rs.next }))
                    .collect(),
            ),
        };
        v
    }

    pub fn points_json(&self, only: Option<&[String]>) -> Value {
        let list: Vec<Value> = self
            .points
            .iter()
            .enumerate()
            .filter(|(_, p)| only.is_none_or(|names| names.contains(&p.def.name)))
            .map(|(i, p)| {
                json!({
                    "name": p.def.name,
                    "value": round6(self.load(i)),
                    "unit": p.def.unit,
                    "table": p.def.table,
                    "address": p.def.address,
                    "data_type": p.def.data_type,
                    "scale": p.def.scale,
                    "signal": p.def.signal,
                    "behaviour": p.def.signal.as_ref().map(describe_signal),
                    "description": p.def.description,
                })
            })
            .collect();
        Value::Array(list)
    }

    fn fault_json(&self, now: f64) -> Value {
        match &self.fault {
            None => Value::Null,
            Some(f) => {
                let mut v = serde_json::to_value(&f.fault.kind).unwrap_or(Value::Null);
                v["text"] = json!(describe_fault(&f.fault));
                if let Some(u) = f.until {
                    v["remaining_s"] = json!(round1((u - now).max(0.0)));
                }
                v
            }
        }
    }

    fn scenario_json(&self, now: f64) -> Value {
        match &self.scenario {
            None => Value::Null,
            Some(rs) => json!({
                "name": rs.scenario.name,
                "state": if rs.finished { "finished" } else { "running" },
                "elapsed_s": round1(now - rs.t0),
                "next_step": rs.next,
                "steps": rs.scenario.steps.len(),
                "repeat": rs.scenario.repeat,
                "loops_completed": rs.loops,
            }),
        }
    }

    /// Current state as a spec that recreates this device (used for saving and cloning).
    pub fn export(&self) -> DeviceSpec {
        DeviceSpec {
            id: Some(self.id.clone()),
            name: Some(self.name.clone()),
            template: None,
            port: Some(self.port),
            unit_id: self.unit_id,
            strict: self.strict,
            points: self
                .points
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let mut def = p.def.clone();
                    def.value = round6(self.load(i));
                    def
                })
                .collect(),
            scenario: self.scenario.as_ref().filter(|rs| !rs.finished).map(|rs| rs.scenario.clone()),
        }
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn round6(v: f64) -> f64 {
    if v.is_finite() {
        (v * 1e6).round() / 1e6
    } else {
        0.0
    }
}

/// Address a client on this machine should connect to.
pub fn connect_host(bind: IpAddr) -> String {
    if bind.is_unspecified() {
        "127.0.0.1 (listening on all interfaces)".to_string()
    } else {
        bind.to_string()
    }
}

pub struct Sim {
    pub cfg: Config,
    pub devices: BTreeMap<String, Device>,
    pub listeners: HashMap<u16, JoinHandle<()>>,
    pub events: VecDeque<Event>,
    t0: Instant,
}

pub type Shared = Arc<Mutex<Sim>>;

impl Sim {
    pub fn new(cfg: Config) -> Shared {
        Arc::new(Mutex::new(Sim {
            cfg,
            devices: BTreeMap::new(),
            listeners: HashMap::new(),
            events: VecDeque::new(),
            t0: Instant::now(),
        }))
    }

    pub fn now(&self) -> f64 {
        self.t0.elapsed().as_secs_f64()
    }

    pub fn device(&mut self, id: &str) -> Result<&mut Device> {
        if self.devices.is_empty() {
            bail!("no devices are running. Create one with create_device first.");
        }
        let known: Vec<String> = self.devices.keys().cloned().collect();
        self.devices.get_mut(id).ok_or_else(|| anyhow!("no device with id '{}'. Devices: {}", id, known.join(", ")))
    }

    /// Add a line to the activity feed shown on the dashboard.
    pub fn note(&mut self, device: Option<&str>, text: impl Into<String>) {
        if self.events.len() >= EVENT_CAPACITY {
            self.events.pop_front();
        }
        self.events.push_back(Event { unix_ms: unix_ms(), device: device.map(str::to_string), text: text.into() });
    }

    pub fn tick(&mut self, dt: f64) {
        let now = self.now();
        let mut happened = Vec::new();
        for d in self.devices.values_mut() {
            d.tick(now, dt);
            for text in d.pending.drain(..) {
                happened.push((d.id.clone(), text));
            }
        }
        for (id, text) in happened {
            self.note(Some(&id), text);
        }
    }

    /// Device that should answer a request arriving on `port` for `unit`.
    pub fn route(&mut self, port: u16, unit: u8) -> Option<&mut Device> {
        let mut on_port = 0usize;
        let mut exact: Option<String> = None;
        let mut only: Option<String> = None;
        for d in self.devices.values() {
            if d.port == port {
                on_port += 1;
                only = Some(d.id.clone());
                if d.unit_id == unit {
                    exact = Some(d.id.clone());
                }
            }
        }
        // Many TCP clients send unit 0 or 255 when talking straight to a device.
        let id = exact.or(if on_port == 1 && (unit == 0 || unit == 255) { only } else { None })?;
        self.devices.get_mut(&id)
    }

    pub fn port_in_use(&self, port: u16) -> bool {
        self.devices.values().any(|d| d.port == port)
    }

    /// Removes a device. Returns the listener to shut down when it was the last device on its port.
    fn take_device(&mut self, id: &str) -> Result<(Value, Option<JoinHandle<()>>)> {
        let d = self.device(id)?;
        let (port, unit) = (d.port, d.unit_id);
        self.devices.remove(id);
        let listener = if self.port_in_use(port) { None } else { self.listeners.remove(&port) };
        Ok((json!({ "removed": id, "port": port, "unit_id": unit }), listener))
    }

    fn take_all(&mut self) -> (usize, Vec<JoinHandle<()>>) {
        let n = self.devices.len();
        self.devices.clear();
        (n, self.listeners.drain().map(|(_, h)| h).collect())
    }

    pub fn export(&self) -> Project {
        Project { devices: self.devices.values().map(|d| d.export()).collect() }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Template points first, then the spec's own points; a spec point replaces a template point of the same name.
fn resolve_points(spec: &DeviceSpec) -> Result<Vec<Point>> {
    let mut points: Vec<Point> = match &spec.template {
        None => Vec::new(),
        Some(name) => {
            let t = templates::find(name).ok_or_else(|| {
                let names: Vec<&str> = templates::TEMPLATES.iter().map(|t| t.name).collect();
                anyhow!("unknown template '{}'. Available: {}", name, names.join(", "))
            })?;
            (t.points)()
        }
    };
    for p in &spec.points {
        match points.iter_mut().find(|q| q.name == p.name) {
            Some(existing) => *existing = p.clone(),
            None => points.push(p.clone()),
        }
    }
    for p in &mut points {
        p.normalize()?;
    }
    Ok(points)
}

/// Create a device and start listening for it. Picks the next free port unless the spec names one.
pub async fn create_device(shared: &Shared, spec: DeviceSpec) -> Result<Value> {
    let points = resolve_points(&spec)?;
    if let Some(s) = &spec.scenario {
        s.validate()?;
    }

    // Decide on an id and a port to try, without holding the lock across the bind.
    let (id, bind, mut candidate, explicit, needs_listener) = {
        let sim = shared.lock().unwrap();
        if sim.devices.len() >= sim.cfg.max_devices {
            bail!(
                "device limit reached ({}). Remove devices or start the simulator with a higher --max-devices.",
                sim.cfg.max_devices
            );
        }
        let id = match &spec.id {
            Some(id) => {
                if !valid_id(id) {
                    bail!("device id may only contain letters, digits, '-' and '_' (max 64 characters)");
                }
                if sim.devices.contains_key(id) {
                    bail!("a device with id '{}' already exists", id);
                }
                id.clone()
            }
            None => {
                let base = spec.template.clone().unwrap_or_else(|| "device".to_string()).replace('_', "-");
                // Lowest free number for this kind of device: pump-1, pump-2, ...
                (1u32..)
                    .map(|n| format!("{}-{}", base, n))
                    .find(|id| !sim.devices.contains_key(id))
                    .expect("an unused id always exists")
            }
        };
        match spec.port {
            Some(port) => {
                if port == 0 {
                    bail!("port must be between 1 and 65535");
                }
                if sim.devices.values().any(|d| d.port == port && d.unit_id == spec.unit_id) {
                    bail!(
                        "port {} already has a device with unit id {}. Use another unit_id to share the port, or another port.",
                        port,
                        spec.unit_id
                    );
                }
                let needs = !sim.listeners.contains_key(&port);
                (id, sim.cfg.bind, port, true, needs)
            }
            None => {
                let mut port = sim.cfg.base_port;
                while sim.port_in_use(port) {
                    port = port.checked_add(1).ok_or_else(|| anyhow!("ran out of ports above the base port"))?;
                }
                (id, sim.cfg.bind, port, false, true)
            }
        }
    };

    let mut listener = None;
    if needs_listener {
        let mut attempts = 0;
        loop {
            match TcpListener::bind((bind, candidate)).await {
                Ok(l) => {
                    listener = Some(l);
                    break;
                }
                Err(e) if explicit => {
                    bail!("cannot listen on {}:{} ({}). Pick another port or omit it to get the next free one.", bind, candidate, e)
                }
                Err(e) => {
                    attempts += 1;
                    if attempts >= AUTO_PORT_ATTEMPTS {
                        bail!("could not find a free port starting from the base port (last error: {})", e);
                    }
                    // Taken by another program, or by one of our own devices: move on.
                    loop {
                        candidate = candidate.checked_add(1).ok_or_else(|| anyhow!("ran out of ports"))?;
                        if !shared.lock().unwrap().port_in_use(candidate) {
                            break;
                        }
                    }
                }
            }
        }
    }

    let mut sim = shared.lock().unwrap();
    let now = sim.now();
    if let Some(l) = listener {
        let handle = net::spawn_listener(shared.clone(), l, candidate);
        if let Some(old) = sim.listeners.insert(candidate, handle) {
            old.abort();
        }
    }
    let mut device = Device::new(id.clone(), &spec, candidate, points, now);
    if let Some(s) = spec.scenario.clone() {
        device.apply_scenario(s, now)?;
    }
    let summary = device.summary(bind, now);
    sim.devices.insert(id, device);
    Ok(summary)
}

/// Stop a listener task and wait until its socket is really closed, so the port can be reused at once.
async fn shut_down(handle: JoinHandle<()>) {
    handle.abort();
    let _ = handle.await;
}

pub async fn remove_device(shared: &Shared, id: &str) -> Result<Value> {
    let (info, listener) = shared.lock().unwrap().take_device(id)?;
    if let Some(h) = listener {
        shut_down(h).await;
    }
    Ok(info)
}

pub async fn remove_all(shared: &Shared) -> usize {
    let (n, listeners) = shared.lock().unwrap().take_all();
    for h in listeners {
        shut_down(h).await;
    }
    n
}

/// Load every device of a project. Stops at the first device that cannot be created.
pub async fn load_project(shared: &Shared, project: Project) -> Result<Vec<Value>> {
    let mut created = Vec::new();
    for spec in project.devices {
        created.push(create_device(shared, spec).await?);
    }
    Ok(created)
}

/// Advance signals, scenarios and fault timers until the process exits.
pub fn spawn_ticker(shared: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        let tick_ms = shared.lock().unwrap().cfg.tick_ms.max(10);
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(tick_ms));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last = Instant::now();
        loop {
            interval.tick().await;
            let dt = last.elapsed().as_secs_f64();
            last = Instant::now();
            shared.lock().unwrap().tick(dt);
        }
    })
}
