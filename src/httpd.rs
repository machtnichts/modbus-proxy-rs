//! HTTP status/metrics endpoint. Same paths and payload shapes as the Python
//! implementation, because that is what the existing tooling curls.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use crate::json::Json;
use crate::proxy::Proxy;

pub fn handle_http(mut stream: TcpStream, proxy: &Proxy) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();

    let (ctype, body): (&str, String) = if path.starts_with("/metrics") {
        ("text/plain", proxy.metrics_text())
    } else if path.starts_with("/cache") {
        ("application/json", proxy.cache.summary().to_string())
    } else if path.starts_with("/ranges") {
        ("application/json", proxy.ranges_json().to_string())
    } else {
        let mut m = std::collections::BTreeMap::new();
        let healthy = proxy.stats.upstream_recently_ok(120);
        m.insert(
            "status".into(),
            Json::Str(if healthy { "ok" } else { "degraded" }.into()),
        );
        m.insert(
            "upstream".into(),
            Json::Str(format!("{}:{}", proxy.up.host, proxy.up.port)),
        );
        m.insert(
            "listen".into(),
            Json::Str(format!(
                "{}:{}",
                proxy.cfg.listen.host, proxy.cfg.listen.port
            )),
        );
        m.insert("stats".into(), proxy.stats.as_json());
        m.insert("cache_chunks".into(), Json::Num(proxy.cache.len() as f64));
        m.insert(
            "cache_registers".into(),
            Json::Num(proxy.cache.register_count() as f64),
        );
        ("application/json", Json::Obj(m).to_string())
    };

    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        ctype,
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// Accept loop for the status endpoint; returns when `stop` is set.
pub fn serve(listener: TcpListener, proxy: Arc<Proxy>, stop: Arc<std::sync::atomic::AtomicBool>) {
    let _ = listener.set_nonblocking(true);
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                handle_http(stream, &proxy);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                crate::logging::warn(format!("http accept: {}", e));
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
}
