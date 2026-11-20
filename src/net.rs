//! Modbus TCP server side: one listener per port, any number of devices (unit ids) behind it.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::model::{FaultKind, Table};
use crate::sim::{Device, Shared, Sim};

const EX_ILLEGAL_FUNCTION: u8 = 1;
const EX_ILLEGAL_ADDRESS: u8 = 2;
const EX_ILLEGAL_VALUE: u8 = 3;
const EX_GATEWAY_NO_RESPONSE: u8 = 0x0b;

enum Outcome {
    Reply(Vec<u8>),
    Silent,
    Close,
}

pub fn spawn_listener(shared: Shared, listener: TcpListener, port: u16) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    let _ = stream.set_nodelay(true);
                    tokio::spawn(connection(shared.clone(), stream, port, peer.to_string()));
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    })
}

async fn connection(shared: Shared, mut stream: TcpStream, port: u16, peer: String) {
    let mut header = [0u8; 7];
    loop {
        if stream.read_exact(&mut header).await.is_err() {
            return;
        }
        let protocol = u16::from_be_bytes([header[2], header[3]]);
        let length = u16::from_be_bytes([header[4], header[5]]) as usize;
        let unit = header[6];
        // Length counts the unit id plus the PDU; a PDU is 1..=253 bytes.
        if protocol != 0 || !(2..=254).contains(&length) {
            return;
        }
        let mut pdu = vec![0u8; length - 1];
        if stream.read_exact(&mut pdu).await.is_err() {
            return;
        }

        let (outcome, delay_ms) = {
            let mut sim = shared.lock().unwrap();
            if !sim.port_in_use(port) {
                return; // every device on this port was removed
            }
            handle(&mut sim, port, unit, &pdu, &peer)
        };

        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        match outcome {
            Outcome::Silent => continue,
            Outcome::Close => return,
            Outcome::Reply(reply) => {
                let mut frame = Vec::with_capacity(7 + reply.len());
                frame.extend_from_slice(&header[0..4]);
                frame.extend_from_slice(&((reply.len() as u16 + 1).to_be_bytes()));
                frame.push(unit);
                frame.extend_from_slice(&reply);
                if stream.write_all(&frame).await.is_err() {
                    return;
                }
            }
        }
    }
}

fn exception(function: u8, code: u8) -> Vec<u8> {
    vec![function | 0x80, code]
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn handle(sim: &mut Sim, port: u16, unit: u8, pdu: &[u8], peer: &str) -> (Outcome, u64) {
    let function = pdu[0];
    let Some(device) = sim.route(port, unit) else {
        // Behave like a gateway that has no such station behind it.
        return (Outcome::Reply(exception(function, EX_GATEWAY_NO_RESPONSE)), 0);
    };

    let (address, quantity) = request_span(pdu);
    let fault = device.fault.as_ref().map(|f| f.fault.kind.clone());
    match fault {
        Some(FaultKind::NoResponse) => {
            device.log_request(peer, function, address, quantity, "fault: no response".into());
            return (Outcome::Silent, 0);
        }
        Some(FaultKind::Disconnect) => {
            device.log_request(peer, function, address, quantity, "fault: disconnected".into());
            return (Outcome::Close, 0);
        }
        Some(FaultKind::Exception { code }) => {
            device.log_request(peer, function, address, quantity, format!("fault: exception {}", code));
            return (Outcome::Reply(exception(function, code)), 0);
        }
        _ => {}
    }
    let delay = match fault {
        Some(FaultKind::Delay { ms }) => ms,
        _ => 0,
    };

    let (reply, result) = match execute(device, pdu) {
        Ok(reply) => (reply, if delay > 0 { format!("ok (delayed {} ms)", delay) } else { "ok".to_string() }),
        Err(code) => (exception(function, code), format!("exception {}", code)),
    };
    device.log_request(peer, function, address, quantity, result);
    (Outcome::Reply(reply), delay)
}

/// Address and quantity of a request, for the traffic log (zeros when the request is malformed).
fn request_span(pdu: &[u8]) -> (u16, u16) {
    if pdu.len() < 5 {
        return (0, 0);
    }
    match pdu[0] {
        1 | 2 | 3 | 4 | 15 | 16 => (be16(pdu, 1), be16(pdu, 3)),
        5 | 6 => (be16(pdu, 1), 1),
        _ => (0, 0),
    }
}

fn check_range(device: &Device, table: Table, address: u16, quantity: u16) -> Result<(), u8> {
    if address as u32 + quantity as u32 > 65536 {
        return Err(EX_ILLEGAL_ADDRESS);
    }
    if device.strict && !(0..quantity).all(|i| device.mapped(table, address + i)) {
        return Err(EX_ILLEGAL_ADDRESS);
    }
    Ok(())
}

/// Run one request against a device. `Err` carries the Modbus exception code.
fn execute(device: &mut Device, pdu: &[u8]) -> Result<Vec<u8>, u8> {
    let function = pdu[0];
    match function {
        1 | 2 => {
            if pdu.len() != 5 {
                return Err(EX_ILLEGAL_VALUE);
            }
            let (address, quantity) = (be16(pdu, 1), be16(pdu, 3));
            if !(1..=2000).contains(&quantity) {
                return Err(EX_ILLEGAL_VALUE);
            }
            let table = if function == 1 { Table::Coil } else { Table::Discrete };
            check_range(device, table, address, quantity)?;
            let mut bytes = vec![0u8; quantity.div_ceil(8) as usize];
            for i in 0..quantity {
                if device.bit(table, address + i) {
                    bytes[(i / 8) as usize] |= 1 << (i % 8);
                }
            }
            let mut reply = vec![function, bytes.len() as u8];
            reply.extend(bytes);
            Ok(reply)
        }
        3 | 4 => {
            if pdu.len() != 5 {
                return Err(EX_ILLEGAL_VALUE);
            }
            let (address, quantity) = (be16(pdu, 1), be16(pdu, 3));
            if !(1..=125).contains(&quantity) {
                return Err(EX_ILLEGAL_VALUE);
            }
            let table = if function == 3 { Table::Holding } else { Table::Input };
            check_range(device, table, address, quantity)?;
            let mut reply = vec![function, (quantity * 2) as u8];
            for i in 0..quantity {
                reply.extend_from_slice(&device.word(table, address + i).to_be_bytes());
            }
            Ok(reply)
        }
        5 => {
            if pdu.len() != 5 {
                return Err(EX_ILLEGAL_VALUE);
            }
            let (address, value) = (be16(pdu, 1), be16(pdu, 3));
            let on = match value {
                0xff00 => true,
                0x0000 => false,
                _ => return Err(EX_ILLEGAL_VALUE),
            };
            check_range(device, Table::Coil, address, 1)?;
            device.set_bit(Table::Coil, address, on);
            Ok(pdu.to_vec())
        }
        6 => {
            if pdu.len() != 5 {
                return Err(EX_ILLEGAL_VALUE);
            }
            let (address, value) = (be16(pdu, 1), be16(pdu, 3));
            check_range(device, Table::Holding, address, 1)?;
            device.set_word(Table::Holding, address, value);
            Ok(pdu.to_vec())
        }
        15 => {
            if pdu.len() < 6 {
                return Err(EX_ILLEGAL_VALUE);
            }
            let (address, quantity, count) = (be16(pdu, 1), be16(pdu, 3), pdu[5] as usize);
            if !(1..=1968).contains(&quantity) || count != quantity.div_ceil(8) as usize || pdu.len() != 6 + count {
                return Err(EX_ILLEGAL_VALUE);
            }
            check_range(device, Table::Coil, address, quantity)?;
            for i in 0..quantity {
                let on = pdu[6 + (i / 8) as usize] & (1 << (i % 8)) != 0;
                device.set_bit(Table::Coil, address + i, on);
            }
            Ok(pdu[0..5].to_vec())
        }
        16 => {
            if pdu.len() < 6 {
                return Err(EX_ILLEGAL_VALUE);
            }
            let (address, quantity, count) = (be16(pdu, 1), be16(pdu, 3), pdu[5] as usize);
            if !(1..=123).contains(&quantity) || count != quantity as usize * 2 || pdu.len() != 6 + count {
                return Err(EX_ILLEGAL_VALUE);
            }
            check_range(device, Table::Holding, address, quantity)?;
            for i in 0..quantity {
                device.set_word(Table::Holding, address + i, be16(pdu, 6 + i as usize * 2));
            }
            Ok(pdu[0..5].to_vec())
        }
        _ => Err(EX_ILLEGAL_FUNCTION),
    }
}
