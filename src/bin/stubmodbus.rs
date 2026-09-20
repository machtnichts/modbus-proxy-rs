//! A tiny Modbus/TCP stub device, for testing the proxy without going near real
//! hardware.
//!
//! Registers are deterministic: `reg[i] = i * 10`, so a test can tell a value
//! that came from the device from one the proxy invented.
//!
//! Usage: stubmodbus [--port N] [--forbidden START-END] [--quiet]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const MODBUS_PORT_DEFAULT: u16 = 15020;

struct Device {
    regs: Mutex<BTreeMap<u16, u16>>,
    forbidden: Vec<u16>,
    requests: AtomicU64,
    connections: AtomicU64,
    traffic: AtomicU64,
    quiet: bool,
}

impl Device {
    fn get(&self, addr: u16) -> u16 {
        if let Some(v) = self.regs.lock().unwrap().get(&addr) {
            return *v;
        }
        addr.wrapping_mul(10)
    }

    fn set(&self, addr: u16, val: u16) {
        self.regs.lock().unwrap().insert(addr, val);
    }
}

fn handle(fc: u8, body: &[u8], dev: &Device) -> Vec<u8> {
    match fc {
        3 if body.len() >= 5 => {
            let addr = u16::from_be_bytes([body[1], body[2]]);
            let count = u16::from_be_bytes([body[3], body[4]]);
            if (0..count).any(|i| dev.forbidden.contains(&addr.wrapping_add(i))) {
                return vec![0x83, 0x02];
            }
            let mut out = vec![3u8, (count * 2) as u8];
            for i in 0..count {
                out.extend_from_slice(&dev.get(addr.wrapping_add(i)).to_be_bytes());
            }
            out
        }
        6 if body.len() >= 5 => {
            let addr = u16::from_be_bytes([body[1], body[2]]);
            let val = u16::from_be_bytes([body[3], body[4]]);
            dev.set(addr, val);
            body[..5].to_vec()
        }
        16 if body.len() >= 6 => {
            let addr = u16::from_be_bytes([body[1], body[2]]);
            let count = u16::from_be_bytes([body[3], body[4]]);
            let bc = body[5] as usize;
            for i in 0..count as usize {
                let off = 6 + i * 2;
                if off + 1 < body.len().min(6 + bc) {
                    let v = u16::from_be_bytes([body[off], body[off + 1]]);
                    dev.set(addr.wrapping_add(i as u16), v);
                }
            }
            let mut out = vec![16u8];
            out.extend_from_slice(&addr.to_be_bytes());
            out.extend_from_slice(&count.to_be_bytes());
            out
        }
        3 => vec![0x83, 0x02],
        other => vec![other | 0x80, 0x01],
    }
}

fn serve(stream: TcpStream, dev: Arc<Device>) {
    dev.connections.fetch_add(1, Ordering::Relaxed);
    if !dev.quiet {
        println!(
            "CONNECTION total={}",
            dev.connections.load(Ordering::Relaxed)
        );
    }
    let mut stream = stream;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(60)));
    let mut hdr = [0u8; 7];
    let mut served_traffic = false;
    loop {
        if stream.read_exact(&mut hdr).is_err() {
            return;
        }
        let tid = u16::from_be_bytes([hdr[0], hdr[1]]);
        let pid = u16::from_be_bytes([hdr[2], hdr[3]]);
        let length = u16::from_be_bytes([hdr[4], hdr[5]]);
        let unit = hdr[6];
        if length < 1 {
            return;
        }
        let mut body = vec![0u8; length as usize - 1];
        if stream.read_exact(&mut body).is_err() {
            return;
        }
        let n = dev.requests.fetch_add(1, Ordering::Relaxed) + 1;
        if !served_traffic {
            served_traffic = true;
            dev.traffic.fetch_add(1, Ordering::Relaxed);
            if !dev.quiet {
                println!(
                    "TRAFFIC total={}  (first request on this connection)",
                    dev.traffic.load(Ordering::Relaxed)
                );
            }
        }
        if !dev.quiet {
            println!(
                "REQUEST n={} fc={} pdu={} unit={}",
                n,
                body[0],
                body.iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<Vec<_>>()
                    .join(""),
                unit
            );
        }
        let resp = handle(body[0], &body, &dev);
        let mut frame = Vec::new();
        frame.extend_from_slice(&tid.to_be_bytes());
        frame.extend_from_slice(&pid.to_be_bytes());
        frame.extend_from_slice(&((resp.len() + 1) as u16).to_be_bytes());
        frame.push(unit);
        frame.extend_from_slice(&resp);
        if stream.write_all(&frame).is_err() {
            return;
        }
        let _ = stream.flush();
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut port = MODBUS_PORT_DEFAULT;
    let mut forbidden = vec![50u16, 51, 52];
    let mut quiet = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                i += 1;
                port = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(port);
            }
            "--forbidden" => {
                i += 1;
                if let Some(spec) = args.get(i) {
                    if let Some((a, b)) = spec.split_once('-') {
                        if let (Ok(a), Ok(b)) = (a.parse::<u16>(), b.parse::<u16>()) {
                            forbidden = (a..=b).collect();
                        }
                    }
                }
            }
            "--quiet" => quiet = true,
            "-h" | "--help" => {
                println!("stubmodbus [--port N] [--forbidden START-END] [--quiet]");
                return;
            }
            _ => {}
        }
        i += 1;
    }

    let dev = Arc::new(Device {
        regs: Mutex::new(BTreeMap::new()),
        forbidden,
        requests: AtomicU64::new(0),
        connections: AtomicU64::new(0),
        traffic: AtomicU64::new(0),
        quiet,
    });

    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind stub port");
    if !quiet {
        println!("stub device listening on 127.0.0.1:{}", port);
    }
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let d = dev.clone();
                std::thread::spawn(move || serve(s, d));
            }
            Err(e) => {
                eprintln!("stub accept: {}", e);
                return;
            }
        }
    }
}
