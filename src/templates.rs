//! Built-in device templates. These are generic examples, not copies of any vendor's register map.

use crate::model::{DataType, Point, Signal, Table};

pub struct Template {
    pub name: &'static str,
    pub description: &'static str,
    pub points: fn() -> Vec<Point>,
}

pub const TEMPLATES: &[Template] = &[
    Template { name: "generic", description: "Empty device. Add your own points.", points: generic },
    Template {
        name: "energy_meter",
        description: "Three-phase energy meter: voltages, currents, power, frequency, energy counter (32-bit floats in input registers).",
        points: energy_meter,
    },
    Template {
        name: "vfd",
        description: "Variable frequency drive: command and speed setpoint (holding), status, frequency, current, temperature (input).",
        points: vfd,
    },
    Template {
        name: "temperature_controller",
        description: "Temperature controller: process value, setpoint, output percentage, high/low alarms.",
        points: temperature_controller,
    },
    Template {
        name: "pump",
        description: "Pump station unit: run command, running/fault status, flow, pressure, motor current, run hours.",
        points: pump,
    },
    Template {
        name: "tank",
        description: "Storage tank: level, temperature, level switches, inlet and outlet valves.",
        points: tank,
    },
];

pub fn find(name: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| t.name.eq_ignore_ascii_case(name))
}

#[allow(clippy::too_many_arguments)]
fn p(
    name: &str,
    table: Table,
    address: u16,
    data_type: DataType,
    scale: f64,
    unit: &str,
    value: f64,
    signal: Option<Signal>,
    description: &str,
) -> Point {
    Point {
        name: name.into(),
        table,
        address,
        data_type,
        scale,
        unit: unit.into(),
        word_swap: false,
        value,
        signal,
        description: description.into(),
    }
}

fn walk(min: f64, max: f64, step: f64) -> Option<Signal> {
    Some(Signal::RandomWalk { min, max, step })
}

fn generic() -> Vec<Point> {
    Vec::new()
}

fn energy_meter() -> Vec<Point> {
    use DataType::F32;
    use Table::Input;
    vec![
        p("voltage_l1", Input, 0, F32, 1.0, "V", 230.0, walk(225.0, 235.0, 0.3), "Phase 1 voltage"),
        p("voltage_l2", Input, 2, F32, 1.0, "V", 230.0, walk(225.0, 235.0, 0.3), "Phase 2 voltage"),
        p("voltage_l3", Input, 4, F32, 1.0, "V", 230.0, walk(225.0, 235.0, 0.3), "Phase 3 voltage"),
        p("current_l1", Input, 6, F32, 1.0, "A", 12.0, walk(8.0, 16.0, 0.2), "Phase 1 current"),
        p("current_l2", Input, 8, F32, 1.0, "A", 12.0, walk(8.0, 16.0, 0.2), "Phase 2 current"),
        p("current_l3", Input, 10, F32, 1.0, "A", 12.0, walk(8.0, 16.0, 0.2), "Phase 3 current"),
        p("active_power", Input, 12, F32, 1.0, "kW", 7.8, walk(5.5, 10.5, 0.1), "Total active power"),
        p("frequency", Input, 14, F32, 1.0, "Hz", 50.0, walk(49.95, 50.05, 0.005), "Grid frequency"),
        p("power_factor", Input, 16, F32, 1.0, "", 0.95, walk(0.9, 0.99, 0.003), "Power factor"),
        p(
            "energy_total",
            Input,
            18,
            F32,
            1.0,
            "kWh",
            15230.0,
            Some(Signal::Counter { per_second: 0.0022, rollover: None }),
            "Total active energy",
        ),
    ]
}

fn vfd() -> Vec<Point> {
    use DataType::U16;
    use Table::{Coil, Holding, Input};
    vec![
        p("command_word", Holding, 0, U16, 1.0, "", 0.0, None, "Command word written by the controller"),
        p("speed_setpoint", Holding, 1, U16, 0.1, "Hz", 35.0, None, "Speed setpoint"),
        p("accel_time", Holding, 2, U16, 0.1, "s", 5.0, None, "Acceleration time"),
        p("decel_time", Holding, 3, U16, 0.1, "s", 5.0, None, "Deceleration time"),
        p("status_word", Input, 0, U16, 1.0, "", 1.0, None, "Status word (1 = ready)"),
        p("output_frequency", Input, 1, U16, 0.1, "Hz", 35.0, walk(34.6, 35.4, 0.1), "Output frequency"),
        p("motor_current", Input, 2, U16, 0.1, "A", 14.2, walk(12.0, 16.5, 0.2), "Motor current"),
        p("dc_bus_voltage", Input, 3, U16, 1.0, "V", 540.0, walk(530.0, 550.0, 1.0), "DC bus voltage"),
        p("drive_temperature", Input, 4, U16, 0.1, "°C", 41.0, walk(38.0, 46.0, 0.1), "Heatsink temperature"),
        p("fault_code", Input, 5, U16, 1.0, "", 0.0, None, "Active fault code (0 = none)"),
        p("run", Coil, 0, DataType::Bool, 1.0, "", 1.0, None, "Run command"),
        p("fault_reset", Coil, 1, DataType::Bool, 1.0, "", 0.0, None, "Fault reset"),
    ]
}

fn temperature_controller() -> Vec<Point> {
    use DataType::{I16, U16};
    use Table::{Discrete, Holding, Input};
    vec![
        p(
            "process_value",
            Input,
            0,
            I16,
            0.1,
            "°C",
            60.0,
            Some(Signal::Sine { min: 58.5, max: 61.5, period_s: 120.0 }),
            "Measured temperature",
        ),
        p("output", Input, 1, U16, 0.1, "%", 42.0, walk(30.0, 55.0, 0.5), "Controller output"),
        p("setpoint", Holding, 0, I16, 0.1, "°C", 60.0, None, "Temperature setpoint"),
        p("alarm_high_limit", Holding, 1, I16, 0.1, "°C", 75.0, None, "High alarm limit"),
        p("alarm_low_limit", Holding, 2, I16, 0.1, "°C", 40.0, None, "Low alarm limit"),
        p("alarm_high", Discrete, 0, DataType::Bool, 1.0, "", 0.0, None, "High temperature alarm"),
        p("alarm_low", Discrete, 1, DataType::Bool, 1.0, "", 0.0, None, "Low temperature alarm"),
    ]
}

fn pump() -> Vec<Point> {
    use DataType::{U16, U32};
    use Table::{Coil, Discrete, Input};
    vec![
        p("run_command", Coil, 0, DataType::Bool, 1.0, "", 1.0, None, "Start/stop command"),
        p("running", Discrete, 0, DataType::Bool, 1.0, "", 1.0, None, "Pump is running"),
        p("fault", Discrete, 1, DataType::Bool, 1.0, "", 0.0, None, "Pump fault"),
        p("flow", Input, 0, U16, 0.1, "m³/h", 42.0, walk(38.0, 46.0, 0.3), "Flow rate"),
        p("pressure", Input, 1, U16, 0.01, "bar", 4.2, walk(3.9, 4.5, 0.02), "Discharge pressure"),
        p("motor_current", Input, 2, U16, 0.1, "A", 18.5, walk(17.0, 20.0, 0.1), "Motor current"),
        p(
            "run_hours",
            Input,
            3,
            U32,
            0.1,
            "h",
            1280.0,
            Some(Signal::Counter { per_second: 1.0 / 3600.0, rollover: None }),
            "Total run hours",
        ),
    ]
}

fn tank() -> Vec<Point> {
    use DataType::{I16, U16};
    use Table::{Coil, Discrete, Input};
    vec![
        p(
            "level",
            Input,
            0,
            U16,
            0.1,
            "%",
            55.0,
            Some(Signal::Sine { min: 30.0, max: 80.0, period_s: 300.0 }),
            "Fill level",
        ),
        p("temperature", Input, 1, I16, 0.1, "°C", 22.0, walk(20.0, 25.0, 0.05), "Liquid temperature"),
        p("high_level", Discrete, 0, DataType::Bool, 1.0, "", 0.0, None, "High level switch"),
        p("low_level", Discrete, 1, DataType::Bool, 1.0, "", 0.0, None, "Low level switch"),
        p("inlet_valve", Coil, 0, DataType::Bool, 1.0, "", 1.0, None, "Inlet valve open command"),
        p("outlet_valve", Coil, 1, DataType::Bool, 1.0, "", 0.0, None, "Outlet valve open command"),
    ]
}
