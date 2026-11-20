//! Data model: points, signals, faults, scenarios and device specs.
//! Everything here is plain data that round-trips through JSON (MCP) and YAML (project files).

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

fn one() -> f64 {
    1.0
}
fn default_unit_id() -> u8 {
    1
}
fn yes() -> bool {
    true
}
fn is_false(b: &bool) -> bool {
    !*b
}
fn is_one(v: &f64) -> bool {
    *v == 1.0
}

/// The four Modbus data tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Table {
    /// Read/write bits (function codes 1, 5, 15).
    Coil,
    /// Read-only bits (function code 2).
    Discrete,
    /// Read/write 16-bit registers (function codes 3, 6, 16).
    Holding,
    /// Read-only 16-bit registers (function code 4).
    Input,
}

impl Table {
    pub fn is_bit(self) -> bool {
        matches!(self, Table::Coil | Table::Discrete)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataType {
    Bool,
    #[default]
    U16,
    I16,
    U32,
    I32,
    F32,
}

impl DataType {
    pub fn words(self) -> u16 {
        match self {
            DataType::U32 | DataType::I32 | DataType::F32 => 2,
            _ => 1,
        }
    }
}

/// A named value mapped onto one or two Modbus addresses.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Point {
    pub name: String,
    pub table: Table,
    /// Zero-based protocol address (holding register 40001 is address 0).
    pub address: u16,
    #[serde(default)]
    pub data_type: DataType,
    /// Engineering value = raw value * scale.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub scale: f64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit: String,
    /// For 32-bit types: send the low word first.
    #[serde(default, skip_serializing_if = "is_false")]
    pub word_swap: bool,
    /// Initial engineering value.
    #[serde(default)]
    pub value: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<Signal>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// Where and how a point is stored; small and `Copy` so it can be used while the device is borrowed.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub table: Table,
    pub address: u16,
    pub data_type: DataType,
    pub scale: f64,
    pub word_swap: bool,
}

impl Point {
    pub fn layout(&self) -> Layout {
        Layout {
            table: self.table,
            address: self.address,
            data_type: self.data_type,
            scale: self.scale,
            word_swap: self.word_swap,
        }
    }

    /// Fix up combinations that cannot work and reject the ones that are plainly wrong.
    pub fn normalize(&mut self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("a point needs a name");
        }
        if self.table.is_bit() {
            self.data_type = DataType::Bool;
            self.scale = 1.0;
        } else if self.data_type == DataType::Bool {
            self.data_type = DataType::U16;
        }
        if self.scale == 0.0 || !self.scale.is_finite() {
            bail!("point '{}': scale must be a non-zero number", self.name);
        }
        if self.address as u32 + self.data_type.words() as u32 > 65536 {
            bail!("point '{}': address {} is out of range", self.name, self.address);
        }
        if let Some(s) = &self.signal {
            s.validate()?;
        }
        Ok(())
    }
}

/// How a point's value changes over time.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Signal {
    /// Hold one value.
    Constant { value: f64 },
    /// Move linearly from `from` to `to` over `period_s`; restarts unless `repeat` is false.
    Ramp {
        from: f64,
        to: f64,
        period_s: f64,
        #[serde(default = "yes")]
        repeat: bool,
    },
    /// Sine wave between `min` and `max`.
    Sine { min: f64, max: f64, period_s: f64 },
    /// A new random value between `min` and `max` on every tick.
    Random { min: f64, max: f64 },
    /// Drift by at most `step` per tick, staying between `min` and `max`.
    RandomWalk { min: f64, max: f64, step: f64 },
    /// Increase by `per_second` every second; wrap to zero at `rollover` if given.
    Counter {
        per_second: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rollover: Option<f64>,
    },
    /// Alternate between `low` and `high`, half a period each.
    Square { low: f64, high: f64, period_s: f64 },
}

impl Signal {
    pub fn validate(&self) -> Result<()> {
        let period = match self {
            Signal::Ramp { period_s, .. } | Signal::Sine { period_s, .. } | Signal::Square { period_s, .. } => {
                Some(*period_s)
            }
            _ => None,
        };
        if let Some(p) = period {
            if p.is_nan() || p <= 0.0 || p.is_infinite() {
                bail!("signal period_s must be greater than zero");
            }
        }
        match self {
            Signal::Random { min, max } | Signal::RandomWalk { min, max, .. } | Signal::Sine { min, max, .. } => {
                if min > max {
                    bail!("signal min must not be greater than max");
                }
            }
            Signal::Counter { rollover: Some(r), .. } if r.is_nan() || *r <= 0.0 => {
                bail!("counter rollover must be greater than zero");
            }
            _ => {}
        }
        Ok(())
    }

    /// Next value. `t` is seconds since the signal started, `dt` seconds since the last tick,
    /// `cur` the previous value and `rnd` a uniform random number in [0, 1).
    pub fn eval(&self, t: f64, dt: f64, cur: f64, rnd: f64) -> f64 {
        match *self {
            Signal::Constant { value } => value,
            Signal::Ramp { from, to, period_s, repeat } => {
                let frac = if repeat { (t % period_s) / period_s } else { (t / period_s).min(1.0) };
                from + (to - from) * frac
            }
            Signal::Sine { min, max, period_s } => {
                let mid = (min + max) / 2.0;
                let amp = (max - min) / 2.0;
                mid + amp * (2.0 * std::f64::consts::PI * t / period_s).sin()
            }
            Signal::Random { min, max } => min + rnd * (max - min),
            Signal::RandomWalk { min, max, step } => (cur + (rnd * 2.0 - 1.0) * step).clamp(min, max),
            Signal::Counter { per_second, rollover } => {
                let v = cur + per_second * dt;
                match rollover {
                    Some(r) if v >= r => v % r,
                    _ => v,
                }
            }
            Signal::Square { low, high, period_s } => {
                if (t % period_s) < period_s / 2.0 {
                    high
                } else {
                    low
                }
            }
        }
    }
}

/// A communication fault applied to one device.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FaultKind {
    /// Answer correctly, but only after `ms` milliseconds.
    Delay { ms: u64 },
    /// Receive requests and never answer (the client sees a timeout).
    NoResponse,
    /// Answer every request with this Modbus exception code (e.g. 4 = device failure, 6 = busy).
    Exception { code: u8 },
    /// Close the TCP connection whenever a request arrives.
    Disconnect,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fault {
    #[serde(flatten)]
    pub kind: FaultKind,
    /// Clear the fault automatically after this many seconds. Omit to keep it until cleared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_s: Option<f64>,
}

impl Fault {
    pub fn validate(&self) -> Result<()> {
        if let FaultKind::Exception { code } = self.kind {
            if code == 0 || code > 0x7f {
                bail!("exception code must be between 1 and 127");
            }
        }
        if let FaultKind::Delay { ms } = self.kind {
            if ms > 600_000 {
                bail!("delay cannot be longer than 600000 ms");
            }
        }
        if let Some(d) = self.duration_s {
            if d.is_nan() || d <= 0.0 {
                bail!("duration_s must be greater than zero");
            }
        }
        Ok(())
    }
}

/// One thing that happens at a given moment of a scenario.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// Set a point to a fixed value (stops any signal on that point).
    SetPoint { point: String, value: f64 },
    /// Start a signal on a point.
    SetSignal { point: String, signal: Signal },
    /// Stop the signal on a point; the value stays where it is.
    ClearSignal { point: String },
    /// Start a communication fault.
    Fault { fault: Fault },
    /// Clear the communication fault.
    ClearFault,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Step {
    /// Seconds after the scenario starts.
    pub at_s: f64,
    #[serde(flatten)]
    pub action: Action,
}

/// A timeline of steps played against one device.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scenario {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Start again from the beginning after the last step.
    #[serde(default, skip_serializing_if = "is_false")]
    pub repeat: bool,
    pub steps: Vec<Step>,
}

impl Scenario {
    pub fn validate(&self) -> Result<()> {
        if self.steps.is_empty() {
            bail!("a scenario needs at least one step");
        }
        for s in &self.steps {
            if s.at_s.is_nan() || s.at_s < 0.0 || s.at_s.is_infinite() {
                bail!("step at_s must be zero or greater");
            }
            match &s.action {
                Action::SetSignal { signal, .. } => signal.validate()?,
                Action::Fault { fault } => fault.validate()?,
                _ => {}
            }
        }
        if self.repeat && self.steps.iter().all(|s| s.at_s == 0.0) {
            bail!("a repeating scenario needs at least one step with at_s greater than zero");
        }
        Ok(())
    }
}

/// Everything needed to create a device. Also the shape of a device inside a project file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Name of a built-in template whose points are used as a starting set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// TCP port. Omit to get the next free port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default = "default_unit_id")]
    pub unit_id: u8,
    /// Answer "illegal data address" for addresses that no point covers.
    #[serde(default, skip_serializing_if = "is_false")]
    pub strict: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub points: Vec<Point>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenario: Option<Scenario>,
}

/// A saved set of devices.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Project {
    #[serde(default)]
    pub devices: Vec<DeviceSpec>,
}

/// What a 16-bit or 32-bit or bit point turns into on the wire.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Raw {
    Bit(bool),
    One(u16),
    Two(u16, u16),
}

/// Engineering value -> raw words (in wire order).
pub fn encode(l: Layout, value: f64) -> Raw {
    if l.table.is_bit() {
        return Raw::Bit(value != 0.0);
    }
    let raw = value / l.scale;
    let two = |bits: u32| {
        let hi = (bits >> 16) as u16;
        let lo = bits as u16;
        if l.word_swap {
            Raw::Two(lo, hi)
        } else {
            Raw::Two(hi, lo)
        }
    };
    match l.data_type {
        DataType::Bool => Raw::One((value != 0.0) as u16),
        DataType::U16 => Raw::One(raw.round().clamp(0.0, 65535.0) as u16),
        DataType::I16 => Raw::One(raw.round().clamp(-32768.0, 32767.0) as i16 as u16),
        DataType::U32 => two(raw.round().clamp(0.0, u32::MAX as f64) as u32),
        DataType::I32 => two(raw.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32 as u32),
        DataType::F32 => two((raw as f32).to_bits()),
    }
}

/// Raw words (in wire order) -> engineering value.
pub fn decode(l: Layout, raw: Raw) -> f64 {
    match raw {
        Raw::Bit(b) => b as u8 as f64,
        Raw::One(w) => match l.data_type {
            DataType::I16 => (w as i16) as f64 * l.scale,
            DataType::Bool => (w != 0) as u8 as f64,
            _ => w as f64 * l.scale,
        },
        Raw::Two(a, b) => {
            let (hi, lo) = if l.word_swap { (b, a) } else { (a, b) };
            let bits = ((hi as u32) << 16) | lo as u32;
            match l.data_type {
                DataType::I32 => (bits as i32) as f64 * l.scale,
                DataType::F32 => f32::from_bits(bits) as f64 * l.scale,
                _ => bits as f64 * l.scale,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lay(dt: DataType, scale: f64, swap: bool) -> Layout {
        Layout { table: Table::Holding, address: 0, data_type: dt, scale, word_swap: swap }
    }

    #[test]
    fn scaled_u16_round_trip() {
        let l = lay(DataType::U16, 0.1, false);
        assert_eq!(encode(l, 23.5), Raw::One(235));
        assert!((decode(l, Raw::One(235)) - 23.5).abs() < 1e-9);
    }

    #[test]
    fn negative_i16() {
        let l = lay(DataType::I16, 1.0, false);
        assert_eq!(encode(l, -2.0), Raw::One(0xfffe));
        assert_eq!(decode(l, Raw::One(0xfffe)), -2.0);
    }

    #[test]
    fn u32_word_order() {
        assert_eq!(encode(lay(DataType::U32, 1.0, false), 65537.0), Raw::Two(1, 1));
        assert_eq!(encode(lay(DataType::U32, 1.0, false), 65538.0), Raw::Two(1, 2));
        assert_eq!(encode(lay(DataType::U32, 1.0, true), 65538.0), Raw::Two(2, 1));
        assert_eq!(decode(lay(DataType::U32, 1.0, true), Raw::Two(2, 1)), 65538.0);
    }

    #[test]
    fn f32_round_trip() {
        let l = lay(DataType::F32, 1.0, false);
        let r = encode(l, 230.5);
        assert_eq!(r, Raw::Two(0x4366, 0x8000));
        assert_eq!(decode(l, r), 230.5);
    }

    #[test]
    fn values_are_clamped_not_wrapped() {
        assert_eq!(encode(lay(DataType::U16, 1.0, false), 70000.0), Raw::One(65535));
        assert_eq!(encode(lay(DataType::U16, 1.0, false), -5.0), Raw::One(0));
    }

    #[test]
    fn signals_behave() {
        let ramp = Signal::Ramp { from: 0.0, to: 10.0, period_s: 10.0, repeat: true };
        assert_eq!(ramp.eval(5.0, 0.1, 0.0, 0.0), 5.0);
        assert_eq!(ramp.eval(15.0, 0.1, 0.0, 0.0), 5.0);
        let once = Signal::Ramp { from: 0.0, to: 10.0, period_s: 10.0, repeat: false };
        assert_eq!(once.eval(15.0, 0.1, 0.0, 0.0), 10.0);
        let sine = Signal::Sine { min: 20.0, max: 80.0, period_s: 60.0 };
        assert!((sine.eval(15.0, 0.1, 0.0, 0.0) - 80.0).abs() < 1e-9);
        let counter = Signal::Counter { per_second: 2.0, rollover: Some(10.0) };
        assert_eq!(counter.eval(0.0, 1.0, 9.0, 0.0), 1.0);
        let walk = Signal::RandomWalk { min: 0.0, max: 1.0, step: 5.0 };
        assert_eq!(walk.eval(0.0, 0.1, 0.5, 0.999), 1.0);
    }

    #[test]
    fn scenario_parses_from_json() {
        let s: Scenario = serde_json::from_str(
            r#"{"steps":[
                {"at_s":0,"action":"set_signal","point":"t","signal":{"type":"sine","min":1,"max":2,"period_s":3}},
                {"at_s":5,"action":"fault","fault":{"type":"delay","ms":500,"duration_s":10}},
                {"at_s":20,"action":"clear_fault"}]}"#,
        )
        .unwrap();
        s.validate().unwrap();
        assert_eq!(s.steps.len(), 3);
    }
}
