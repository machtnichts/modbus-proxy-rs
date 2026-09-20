//! muxproxy (Rust) - caching, multiplexing Modbus/TCP proxy for a single-client
//! device.
//!
//! Behaviour-compatible with the Python implementation next to it
//! (`../modbus-proxy/muxproxy.py`): same config file, same CLI, same HTTP status
//! endpoints, same wire behaviour. The difference is that this builds to one
//! self-contained binary with no interpreter and no packages.
//!
//! Usage: muxproxy [-c config.json] [--listen-port N] [--http-port N] [--log-level L]

mod cache;
mod config;
mod httpd;
mod json;
mod logging;
mod modbus;
mod proxy;
mod stats;
mod upstream;

use std::net::TcpListener;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use config::Config;
use json::Json;
use proxy::Proxy;
use stats::Stats;

const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: i32) {
    STOP.store(true, Ordering::SeqCst);
}

/// SIGINT/SIGTERM without a crate: setting a flag in a handler is async-signal-safe.
fn install_signal_handlers() {
    unsafe {
        signal(SIGINT, on_signal as usize);
        signal(SIGTERM, on_signal as usize);
    }
}

struct Args {
    config: Option<String>,
    listen_port: Option<u16>,
    http_port: Option<u16>,
    log_level: Option<String>,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut args = Args {
        config: None,
        listen_port: None,
        http_port: None,
        log_level: None,
    };
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "-c" | "--config" => {
                i += 1;
                args.config = Some(argv.get(i).ok_or("--config needs a path")?.clone());
            }
            "--listen-port" => {
                i += 1;
                args.listen_port = Some(
                    argv.get(i)
                        .ok_or("--listen-port needs a number")?
                        .parse()
                        .map_err(|_| "--listen-port must be a number")?,
                );
            }
            "--http-port" => {
                i += 1;
                args.http_port = Some(
                    argv.get(i)
                        .ok_or("--http-port needs a number")?
                        .parse()
                        .map_err(|_| "--http-port must be a number")?,
                );
            }
            "--log-level" => {
                i += 1;
                args.log_level = Some(argv.get(i).ok_or("--log-level needs a value")?.clone());
            }
            "-h" | "--help" => {
                println!(
                    "muxproxy - caching multiplexing Modbus/TCP proxy\n\n\
                     USAGE:\n    muxproxy [-c config.json] [--listen-port N] [--http-port N] \
                     [--log-level LEVEL]\n\n\
                     The config file is the same one the Python implementation uses."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument '{}'", other)),
        }
        i += 1;
    }
    Ok(args)
}

fn default_config_path() -> String {
    // alongside the binary, like the Python version uses its own directory
    let mut p = std::env::current_exe().unwrap_or_else(|_| "muxproxy".into());
    p.pop();
    p.push("config.json");
    p.to_string_lossy().to_string()
}

fn run() -> Result<(), String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = parse_args(&argv)?;

    let path = args.config.unwrap_or_else(default_config_path);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read config '{}': {}", path, e))?;
    let parsed = Json::parse(&text).map_err(|e| format!("config '{}': {}", path, e))?;
    let mut cfg = Config::from_json(&parsed)?;

    if let Some(p) = args.listen_port {
        cfg.listen.port = p;
    }
    if let Some(p) = args.http_port {
        if let Some(h) = cfg.http.as_mut() {
            h.port = p;
        }
    }
    if let Some(l) = args.log_level {
        cfg.logging.level = l;
    }

    logging::init(&cfg.logging);
    logging::info(format!("muxproxy starting (pid {})", std::process::id()));
    logging::info(format!("config: {}", path));

    let stats = Arc::new(Stats::new());
    let stop = Arc::new(AtomicBool::new(false));
    let proxy = Arc::new(Proxy::new(cfg.clone(), stats.clone(), stop.clone()));

    // bind before starting threads so a port conflict fails fast and loudly
    let modbus_listener = TcpListener::bind((cfg.listen.host.as_str(), cfg.listen.port))
        .map_err(|e| format!("bind modbus {}:{}: {}", cfg.listen.host, cfg.listen.port, e))?;
    logging::info(format!(
        "modbus proxy listening on {}:{} -> upstream {}:{}",
        cfg.listen.host, cfg.listen.port, cfg.upstream.host, cfg.upstream.port
    ));

    if let Some(http) = cfg.http.clone() {
        let http_listener = TcpListener::bind((http.host.as_str(), http.port))
            .map_err(|e| format!("bind http {}:{}: {}", http.host, http.port, e))?;
        logging::info(format!(
            "status endpoint on http://{}:{}/",
            http.host, http.port
        ));
        let p = proxy.clone();
        let s = stop.clone();
        std::thread::spawn(move || httpd::serve(http_listener, p, s));
    }

    install_signal_handlers();

    {
        let p = proxy.clone();
        std::thread::spawn(move || p.poll_loop());
    }

    // main thread runs the accept loop; STOP is mirrored into the shared flag
    modbus_accept_loop(modbus_listener, proxy.clone(), stop.clone());

    logging::info("shutting down".to_string());
    Ok(())
}

fn modbus_accept_loop(listener: TcpListener, proxy: Arc<Proxy>, stop: Arc<AtomicBool>) {
    let _ = listener.set_nonblocking(true);
    while !(stop.load(Ordering::Relaxed) || STOP.load(Ordering::Relaxed)) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let p = proxy.clone();
                std::thread::spawn(move || p.handle_client(stream));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => {
                logging::warn(format!("accept: {}", e));
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("muxproxy: {}", e);
            ExitCode::from(2)
        }
    }
}
