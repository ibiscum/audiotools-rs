use anyhow::{Context, Result, anyhow};
use env_logger::{Builder, Env, Target};
use log::{debug, info, warn};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Debug)]
struct Config {
    snapcast_jsonrpc_url: String,
    poll_interval: Duration,
    min_update_interval: Duration,
    target_groups: Option<Vec<String>>,
    mpd_host: String,
    mpd_port: u16,
    mpd_stream_name: String,
    airplay_stream_name: String,
    airplay_volume_file: Option<String>,
    airplay_file_max_age: Duration,
}

#[derive(Clone, Debug, Deserialize)]
struct ServerGetStatusResponse {
    server: ServerStatus,
}

#[derive(Clone, Debug, Deserialize)]
struct ServerStatus {
    streams: Vec<Stream>,
    groups: Vec<Group>,
}

#[derive(Clone, Debug, Deserialize)]
struct Stream {
    id: String,
    uri: StreamUri,
}

#[derive(Clone, Debug, Deserialize)]
struct StreamUri {
    #[serde(default)]
    query: HashMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
struct Group {
    id: String,
    stream_id: String,
    clients: Vec<GroupClient>,
}

#[derive(Clone, Debug, Deserialize)]
struct GroupClient {
    id: String,
    config: GroupClientConfig,
}

#[derive(Clone, Debug, Deserialize)]
struct GroupClientConfig {
    volume: GroupClientVolume,
}

#[derive(Clone, Debug, Deserialize)]
struct GroupClientVolume {
    percent: u8,
    muted: bool,
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

fn parse_optional_csv_env(name: &str) -> Option<Vec<String>> {
    let Ok(v) = env::var(name) else {
        return None;
    };
    let values: Vec<String> = v
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if values.is_empty() { None } else { Some(values) }
}

fn parse_config() -> Result<Config> {
    let snapcast_jsonrpc_url =
        env::var("SNAPCAST_JSONRPC_URL").unwrap_or_else(|_| "http://127.0.0.1:1780/jsonrpc".to_owned());
    let poll_interval = parse_env_duration("VOLUME_BRIDGE_POLL_SECS", 2)?;
    let min_update_interval = parse_env_duration("VOLUME_BRIDGE_MIN_UPDATE_SECS", 1)?;
    let target_groups = parse_optional_csv_env("VOLUME_BRIDGE_TARGET_GROUPS");
    let mpd_host = env::var("MPD_HOST").unwrap_or_else(|_| "127.0.0.1".to_owned());
    let mpd_port = env::var("MPD_PORT")
        .ok()
        .map(|v| v.trim().parse::<u16>())
        .transpose()
        .context("Failed to parse MPD_PORT as u16")?
        .unwrap_or(6600);
    let mpd_stream_name = env::var("VOLUME_BRIDGE_MPD_STREAM").unwrap_or_else(|_| "MPD".to_owned());
    let airplay_stream_name =
        env::var("VOLUME_BRIDGE_AIRPLAY_STREAM").unwrap_or_else(|_| "AirPlay".to_owned());
    let airplay_volume_file = env::var("AIRPLAY_VOLUME_FILE")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let airplay_file_max_age = parse_env_duration("AIRPLAY_VOLUME_MAX_AGE_SECS", 20)?;

    Ok(Config {
        snapcast_jsonrpc_url,
        poll_interval,
        min_update_interval,
        target_groups,
        mpd_host,
        mpd_port,
        mpd_stream_name,
        airplay_stream_name,
        airplay_volume_file,
        airplay_file_max_age,
    })
}

fn rpc_call(client: &Client, url: &str, method: &str, params: Value, id: u64) -> Result<Value> {
    let request = JsonRpcRequest {
        id,
        jsonrpc: "2.0",
        method,
        params,
    };

    let response: JsonRpcResponse = client
        .post(url)
        .json(&request)
        .send()
        .with_context(|| format!("HTTP request failed for method {method}"))?
        .error_for_status()
        .with_context(|| format!("HTTP status error for method {method}"))?
        .json()
        .with_context(|| format!("Failed to decode JSON-RPC response for method {method}"))?;

    if let Some(err) = response.error {
        return Err(anyhow!(
            "Snapcast JSON-RPC error for {method}: {} ({})",
            err.message,
            err.code
        ));
    }

    response
        .result
        .ok_or_else(|| anyhow!("Missing JSON-RPC result for method {method}"))
}

fn get_status(client: &Client, cfg: &Config, request_id: u64) -> Result<ServerStatus> {
    let value = rpc_call(client, &cfg.snapcast_jsonrpc_url, "Server.GetStatus", json!({}), request_id)?;
    let parsed: ServerGetStatusResponse =
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

fn should_manage_group(group_id: &str, cfg: &Config) -> bool {
    match &cfg.target_groups {
        Some(groups) => groups.iter().any(|g| g == group_id),
        None => true,
    }
}

fn fetch_mpd_volume_percent(cfg: &Config) -> Result<Option<u8>> {
    let endpoint = format!("{}:{}", cfg.mpd_host, cfg.mpd_port);
    let mut addrs = endpoint
        .to_socket_addrs()
        .with_context(|| format!("Failed to resolve MPD endpoint '{}'", endpoint))?;
    let Some(addr) = addrs.next() else {
        return Err(anyhow!("No socket address resolved for MPD endpoint '{}'", endpoint));
    };

    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .with_context(|| format!("Failed to connect to MPD at {}", endpoint))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .context("Failed to set MPD read timeout")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .context("Failed to set MPD write timeout")?;

    let mut reader = BufReader::new(stream.try_clone().context("Failed to clone MPD socket")?);
    let mut greeting = String::new();
    reader
        .read_line(&mut greeting)
        .context("Failed to read MPD greeting")?;
    if !greeting.starts_with("OK MPD") {
        return Err(anyhow!("Unexpected MPD greeting: '{}'", greeting.trim()));
    }

    stream
        .write_all(b"status\n")
        .context("Failed to send MPD status command")?;
    stream.flush().context("Failed to flush MPD command")?;

    let mut volume: Option<u8> = None;
    loop {
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .context("Failed to read MPD response line")?;
        if bytes == 0 {
            return Err(anyhow!("MPD closed connection while reading status"));
        }
        let line = line.trim_end();
        if line == "OK" {
            break;
        }
        if line.starts_with("ACK ") {
            return Err(anyhow!("MPD returned error: {line}"));
        }
        if let Some(value) = line.strip_prefix("volume: ") {
            let raw = value
                .trim()
                .parse::<i16>()
                .with_context(|| format!("Failed to parse MPD volume from '{line}'"))?;
            if raw >= 0 {
                let clamped = raw.clamp(0, 100) as u8;
                volume = Some(clamped);
            }
        }
    }

    Ok(volume)
}

fn parse_airplay_volume_percent(raw: &str) -> Option<u8> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let parsed = trimmed.parse::<f64>().ok()?;
    if (0.0..=100.0).contains(&parsed) {
        return Some(parsed.round().clamp(0.0, 100.0) as u8);
    }
    if parsed <= -144.0 {
        return Some(0);
    }
    if (-30.0..=0.0).contains(&parsed) {
        let p = ((parsed + 30.0) / 30.0) * 100.0;
        return Some(p.round().clamp(0.0, 100.0) as u8);
    }
    None
}

fn fetch_airplay_volume_percent(cfg: &Config) -> Result<Option<u8>> {
    let Some(path) = &cfg.airplay_volume_file else {
        return Ok(None);
    };
    let metadata = fs::metadata(path)
        .with_context(|| format!("Failed to stat AIRPLAY_VOLUME_FILE '{}'", path))?;
    let modified = metadata
        .modified()
        .with_context(|| format!("Failed to read mtime for AIRPLAY_VOLUME_FILE '{}'", path))?;
    let age = SystemTime::now()
        .duration_since(modified)
        .with_context(|| format!("AIRPLAY_VOLUME_FILE '{}' has modified time in the future", path))?;

    if age > cfg.airplay_file_max_age {
        debug!(
            "AIRPLAY_VOLUME_FILE '{}' is stale ({}s old, max {}s); ignoring",
            path,
            age.as_secs(),
            cfg.airplay_file_max_age.as_secs()
        );
        return Ok(None);
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read AIRPLAY_VOLUME_FILE '{}'", path))?;
    Ok(parse_airplay_volume_percent(&content))
}

fn set_client_volume(
    client: &Client,
    cfg: &Config,
    client_id: &str,
    percent: u8,
    request_id: u64,
) -> Result<()> {
    let muted = percent == 0;
    let _ = rpc_call(
        client,
        &cfg.snapcast_jsonrpc_url,
        "Client.SetVolume",
        json!({
            "id": client_id,
            "volume": {
                "muted": muted,
                "percent": percent
            }
        }),
        request_id,
    )?;
    Ok(())
}

fn main() -> Result<()> {
    init_logging();
    let cfg = parse_config()?;

    info!(
        "Starting volume-arbiter: url='{}', poll={}s, min_update={}s, target_groups={:?}, mpd_stream='{}', airplay_stream='{}', airplay_file={:?}",
        cfg.snapcast_jsonrpc_url,
        cfg.poll_interval.as_secs(),
        cfg.min_update_interval.as_secs(),
        cfg.target_groups,
        cfg.mpd_stream_name,
        cfg.airplay_stream_name,
        cfg.airplay_volume_file
    );

    let http = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("Failed to build HTTP client")?;

    let mut request_id = 1_u64;
    let mut last_client_update: HashMap<String, Instant> = HashMap::new();

    loop {
        let status = match get_status(&http, &cfg, request_id) {
            Ok(status) => status,
            Err(err) => {
                warn!("Failed to fetch Snapcast status: {err}");
                request_id = request_id.wrapping_add(1);
                thread::sleep(cfg.poll_interval);
                continue;
            }
        };
        request_id = request_id.wrapping_add(1);

        let stream_names: HashMap<&str, &str> = status
            .streams
            .iter()
            .map(|stream| (stream.id.as_str(), stream_name(stream)))
            .collect();

        let mpd_volume = match fetch_mpd_volume_percent(&cfg) {
            Ok(v) => v,
            Err(err) => {
                warn!("Failed to read MPD volume: {err}");
                None
            }
        };
        let airplay_volume = match fetch_airplay_volume_percent(&cfg) {
            Ok(v) => v,
            Err(err) => {
                warn!("Failed to read AirPlay volume file: {err}");
                None
            }
        };

        for group in &status.groups {
            if !should_manage_group(&group.id, &cfg) {
                continue;
            }

            let Some(stream_name) = stream_names.get(group.stream_id.as_str()) else {
                debug!(
                    "Skipping group '{}' because stream id '{}' is unknown",
                    group.id, group.stream_id
                );
                continue;
            };

            let desired_volume = if stream_name.eq_ignore_ascii_case(&cfg.mpd_stream_name) {
                mpd_volume
            } else if stream_name.eq_ignore_ascii_case(&cfg.airplay_stream_name) {
                airplay_volume
            } else {
                None
            };

            let Some(desired_volume) = desired_volume else {
                continue;
            };

            for client_status in &group.clients {
                let current_percent = client_status.config.volume.percent;
                let current_muted = client_status.config.volume.muted;
                let desired_muted = desired_volume == 0;
                if current_percent == desired_volume && current_muted == desired_muted {
                    continue;
                }

                if let Some(last_update) = last_client_update.get(&client_status.id)
                    && last_update.elapsed() < cfg.min_update_interval
                {
                    continue;
                }

                match set_client_volume(&http, &cfg, &client_status.id, desired_volume, request_id) {
                    Ok(()) => {
                        request_id = request_id.wrapping_add(1);
                        last_client_update.insert(client_status.id.clone(), Instant::now());
                        info!(
                            "Set client '{}' volume to {}% for group '{}' (stream '{}')",
                            client_status.id, desired_volume, group.id, stream_name
                        );
                    }
                    Err(err) => {
                        request_id = request_id.wrapping_add(1);
                        warn!(
                            "Failed to set client '{}' volume to {}%: {}",
                            client_status.id, desired_volume, err
                        );
                    }
                }
            }
        }

        thread::sleep(cfg.poll_interval);
    }
}
