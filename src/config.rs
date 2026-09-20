//! Typed configuration, mirroring the Python version's keys and defaults so the
//! same config.json drives either implementation.

use std::time::Duration;

use crate::json::Json;

#[derive(Debug, Clone)]
pub struct ListenCfg {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone)]
pub struct UpstreamCfg {
    pub host: String,
    pub port: u16,
    pub unit: u8,
    pub connect_timeout: Duration,
    pub response_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct HttpCfg {
    pub host: String,
    pub port: u16,
}

/// One register range the poller keeps hot.
#[derive(Debug, Clone)]
pub struct RangeCfg {
    pub name: String,
    pub address: u16,
    pub count: u16,
    /// SunSpec block header (DID, length) asserted before caching.
    pub expect_header: Option<(u16, u16)>,
    /// Offsets (from the range start) that must hold a small signed value.
    pub sf_offsets: Vec<usize>,
    /// Same, but with the battery-inverter acceptance band (-5.5..=4.5).
    pub sf18_offsets: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct PollCfg {
    pub interval_active: Duration,
    pub interval_idle: Duration,
    pub min_request_gap: Duration,
    pub ondemand_ttl: Duration,
    pub max_registers_per_read: u16,
    pub startup_delay: Duration,
    pub ranges: Vec<RangeCfg>,
}

#[derive(Debug, Clone)]
pub struct PolicyCfg {
    pub allow_writes: bool,
    pub log_every_request: bool,
    pub reject_unsupported_functions: bool,
}

#[derive(Debug, Clone)]
pub struct LogCfg {
    pub level: String,
    pub file: Option<String>,
    pub max_bytes: u64,
    pub backup_count: u32,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: ListenCfg,
    pub upstream: UpstreamCfg,
    pub http: Option<HttpCfg>,
    pub poll: PollCfg,
    pub policy: PolicyCfg,
    pub logging: LogCfg,
}

fn get_str(v: &Json, path: &[&str], default: &str) -> String {
    v.get_path(path)
        .and_then(|j| j.as_str())
        .unwrap_or(default)
        .to_string()
}

fn get_f64(v: &Json, path: &[&str], default: f64) -> f64 {
    v.get_path(path).and_then(|j| j.as_f64()).unwrap_or(default)
}

fn get_u64(v: &Json, path: &[&str], default: u64) -> u64 {
    let f = get_f64(v, path, default as f64);
    if f < 0.0 {
        default
    } else {
        f as u64
    }
}

fn get_bool(v: &Json, path: &[&str], default: bool) -> bool {
    v.get_path(path)
        .and_then(|j| j.as_bool())
        .unwrap_or(default)
}

fn json_u16_array(v: &Json, path: &[&str]) -> Vec<usize> {
    v.get_path(path)
        .and_then(|j| j.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_f64())
                .filter(|f| *f >= 0.0)
                .map(|f| f as usize)
                .collect()
        })
        .unwrap_or_default()
}

impl Config {
    pub fn from_json(v: &Json) -> Result<Config, String> {
        let listen = ListenCfg {
            host: get_str(v, &["listen", "host"], "0.0.0.0"),
            port: get_u64(v, &["listen", "port"], 1503) as u16,
        };

        let up_host = get_str(v, &["upstream", "host"], "");
        if up_host.is_empty() {
            return Err("upstream.host is required".into());
        }
        let upstream = UpstreamCfg {
            host: up_host,
            port: get_u64(v, &["upstream", "port"], 502) as u16,
            unit: get_u64(v, &["upstream", "unit"], 1) as u8,
            connect_timeout: Duration::from_secs_f64(get_f64(
                v,
                &["upstream", "connect_timeout"],
                8.0,
            )),
            response_timeout: Duration::from_secs_f64(get_f64(
                v,
                &["upstream", "response_timeout"],
                10.0,
            )),
        };

        // http is optional: a null value or a missing block disables it
        let http = match v.get("http") {
            Some(j) if !j.is_null() && j.get("port").is_some() => Some(HttpCfg {
                host: get_str(v, &["http", "host"], "0.0.0.0"),
                port: get_u64(v, &["http", "port"], 1504) as u16,
            }),
            _ => None,
        };

        let mut ranges = Vec::new();
        if let Some(arr) = v.get_path(&["poll", "ranges"]).and_then(|j| j.as_array()) {
            for item in arr {
                let address = item.get("address").and_then(|j| j.as_f64());
                let count = item.get("count").and_then(|j| j.as_f64());
                let (address, count) = match (address, count) {
                    (Some(a), Some(c)) if c > 0.0 => (a as u16, c as u16),
                    _ => {
                        return Err(format!(
                            "poll range needs numeric address and count: {}",
                            item.to_string()
                        ))
                    }
                };
                let expect_header = item
                    .get("expect_header")
                    .and_then(|j| j.as_array())
                    .filter(|a| a.len() >= 2)
                    .map(|a| {
                        (
                            a[0].as_f64().unwrap_or(0.0) as u16,
                            a[1].as_f64().unwrap_or(0.0) as u16,
                        )
                    });
                let name = item
                    .get("name")
                    .and_then(|j| j.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("0x{:04X}", address));
                ranges.push(RangeCfg {
                    name,
                    address,
                    count,
                    expect_header,
                    sf_offsets: json_u16_array(item, &["sf_offsets"]),
                    sf18_offsets: json_u16_array(item, &["sf18_offsets"]),
                });
            }
        }

        let poll = PollCfg {
            interval_active: Duration::from_secs_f64(get_f64(v, &["poll", "interval_active"], 5.0)),
            interval_idle: Duration::from_secs_f64(get_f64(v, &["poll", "interval_idle"], 30.0)),
            min_request_gap: Duration::from_secs_f64(get_f64(
                v,
                &["poll", "min_request_gap"],
                0.12,
            )),
            ondemand_ttl: Duration::from_secs_f64(get_f64(v, &["poll", "ondemand_ttl"], 2.0)),
            max_registers_per_read: get_u64(v, &["poll", "max_registers_per_read"], 100) as u16,
            startup_delay: Duration::from_secs_f64(get_f64(v, &["poll", "startup_delay"], 0.0)),
            ranges,
        };

        let policy = PolicyCfg {
            allow_writes: get_bool(v, &["policy", "allow_writes"], true),
            log_every_request: get_bool(v, &["policy", "log_every_request"], false),
            reject_unsupported_functions: get_bool(
                v,
                &["policy", "reject_unsupported_functions"],
                false,
            ),
        };

        let logging = LogCfg {
            level: get_str(v, &["logging", "level"], "INFO"),
            file: v
                .get_path(&["logging", "file"])
                .and_then(|j| j.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string()),
            max_bytes: get_u64(v, &["logging", "max_bytes"], 2_000_000),
            backup_count: get_u64(v, &["logging", "backup_count"], 3) as u32,
        };

        Ok(Config {
            listen,
            upstream,
            http,
            poll,
            policy,
            logging,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_conformance_test_config_shape() {
        // exactly the shape tools/test_protocol_conformance.py writes
        let raw = r#"{
            "upstream": {"host":"127.0.0.1","port":15020,"unit":1,
                         "connect_timeout":5.0,"response_timeout":5.0},
            "listen": {"host":"127.0.0.1","port":15030},
            "http": {"host":"127.0.0.1","port":15040},
            "poll": {"interval_active":2.0,"interval_idle":2.0,"min_request_gap":0.02,
                     "ondemand_ttl":2.0,"max_registers_per_read":125,
                     "ranges":[{"name":"stub-a","address":0,"count":40},
                               {"name":"stub-b","address":100,"count":20}]},
            "logging": {"level":"WARNING","file":null},
            "policy": {"allow_writes":true,"reject_unsupported_functions":false}
        }"#;
        let cfg = Config::from_json(&Json::parse(raw).unwrap()).unwrap();
        assert_eq!(cfg.listen.port, 15030);
        assert_eq!(cfg.upstream.port, 15020);
        assert_eq!(cfg.http.as_ref().unwrap().port, 15040);
        assert_eq!(cfg.poll.ranges.len(), 2);
        assert_eq!(cfg.poll.ranges[1].address, 100);
        assert_eq!(cfg.poll.max_registers_per_read, 125);
        assert!(cfg.logging.file.is_none());
        assert!(cfg.policy.allow_writes);
    }

    #[test]
    fn defaults_are_the_python_defaults() {
        let raw = r#"{"upstream":{"host":"10.0.0.1"},"listen":{"port":1503}}"#;
        let cfg = Config::from_json(&Json::parse(raw).unwrap()).unwrap();
        assert_eq!(cfg.upstream.port, 502);
        assert_eq!(cfg.upstream.unit, 1);
        assert_eq!(cfg.poll.interval_active.as_secs_f64(), 5.0);
        assert_eq!(cfg.poll.interval_idle.as_secs_f64(), 30.0);
        assert_eq!(cfg.poll.ondemand_ttl.as_secs_f64(), 2.0);
        assert_eq!(cfg.poll.max_registers_per_read, 100);
        assert!(cfg.http.is_none());
        assert_eq!(cfg.logging.level, "INFO");
        assert_eq!(cfg.logging.backup_count, 3);
    }

    #[test]
    fn missing_upstream_host_is_an_error() {
        assert!(Config::from_json(&Json::parse(r#"{"listen":{"port":1503}}"#).unwrap()).is_err());
    }
}
