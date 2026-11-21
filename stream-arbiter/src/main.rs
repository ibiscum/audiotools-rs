use anyhow::{Context, Result, anyhow};
use env_logger::{Builder, Env, Target};
use log::{debug, info, warn};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::env;
use std::io::Write;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
struct Config {
    snapcast_jsonrpc_url: String,
    poll_interval: Duration,
    min_switch_interval: Duration,
    stream_priority: Vec<String>,
    target_groups: Option<Vec<String>>,
    idle_fallback_stream: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ServerGetStatusResult {
    server: ServerStatus,
}

#[derive(Clone, Debug, Deserialize)]
struct ServerStatus {
    groups: Vec<Group>,
    streams: Vec<Stream>,
}

#[derive(Clone, Debug, Deserialize)]
struct Group {
    id: String,
    stream_id: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Stream {
    id: String,
    status: String,
    uri: StreamUri,
}

#[derive(Clone, Debug, Deserialize)]
struct StreamUri {
    #[serde(default)]
    query: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
struct JsonRpcRequest<'a> {
    id: u64,
    jsonrpc: &'a str,
    method: &'a str,
    params: Value,
}

#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    result: Option<Value>,
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

fn init_logging() {
    Builder::from_env(Env::default().default_filter_or("info"))
        .target(Target::Stdout)
        .format(|buf, record| {
            writeln!(
                buf,
                "{} [{:>5}] {}",
                buf.timestamp_millis(),
                record.level(),
                record.args()
            )
        })
        .init();
}

fn parse_env_duration(name: &str, default_secs: u64) -> Result<Duration> {
    match env::var(name) {
        Ok(v) => {
            let secs = v
                .trim()
                .parse::<u64>()
                .with_context(|| format!("Failed to parse {name}='{v}' as seconds"))?;
            Ok(Duration::from_secs(secs))
        }
        Err(_) => Ok(Duration::from_secs(default_secs)),
    }
}

fn parse_csv_env(name: &str, default: &[&str]) -> Vec<String> {
    match env::var(name) {
        Ok(v) => v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        Err(_) => default.iter().map(|s| (*s).to_owned()).collect(),
    }
}

fn parse_optional_csv_env(name: &str) -> Option<Vec<String>> {
    let Ok(v) = env::var(name) else {
        return None;
    };
    let groups: Vec<String> = v
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if groups.is_empty() { None } else { Some(groups) }
}

fn parse_config() -> Result<Config> {
    let snapcast_jsonrpc_url =
        env::var("SNAPCAST_JSONRPC_URL").unwrap_or_else(|_| "http://127.0.0.1:1780/jsonrpc".to_owned());
    let poll_interval = parse_env_duration("ARBITER_POLL_SECS", 2)?;
    let min_switch_interval = parse_env_duration("ARBITER_MIN_SWITCH_SECS", 5)?;
    let stream_priority = parse_csv_env("ARBITER_STREAM_PRIORITY", &["AirPlay", "MPD"]);
    if stream_priority.is_empty() {
        return Err(anyhow!("ARBITER_STREAM_PRIORITY resolved to an empty list"));
    }
    let target_groups = parse_optional_csv_env("ARBITER_TARGET_GROUPS");
    let idle_fallback_stream = env::var("ARBITER_IDLE_STREAM")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());

    Ok(Config {
        snapcast_jsonrpc_url,
        poll_interval,
        min_switch_interval,
        stream_priority,
        target_groups,
        idle_fallback_stream,
    })
}

fn rpc_call(client: &Client, url: &str, method: &str, params: Value, id: u64) -> Result<Value> {
    let req = JsonRpcRequest {
        id,
        jsonrpc: "2.0",
        method,
        params,
    };

    let response: JsonRpcResponse = client
        .post(url)
        .json(&req)
        .send()
        .with_context(|| format!("HTTP request to Snapcast failed for method {method}"))?
        .error_for_status()
        .with_context(|| format!("Snapcast returned HTTP error for method {method}"))?
        .json()
        .with_context(|| format!("Failed to decode Snapcast response for method {method}"))?;

    if let Some(error) = response.error {
        return Err(anyhow!(
            "Snapcast JSON-RPC error for {method}: {} ({})",
            error.message,
            error.code
        ));
    }

    response
        .result
        .ok_or_else(|| anyhow!("Snapcast JSON-RPC response for {method} has neither result nor error"))
}

fn get_status(client: &Client, cfg: &Config, request_id: u64) -> Result<ServerStatus> {
    let value = rpc_call(client, &cfg.snapcast_jsonrpc_url, "Server.GetStatus", json!({}), request_id)?;
    let parsed: ServerGetStatusResult =
        serde_json::from_value(json!({ "server": value["server"].clone() }))
            .context("Failed to parse Server.GetStatus result")?;
    Ok(parsed.server)
}

fn stream_name(stream: &Stream) -> &str {
    stream
        .uri
        .query
        .get("name")
        .map(String::as_str)
        .unwrap_or(stream.id.as_str())
}

fn is_stream_active(stream: &Stream) -> bool {
    let status = stream.status.trim().to_ascii_lowercase();
    !status.is_empty() && status != "idle" && status != "disabled"
}

fn pick_desired_stream_id(status: &ServerStatus, cfg: &Config) -> Option<String> {
    for wanted_name in &cfg.stream_priority {
        let wanted_lower = wanted_name.to_ascii_lowercase();
        if let Some(stream) = status.streams.iter().find(|s| {
            let stream_name_lower = stream_name(s).to_ascii_lowercase();
            stream_name_lower == wanted_lower && is_stream_active(s)
        }) {
            return Some(stream.id.clone());
        }
    }

    cfg.idle_fallback_stream.as_ref().and_then(|wanted| {
        let wanted_lower = wanted.to_ascii_lowercase();
        status.streams.iter().find_map(|s| {
            let name_match = stream_name(s).to_ascii_lowercase() == wanted_lower;
            let id_match = s.id.to_ascii_lowercase() == wanted_lower;
            if name_match || id_match {
                Some(s.id.clone())
            } else {
                None
            }
        })
    })
}

fn should_manage_group(group_id: &str, cfg: &Config) -> bool {
    match &cfg.target_groups {
        Some(allowed) => allowed.iter().any(|g| g == group_id),
        None => true,
    }
}

fn set_group_stream(client: &Client, cfg: &Config, group_id: &str, stream_id: &str, request_id: u64) -> Result<()> {
    let _ = rpc_call(
        client,
        &cfg.snapcast_jsonrpc_url,
        "Group.SetStream",
        json!({
            "id": group_id,
            "stream_id": stream_id
        }),
        request_id,
    )?;
    Ok(())
}

fn main() -> Result<()> {
    init_logging();
    let cfg = parse_config()?;

    info!(
        "Starting stream-arbiter: url='{}', poll={}s, min_switch={}s, priority={:?}, target_groups={:?}, idle_fallback={:?}",
        cfg.snapcast_jsonrpc_url,
        cfg.poll_interval.as_secs(),
        cfg.min_switch_interval.as_secs(),
        cfg.stream_priority,
        cfg.target_groups,
        cfg.idle_fallback_stream
    );

    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("Failed to build HTTP client")?;

    let mut request_id = 1_u64;
    let mut group_last_switch: HashMap<String, Instant> = HashMap::new();

    loop {
        let status = match get_status(&client, &cfg, request_id) {
            Ok(status) => status,
            Err(err) => {
                warn!("Failed to get Snapcast status: {err}");
                request_id = request_id.wrapping_add(1);
                thread::sleep(cfg.poll_interval);
                continue;
            }
        };
        request_id = request_id.wrapping_add(1);

        let desired_stream_id = pick_desired_stream_id(&status, &cfg);
        let Some(desired_stream_id) = desired_stream_id else {
            debug!("No active or fallback stream selected this cycle.");
            thread::sleep(cfg.poll_interval);
            continue;
        };

        for group in &status.groups {
            if !should_manage_group(&group.id, &cfg) {
                debug!("Skipping unmanaged group '{}'", group.id);
                continue;
            }
            if group.stream_id == desired_stream_id {
                continue;
            }

            if let Some(last_switch) = group_last_switch.get(&group.id)
                && last_switch.elapsed() < cfg.min_switch_interval
            {
                debug!(
                    "Skipping switch for group '{}' due to min switch interval ({}s)",
                    group.id,
                    cfg.min_switch_interval.as_secs()
                );
                continue;
            }

            match set_group_stream(&client, &cfg, &group.id, &desired_stream_id, request_id) {
                Ok(()) => {
                    request_id = request_id.wrapping_add(1);
                    group_last_switch.insert(group.id.clone(), Instant::now());
                    info!(
                        "Switched group '{}' from stream '{}' to '{}'",
                        group.id, group.stream_id, desired_stream_id
                    );
                }
                Err(err) => {
                    request_id = request_id.wrapping_add(1);
                    warn!(
                        "Failed to switch group '{}' from '{}' to '{}': {}",
                        group.id, group.stream_id, desired_stream_id, err
                    );
                }
            }
        }

        thread::sleep(cfg.poll_interval);
    }
}
