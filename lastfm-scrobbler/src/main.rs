use base64::{Engine as _, engine::general_purpose::STANDARD};
use dbus::arg::{PropMap, RefArg};
use dbus::blocking::Connection as DbusConnection;
use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
use quick_xml::Reader;
use quick_xml::events::Event;
use rusqlite::{Connection as SqliteConnection, params};
use rustfm_scrobble::{Scrobble, ScrobbleBatch, Scrobbler};
use std::cmp;
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Default, Clone, Debug, PartialEq)]
struct TrackInfo {
    title: String,
    artist: String,
    album: String,
}

struct PlayerState {
    pending: TrackInfo,
    current: TrackInfo,
    pending_duration: Option<Duration>,
    current_duration: Option<Duration>,
    segment_started_at: Option<Instant>,
    played_duration: Duration,
    track_started_unix: Option<u64>,
    scrobbled: bool,
    is_playing: bool,
}

struct MetadataItem {
    item_type: String,
    item_code: String,
    decoded_text: String,
    decoded_bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
enum InputBackend {
    Pipe,
    Dbus,
    Mpris,
}

struct PlaybackSnapshot {
    title: String,
    artist: String,
    album: String,
    duration: Option<Duration>,
    is_playing: bool,
}

#[derive(Clone)]
struct LastfmCredentials {
    username: String,
    password: String,
}

#[derive(Clone, Debug)]
struct PendingScrobble {
    artist: String,
    title: String,
    album: String,
    played_at_unix: u64,
}

#[derive(Debug)]
struct QueuedScrobble {
    id: i64,
    artist: String,
    title: String,
    album: String,
    played_at_unix: u64,
}

#[derive(Clone, Copy)]
enum DurationUnit {
    Millis,
    Micros,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            pending: TrackInfo::default(),
            current: TrackInfo::default(),
            pending_duration: None,
            current_duration: None,
            segment_started_at: None,
            played_duration: Duration::ZERO,
            track_started_unix: None,
            scrobbled: false,
            is_playing: true,
        }
    }
}

fn main() {
    let input_backend = parse_input_backend();
    let pipe_path =
        env::var("SHAIRPORT_PIPE").unwrap_or_else(|_| "/tmp/shairport-sync-metadata".to_string());
    let queue_db_path = env::var("LASTFM_QUEUE_DB")
        .unwrap_or_else(|_| "/var/lib/shairport-scrobbler/scrobble-queue.sqlite3".to_string());
    let offline_mode = env::var("LASTFM_OFFLINE_MODE")
        .ok()
        .is_some_and(|value| is_truthy_flag(&value));

    let mut credentials: Option<LastfmCredentials> = None;
    let scrobbler = if offline_mode {
        println!("Last.fm offline mode enabled; skipping authentication and API calls.");
        None
    } else {
        let api_key = env::var("LASTFM_API_KEY").expect("LASTFM_API_KEY is missing");
        let api_secret = env::var("LASTFM_API_SECRET").expect("LASTFM_API_SECRET is missing");
        let username = env::var("LASTFM_USERNAME").expect("LASTFM_USERNAME is missing");
        let password = env::var("LASTFM_PASSWORD").expect("LASTFM_PASSWORD is missing");
        credentials = Some(LastfmCredentials {
            username: username.clone(),
            password: password.clone(),
        });

        let mut scrobbler = Scrobbler::new(&api_key, &api_secret);
        scrobbler
            .authenticate_with_password(&username, &password)
            .expect("Last.fm authentication failed");
        println!("Successfully authenticated with Last.fm.");
        Some(Arc::new(Mutex::new(scrobbler)))
    };

    let state = Arc::new(Mutex::new(PlayerState::default()));

    let bg_state = Arc::clone(&state);
    let bg_scrobbler = scrobbler.clone();
    let bg_credentials = credentials.clone();
    let mut queue_conn = if offline_mode {
        None
    } else {
        Some(
            init_queue(&queue_db_path)
                .unwrap_or_else(|err| panic!("Failed to initialize scrobble queue: {}", err)),
        )
    };
    thread::spawn(move || {
        let mut last_queue_retry = Instant::now();

        loop {
            thread::sleep(Duration::from_secs(1));

            if let (Some(scrobbler), Some(conn)) = (&bg_scrobbler, queue_conn.as_mut()) {
                let has_backlog = queue_has_items(conn).unwrap_or(false);
                if has_backlog && last_queue_retry.elapsed() >= Duration::from_secs(15) {
                    if let Ok(mut scr) = scrobbler.lock()
                        && let Err(err) = retry_queued_scrobbles(conn, &mut scr, bg_credentials.as_ref())
                    {
                        eprintln!("Queue retry error: {}", err);
                    }
                    last_queue_retry = Instant::now();
                }
            }

            let pending_scrobble = {
                let mut st = bg_state.lock().unwrap();
                maybe_ready_for_scrobble(&mut st)
            };

            let Some(pending_scrobble) = pending_scrobble else {
                continue;
            };

            let mut scrobble = Scrobble::new(
                &pending_scrobble.artist,
                &pending_scrobble.title,
                &pending_scrobble.album,
            );
            scrobble.with_timestamp(pending_scrobble.played_at_unix);

            if let Some(scrobbler) = &bg_scrobbler {
                if let Some(conn) = queue_conn.as_mut()
                    && queue_has_items(conn).unwrap_or(false)
                {
                    if let Err(queue_err) = enqueue_failed_scrobble(
                        conn,
                        &pending_scrobble,
                        "queued_due_to_backlog",
                    ) {
                        eprintln!("Failed to persist queued scrobble: {}", queue_err);
                    }
                    continue;
                }

                if let Ok(mut scr) = scrobbler.lock() {
                    match scr.scrobble(&scrobble) {
                        Ok(_) => println!(
                            "Scrobbled: {} - {}",
                            pending_scrobble.artist, pending_scrobble.title
                        ),
                        Err(e) => {
                            let mut error_text = e.to_string();
                            if should_reauthenticate(&error_text)
                                && let Some(creds) = bg_credentials.as_ref()
                                && reauthenticate_scrobbler(&mut scr, creds)
                            {
                                match scr.scrobble(&scrobble) {
                                    Ok(_) => {
                                        println!(
                                            "Scrobbled after re-auth: {} - {}",
                                            pending_scrobble.artist, pending_scrobble.title
                                        );
                                        continue;
                                    }
                                    Err(retry_err) => {
                                        error_text = retry_err.to_string();
                                    }
                                }
                            }

                            let is_retriable = is_retriable_scrobble_error(&error_text);
                            eprintln!(
                                "Scrobble error: {}; queueing decision for {} - {}",
                                error_text, pending_scrobble.artist, pending_scrobble.title
                            );
                            if is_retriable {
                                if let Some(conn) = queue_conn.as_mut()
                                    && let Err(queue_err) = enqueue_failed_scrobble(
                                        conn,
                                        &pending_scrobble,
                                        &error_text,
                                    )
                                {
                                    eprintln!("Failed to persist queued scrobble: {}", queue_err);
                                }
                            } else {
                                eprintln!(
                                    "Not retrying non-retriable scrobble failure for {} - {}: {}",
                                    pending_scrobble.artist, pending_scrobble.title, error_text
                                );
                            }
                        }
                    }
                }
            } else {
                println!(
                    "Offline mode: skipping scrobble for {} - {}",
                    pending_scrobble.artist, pending_scrobble.title
                );
            }
        }
    });

    match input_backend {
        InputBackend::Pipe => run_pipe_input_loop(&pipe_path, &state, scrobbler.as_ref()),
        InputBackend::Dbus => run_native_dbus_input_loop(&state, scrobbler.as_ref()),
        InputBackend::Mpris => run_mpris_input_loop(&state, scrobbler.as_ref()),
    }
}

fn parse_input_backend() -> InputBackend {
    let input_backend = env::var("LASTFM_INPUT_BACKEND").unwrap_or_else(|_| "pipe".to_string());
    match input_backend.trim().to_ascii_lowercase().as_str() {
        "pipe" => InputBackend::Pipe,
        "dbus" => InputBackend::Dbus,
        "mpris" => InputBackend::Mpris,
        other => panic!(
            "Unsupported LASTFM_INPUT_BACKEND value: {}. Use one of: pipe, dbus, mpris.",
            other
        ),
    }
}

fn run_pipe_input_loop(
    pipe_path: &str,
    state: &Arc<Mutex<PlayerState>>,
    scrobbler: Option<&Arc<Mutex<Scrobbler>>>,
) -> ! {
    loop {
        println!("Opening pipe: {}", pipe_path);
        let file = match File::open(pipe_path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Waiting for pipe ({}): {}", pipe_path, e);
                thread::sleep(Duration::from_secs(3));
                continue;
            }
        };

        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut item_buffer = String::new();

        while let Ok(bytes_read) = reader.read_line(&mut line) {
            if bytes_read == 0 {
                break;
            }
            item_buffer.push_str(&line);

            if line.trim_end().ends_with("</item>") {
                process_xml_item(&item_buffer, state, scrobbler);
                item_buffer.clear();
            }
            line.clear();
        }
    }
}

fn run_mpris_input_loop(
    state: &Arc<Mutex<PlayerState>>,
    scrobbler: Option<&Arc<Mutex<Scrobbler>>>,
) -> ! {
    let bus = env::var("MPRIS_BUS").unwrap_or_else(|_| "system".to_string());
    let nametag = env::var("SHAIRPORT_DBUS_NAMETAG").ok();
    let default_service = match nametag {
        Some(tag) if !tag.trim().is_empty() => format!("org.mpris.MediaPlayer2.ShairportSync.{tag}"),
        _ => "org.mpris.MediaPlayer2.ShairportSync".to_string(),
    };
    let service = env::var("MPRIS_SERVICE").unwrap_or(default_service);
    let path = env::var("MPRIS_PATH").unwrap_or_else(|_| "/org/mpris/MediaPlayer2".to_string());
    let conn = open_dbus_connection(&bus);

    println!(
        "Using MPRIS backend on {} bus: service={}, path={}",
        bus, service, path
    );

    loop {
        match read_mpris_snapshot(&conn, &service, &path) {
            Ok(snapshot) => apply_playback_snapshot(&snapshot, state, scrobbler),
            Err(err) => eprintln!("MPRIS read error: {}", err),
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn run_native_dbus_input_loop(
    state: &Arc<Mutex<PlayerState>>,
    scrobbler: Option<&Arc<Mutex<Scrobbler>>>,
) -> ! {
    let bus = env::var("SHAIRPORT_DBUS_BUS").unwrap_or_else(|_| "system".to_string());
    let nametag = env::var("SHAIRPORT_DBUS_NAMETAG").ok();
    let default_service = match nametag {
        Some(tag) if !tag.trim().is_empty() => format!("org.gnome.ShairportSync.{tag}"),
        _ => "org.gnome.ShairportSync".to_string(),
    };
    let service = env::var("SHAIRPORT_DBUS_SERVICE").unwrap_or(default_service);
    let path = env::var("SHAIRPORT_DBUS_PATH").unwrap_or_else(|_| "/org/gnome/ShairportSync".to_string());
    let conn = open_dbus_connection(&bus);

    println!(
        "Using native DBus backend on {} bus: service={}, path={}",
        bus, service, path
    );

    loop {
        match read_native_dbus_snapshot(&conn, &service, &path) {
            Ok(snapshot) => apply_playback_snapshot(&snapshot, state, scrobbler),
            Err(err) => eprintln!("DBus read error: {}", err),
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn open_dbus_connection(bus: &str) -> DbusConnection {
    match bus.trim().to_ascii_lowercase().as_str() {
        "session" => DbusConnection::new_session().expect("Failed to connect to session DBus"),
        "system" => DbusConnection::new_system().expect("Failed to connect to system DBus"),
        other => panic!("Unsupported bus '{}'; use 'system' or 'session'.", other),
    }
}

fn read_mpris_snapshot(
    conn: &DbusConnection,
    service: &str,
    path: &str,
) -> Result<PlaybackSnapshot, dbus::Error> {
    let proxy = conn.with_proxy(service, path, Duration::from_secs(2));
    let status: String = proxy.get("org.mpris.MediaPlayer2.Player", "PlaybackStatus")?;
    let metadata: PropMap = proxy.get("org.mpris.MediaPlayer2.Player", "Metadata")?;

    Ok(PlaybackSnapshot {
        title: propmap_first_string(&metadata, &["xesam:title", "title"]).unwrap_or_default(),
        artist: propmap_first_string(&metadata, &["xesam:artist", "artist"]).unwrap_or_default(),
        album: propmap_first_string(&metadata, &["xesam:album", "album"]).unwrap_or_default(),
        duration: propmap_duration(
            &metadata,
            &[("mpris:length", DurationUnit::Micros), ("duration", DurationUnit::Millis)],
        ),
        is_playing: is_playing_status(&status),
    })
}

fn read_native_dbus_snapshot(
    conn: &DbusConnection,
    service: &str,
    path: &str,
) -> Result<PlaybackSnapshot, dbus::Error> {
    let proxy = conn.with_proxy(service, path, Duration::from_secs(2));
    let status: String = proxy.get("org.gnome.ShairportSync.RemoteControl", "PlayerState")?;
    let metadata: PropMap = proxy.get("org.gnome.ShairportSync.RemoteControl", "Metadata")?;

    Ok(PlaybackSnapshot {
        title: propmap_first_string(&metadata, &["xesam:title", "itemname", "title", "minm"])
            .unwrap_or_default(),
        artist: propmap_first_string(&metadata, &["xesam:artist", "artist", "asar"])
            .unwrap_or_default(),
        album: propmap_first_string(&metadata, &["xesam:album", "album", "asal"])
            .unwrap_or_default(),
        duration: propmap_duration(
            &metadata,
            &[
                ("mpris:length", DurationUnit::Micros),
                ("songtime", DurationUnit::Millis),
                ("astm", DurationUnit::Millis),
                ("duration", DurationUnit::Millis),
            ],
        ),
        is_playing: is_playing_status(&status),
    })
}

fn apply_playback_snapshot(
    snapshot: &PlaybackSnapshot,
    state: &Arc<Mutex<PlayerState>>,
    scrobbler: Option<&Arc<Mutex<Scrobbler>>>,
) {
    let mut st = state.lock().unwrap();
    if apply_direct_playback_snapshot(snapshot, &mut st) {
        println!(
            "Now Playing: {} - {} ({})",
            st.current.artist, st.current.title, st.current.album
        );
        if let Some(scrobbler) = scrobbler {
            let track = Scrobble::new(&st.current.artist, &st.current.title, &st.current.album);
            if let Ok(scr) = scrobbler.lock() {
                let _ = scr.now_playing(&track);
            }
        } else {
            println!("Offline mode: skipping Last.fm now playing update.");
        }
    }
}

fn process_xml_item(
    xml: &str,
    state: &Arc<Mutex<PlayerState>>,
    scrobbler: Option<&Arc<Mutex<Scrobbler>>>,
) {
    let Some(item) = parse_xml_item(xml) else {
        return;
    };

    let mut st = state.lock().unwrap();
    if apply_metadata_item(&item, &mut st) {
        println!(
            "Now Playing: {} - {} ({})",
            st.current.artist, st.current.title, st.current.album
        );
        if let Some(scrobbler) = scrobbler {
            let track = Scrobble::new(&st.current.artist, &st.current.title, &st.current.album);
            if let Ok(scr) = scrobbler.lock() {
                let _ = scr.now_playing(&track);
            }
        } else {
            println!("Offline mode: skipping Last.fm now playing update.");
        }
    }
}

fn propmap_first_string(map: &PropMap, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| map.get(*key).and_then(|value| refarg_to_string(value.0.as_ref())))
}

fn propmap_duration(map: &PropMap, keys: &[(&str, DurationUnit)]) -> Option<Duration> {
    keys.iter().find_map(|(key, unit)| {
        let value = map.get(*key)?;
        let raw = refarg_to_i64(value.0.as_ref())?;
        if raw < 0 {
            return None;
        }
        match unit {
            DurationUnit::Millis => Some(Duration::from_millis(raw as u64)),
            DurationUnit::Micros => Some(Duration::from_micros(raw as u64)),
        }
    })
}

fn refarg_to_string(arg: &dyn RefArg) -> Option<String> {
    if let Some(text) = arg.as_str() {
        return Some(text.to_string());
    }

    let mut iter = arg.as_iter()?;
    iter.find_map(|entry| entry.as_str().map(|text| text.to_string()))
}

fn refarg_to_i64(arg: &dyn RefArg) -> Option<i64> {
    arg.as_i64()
        .or_else(|| arg.as_u64().map(|value| value as i64))
}

fn is_playing_status(status: &str) -> bool {
    status.trim().eq_ignore_ascii_case("playing")
        || status.trim().eq_ignore_ascii_case("play")
}

fn apply_direct_playback_snapshot(snapshot: &PlaybackSnapshot, st: &mut PlayerState) -> bool {
    if !snapshot.title.is_empty() && !snapshot.artist.is_empty() {
        let next_track = TrackInfo {
            title: snapshot.title.clone(),
            artist: snapshot.artist.clone(),
            album: snapshot.album.clone(),
        };

        if next_track != st.current {
            st.current = next_track;
            st.current_duration = snapshot.duration;
            st.played_duration = Duration::ZERO;
            st.track_started_unix = Some(current_unix_timestamp());
            st.segment_started_at = if snapshot.is_playing {
                Some(Instant::now())
            } else {
                None
            };
            st.scrobbled = false;
            st.is_playing = snapshot.is_playing;
            return true;
        }
    }

    if snapshot.duration.is_some() {
        st.current_duration = snapshot.duration;
    }
    set_playing_state(st, snapshot.is_playing);
    false
}

fn maybe_ready_for_scrobble(st: &mut PlayerState) -> Option<PendingScrobble> {
    if st.scrobbled || st.current.title.is_empty() || st.current.artist.is_empty() {
        return None;
    }

    let Some(threshold) = required_scrobble_play_time(st.current_duration) else {
        st.scrobbled = true;
        println!(
            "Skipping scrobble (duration <= 30s): {} - {}",
            st.current.artist, st.current.title
        );
        return None;
    };

    let played = effective_played_duration(st);
    if played < threshold {
        return None;
    }

    st.scrobbled = true;
    Some(PendingScrobble {
        artist: st.current.artist.clone(),
        title: st.current.title.clone(),
        album: st.current.album.clone(),
        played_at_unix: st.track_started_unix.unwrap_or_else(current_unix_timestamp),
    })
}

fn required_scrobble_play_time(track_duration: Option<Duration>) -> Option<Duration> {
    match track_duration {
        Some(duration) if duration <= Duration::from_secs(30) => None,
        Some(duration) => Some(cmp::min(
            Duration::from_millis((duration.as_millis() / 2) as u64),
            Duration::from_secs(240),
        )),
        None => Some(Duration::from_secs(30)),
    }
}

fn effective_played_duration(st: &PlayerState) -> Duration {
    if st.is_playing && let Some(segment_started_at) = st.segment_started_at {
        return st.played_duration + segment_started_at.elapsed();
    }
    st.played_duration
}

fn set_playing_state(st: &mut PlayerState, is_playing: bool) {
    if st.is_playing && !is_playing
        && let Some(segment_started_at) = st.segment_started_at.take()
    {
        st.played_duration += segment_started_at.elapsed();
    } else if !st.is_playing && is_playing {
        st.segment_started_at = Some(Instant::now());
    }
    st.is_playing = is_playing;
}

fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn decode_duration_ms(bytes: &[u8]) -> Option<Duration> {
    if bytes.len() < 4 {
        return None;
    }
    let millis = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64;
    Some(Duration::from_millis(millis))
}

fn is_truthy_flag(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Converts Shairport Sync's 8-digit hex code into a 4-character string (e.g. "6d696e6d" -> "minm").
fn hex_to_fourcc(hex_str: &str) -> Option<String> {
    if hex_str.len() != 8 {
        return None;
    }
    let bytes: Result<Vec<u8>, _> = (0..8)
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex_str[i..i + 2], 16))
        .collect();
    bytes.ok().and_then(|b| String::from_utf8(b).ok())
}

fn parse_xml_item(xml: &str) -> Option<MetadataItem> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut current_tag = String::new();
    let mut item_type = String::new();
    let mut item_code = String::new();
    let mut data_b64 = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                current_tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().to_string();
                match current_tag.as_str() {
                    "type" => item_type = text,
                    "code" => item_code = text,
                    "data" => data_b64.push_str(&text),
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                if e.name().as_ref() == b"item" {
                    break;
                }
                current_tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
    }

    let item_type = hex_to_fourcc(&item_type)?;
    let item_code = hex_to_fourcc(&item_code)?;

    let decoded_bytes = if data_b64.is_empty() {
        Vec::new()
    } else {
        STANDARD.decode(data_b64.trim()).ok().unwrap_or_default()
    };
    let decoded_text = String::from_utf8(decoded_bytes.clone()).ok().unwrap_or_default();

    Some(MetadataItem {
        item_type,
        item_code,
        decoded_text,
        decoded_bytes,
    })
}

fn apply_metadata_item(item: &MetadataItem, st: &mut PlayerState) -> bool {
    match (item.item_type.as_str(), item.item_code.as_str()) {
        ("core", "minm") => st.pending.title = item.decoded_text.clone(),
        ("core", "asar") => st.pending.artist = item.decoded_text.clone(),
        ("core", "asal") => st.pending.album = item.decoded_text.clone(),
        ("core", "astm") => st.pending_duration = decode_duration_ms(&item.decoded_bytes),
        ("ssnc", "mdst") => {
            st.pending = TrackInfo::default();
            st.pending_duration = None;
        }
        ("ssnc", "mden") => {
            if !st.pending.title.is_empty() && st.pending != st.current {
                st.current = st.pending.clone();
                st.current_duration = st.pending_duration;
                st.segment_started_at = Some(Instant::now());
                st.played_duration = Duration::ZERO;
                st.track_started_unix = Some(current_unix_timestamp());
                st.scrobbled = false;
                st.is_playing = true;
                return true;
            }
        }
        ("ssnc", "pend") | ("ssnc", "pfls") => {
            set_playing_state(st, false);
        }
        ("ssnc", "prsm") | ("ssnc", "pbeg") => {
            set_playing_state(st, true);
        }
        _ => {}
    }

    false
}

fn init_queue(path: &str) -> Result<SqliteConnection, String> {
    let queue_path = Path::new(path);
    if let Some(parent) = queue_path.parent() && !parent.as_os_str().is_empty() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let conn = SqliteConnection::open(queue_path).map_err(|e| e.to_string())?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS scrobble_queue (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            artist TEXT NOT NULL,
            title TEXT NOT NULL,
            album TEXT NOT NULL,
            played_at_unix INTEGER NOT NULL,
            retry_count INTEGER NOT NULL DEFAULT 0,
            last_error TEXT,
            created_at_unix INTEGER NOT NULL
        )",
        [],
    )
    .map_err(|e| e.to_string())?;

    Ok(conn)
}

fn enqueue_failed_scrobble(
    conn: &mut SqliteConnection,
    track: &PendingScrobble,
    error: &str,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO scrobble_queue
            (artist, title, album, played_at_unix, retry_count, last_error, created_at_unix)
         VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)",
        params![
            track.artist,
            track.title,
            track.album,
            track.played_at_unix as i64,
            error,
            current_unix_timestamp() as i64
        ],
    )?;
    Ok(())
}

fn queue_has_items(conn: &SqliteConnection) -> Result<bool, rusqlite::Error> {
    let count: i64 = conn.query_row("SELECT COUNT(1) FROM scrobble_queue", [], |row| row.get(0))?;
    Ok(count > 0)
}

fn pop_queued_scrobbles(
    conn: &mut SqliteConnection,
    limit: i64,
) -> Result<Vec<QueuedScrobble>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, artist, title, album, played_at_unix
         FROM scrobble_queue
         ORDER BY id ASC
         LIMIT ?1",
    )?;
    let queued_rows = stmt.query_map(params![limit], |row| {
        Ok(QueuedScrobble {
            id: row.get(0)?,
            artist: row.get(1)?,
            title: row.get(2)?,
            album: row.get(3)?,
            played_at_unix: row.get::<_, i64>(4)? as u64,
        })
    })?;

    queued_rows.collect::<Result<Vec<_>, rusqlite::Error>>()
}

fn is_retriable_scrobble_error(error_text: &str) -> bool {
    let lowered = error_text.to_ascii_lowercase();
    lowered.contains("service offline")
        || lowered.contains("temporarily unavailable")
        || lowered.contains("connection")
        || lowered.contains("timeout")
        || lowered.contains("timed out")
        || lowered.contains("non success status (500")
        || lowered.contains("non success status (502")
        || lowered.contains("non success status (503")
        || lowered.contains("non success status (504")
}

fn should_reauthenticate(error_text: &str) -> bool {
    let lowered = error_text.to_ascii_lowercase();
    lowered.contains("invalid session key")
        || lowered.contains("code=\"9\"")
        || lowered.contains("error code 9")
}

fn reauthenticate_scrobbler(scrobbler: &mut Scrobbler, creds: &LastfmCredentials) -> bool {
    match scrobbler.authenticate_with_password(&creds.username, &creds.password) {
        Ok(_) => {
            println!("Re-authenticated Last.fm session.");
            true
        }
        Err(err) => {
            eprintln!("Failed to re-authenticate Last.fm session: {}", err);
            false
        }
    }
}

fn retry_queued_scrobbles(
    conn: &mut SqliteConnection,
    scrobbler: &mut Scrobbler,
    credentials: Option<&LastfmCredentials>,
) -> Result<(), rusqlite::Error> {
    let queued_scrobbles = pop_queued_scrobbles(conn, 50)?;
    if queued_scrobbles.is_empty() {
        return Ok(());
    }

    let mut batch_tracks = Vec::with_capacity(queued_scrobbles.len());
    for queued in &queued_scrobbles {
        let mut scrobble = Scrobble::new(&queued.artist, &queued.title, &queued.album);
        scrobble.with_timestamp(queued.played_at_unix);
        batch_tracks.push(scrobble);
    }
    let batch = ScrobbleBatch::from(batch_tracks);

    match scrobbler.scrobble_batch(&batch) {
        Ok(_) => {
            let tx = conn.transaction()?;
            for queued in &queued_scrobbles {
                tx.execute("DELETE FROM scrobble_queue WHERE id = ?1", params![queued.id])?;
            }
            tx.commit()?;
            println!("Retried queued scrobbles batch: {}", queued_scrobbles.len());
            return Ok(());
        }
        Err(err) => {
            let error_text = err.to_string();
            if should_reauthenticate(&error_text)
                && let Some(creds) = credentials
                && reauthenticate_scrobbler(scrobbler, creds)
            {
                match scrobbler.scrobble_batch(&batch) {
                    Ok(_) => {
                        let tx = conn.transaction()?;
                        for queued in &queued_scrobbles {
                            tx.execute("DELETE FROM scrobble_queue WHERE id = ?1", params![queued.id])?;
                        }
                        tx.commit()?;
                        println!(
                            "Retried queued scrobbles batch after re-auth: {}",
                            queued_scrobbles.len()
                        );
                        return Ok(());
                    }
                    Err(retry_err) => {
                        let retry_error = retry_err.to_string();
                        if is_retriable_scrobble_error(&retry_error) {
                            let tx = conn.transaction()?;
                            for queued in &queued_scrobbles {
                                tx.execute(
                                    "UPDATE scrobble_queue
                                     SET retry_count = retry_count + 1, last_error = ?1
                                     WHERE id = ?2",
                                    params![retry_error.as_str(), queued.id],
                                )?;
                            }
                            tx.commit()?;
                            return Ok(());
                        }
                    }
                }
            }

            if is_retriable_scrobble_error(&error_text) {
                let tx = conn.transaction()?;
                for queued in &queued_scrobbles {
                    tx.execute(
                        "UPDATE scrobble_queue
                         SET retry_count = retry_count + 1, last_error = ?1
                         WHERE id = ?2",
                        params![error_text.as_str(), queued.id],
                    )?;
                }
                tx.commit()?;
            } else {
                let tx = conn.transaction()?;
                for queued in &queued_scrobbles {
                    tx.execute("DELETE FROM scrobble_queue WHERE id = ?1", params![queued.id])?;
                }
                tx.commit()?;
                eprintln!(
                    "Dropping non-retriable queued scrobble batch failure: {}",
                    error_text
                );
            }
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MetadataItem, PendingScrobble, PlayerState, PlaybackSnapshot, apply_direct_playback_snapshot,
        apply_metadata_item, decode_duration_ms, enqueue_failed_scrobble, hex_to_fourcc, init_queue,
        is_playing_status, is_retriable_scrobble_error, is_truthy_flag, maybe_ready_for_scrobble,
        parse_xml_item, pop_queued_scrobbles, queue_has_items, required_scrobble_play_time,
        should_reauthenticate,
    };
    use std::time::{Duration, Instant};

    fn extract_items(input: &str) -> Vec<&str> {
        let mut items = Vec::new();
        let mut cursor = 0;

        while let Some(start_rel) = input[cursor..].find("<item>") {
            let start = cursor + start_rel;
            if let Some(end_rel) = input[start..].find("</item>") {
                let end = start + end_rel + "</item>".len();
                items.push(&input[start..end]);
                cursor = end;
            } else {
                break;
            }
        }

        items
    }

    #[test]
    fn parses_dmap_hex_codes() {
        assert_eq!(hex_to_fourcc("6d696e6d").as_deref(), Some("minm"));
        assert_eq!(hex_to_fourcc("73736e63").as_deref(), Some("ssnc"));
        assert!(hex_to_fourcc("zzz").is_none());
    }

    #[test]
    fn parses_truthy_flag_values() {
        assert!(is_truthy_flag("1"));
        assert!(is_truthy_flag("true"));
        assert!(is_truthy_flag(" YES "));
        assert!(is_truthy_flag("On"));
        assert!(!is_truthy_flag("0"));
        assert!(!is_truthy_flag("false"));
        assert!(!is_truthy_flag("no"));
    }

    #[test]
    fn parses_playback_status() {
        assert!(is_playing_status("Playing"));
        assert!(is_playing_status("play"));
        assert!(!is_playing_status("paused"));
    }

    #[test]
    fn classifies_retryable_errors() {
        assert!(is_retriable_scrobble_error("Non Success status (503)"));
        assert!(is_retriable_scrobble_error("Service temporarily unavailable"));
        assert!(!is_retriable_scrobble_error("Missing required parameter artist"));
    }

    #[test]
    fn detects_reauth_errors() {
        assert!(should_reauthenticate("Invalid session key"));
        assert!(should_reauthenticate("error code 9"));
        assert!(!should_reauthenticate("Non Success status (503)"));
    }

    #[test]
    fn decodes_track_duration_from_bytes() {
        assert_eq!(
            decode_duration_ms(&[0x00, 0x01, 0xD4, 0xC0]),
            Some(Duration::from_millis(120_000))
        );
    }

    #[test]
    fn applies_lastfm_scrobble_threshold_rules() {
        assert_eq!(required_scrobble_play_time(Some(Duration::from_secs(30))), None);
        assert_eq!(
            required_scrobble_play_time(Some(Duration::from_secs(60))),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            required_scrobble_play_time(Some(Duration::from_secs(900))),
            Some(Duration::from_secs(240))
        );
        assert_eq!(
            required_scrobble_play_time(None),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn queue_helpers_keep_fifo_order_and_limit() {
        let mut conn = init_queue(":memory:").expect("queue init should work");
        assert!(!queue_has_items(&conn).expect("queue check should work"));

        for idx in 0..3 {
            let pending = PendingScrobble {
                artist: "A".to_string(),
                title: format!("Track-{idx}"),
                album: "Album".to_string(),
                played_at_unix: 1_000 + idx as u64,
            };
            enqueue_failed_scrobble(&mut conn, &pending, "err")
                .expect("enqueue should work");
        }

        assert!(queue_has_items(&conn).expect("queue check should work"));
        let first_two = pop_queued_scrobbles(&mut conn, 2).expect("pop should work");
        assert_eq!(first_two.len(), 2);
        assert_eq!(first_two[0].title, "Track-0");
        assert_eq!(first_two[1].title, "Track-1");

        let all = pop_queued_scrobbles(&mut conn, 50).expect("pop should work");
        assert_eq!(all.len(), 3);
        assert_eq!(all[2].title, "Track-2");
    }

    #[test]
    fn paused_time_does_not_count_toward_scrobble_threshold() {
        let mut state = PlayerState::default();
        state.current.title = "Song".to_string();
        state.current.artist = "Artist".to_string();
        state.current.album = "Album".to_string();
        state.current_duration = Some(Duration::from_secs(60));
        state.scrobbled = false;
        state.is_playing = false;
        state.played_duration = Duration::from_secs(29);
        state.segment_started_at = Some(Instant::now() - Duration::from_secs(10));
        state.track_started_unix = Some(1_700_000_000);

        assert!(maybe_ready_for_scrobble(&mut state).is_none());

        state.played_duration = Duration::from_secs(30);
        let ready = maybe_ready_for_scrobble(&mut state);
        assert!(ready.is_some());
    }

    #[test]
    fn parses_and_applies_duration_metadata() {
        let mut state = PlayerState::default();
        let item = MetadataItem {
            item_type: "core".to_string(),
            item_code: "astm".to_string(),
            decoded_text: String::new(),
            decoded_bytes: vec![0x00, 0x01, 0xD4, 0xC0],
        };

        assert!(!apply_metadata_item(&item, &mut state));
        assert_eq!(state.pending_duration, Some(Duration::from_millis(120_000)));
    }

    #[test]
    fn applies_direct_snapshot_and_resets_scrobble_state() {
        let mut state = PlayerState::default();
        state.scrobbled = true;
        let snapshot = PlaybackSnapshot {
            title: "Song".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            duration: Some(Duration::from_secs(300)),
            is_playing: true,
        };
        assert!(apply_direct_playback_snapshot(&snapshot, &mut state));
        assert!(!state.scrobbled);
        assert_eq!(state.current.title, "Song");
        assert_eq!(state.current_duration, Some(Duration::from_secs(300)));
    }

    #[test]
    fn replays_metadata_fixture_and_updates_tracks() {
        let fixture = include_str!("../tests/fixtures/metadata.xml");
        let items = extract_items(fixture);
        assert!(!items.is_empty(), "metadata fixture should contain items");

        let mut state = PlayerState::default();
        let mut now_playing_tracks = Vec::new();

        for xml_item in items {
            if let Some(item) = parse_xml_item(xml_item)
                && apply_metadata_item(&item, &mut state)
            {
                now_playing_tracks.push((
                    state.current.artist.clone(),
                    state.current.title.clone(),
                    state.current.album.clone(),
                    state.current_duration,
                ));
            }
        }

        assert_eq!(now_playing_tracks.len(), 2);
        assert_eq!(
            now_playing_tracks[0],
            (
                "Tosca".to_string(),
                "Naschkatze".to_string(),
                "J.A.C.".to_string(),
                Some(Duration::from_millis(258_272)),
            )
        );
        assert_eq!(
            now_playing_tracks[1],
            (
                "Tosca".to_string(),
                "Sala".to_string(),
                "J.A.C.".to_string(),
                Some(Duration::from_millis(498_076)),
            )
        );
        assert!(state.track_started_unix.is_some());
        assert!(state.is_playing);
        assert!(!state.scrobbled);
    }
}
