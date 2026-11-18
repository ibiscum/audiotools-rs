use base64::{Engine as _, engine::general_purpose::STANDARD};
use dbus::arg::{PropMap, RefArg};
use dbus::blocking::Connection as DbusConnection;
use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
use env_logger::{Builder, Env, Target};
use log::{debug, error, info, warn};
use quick_xml::Reader;
use quick_xml::events::Event;
use rusqlite::{Connection as SqliteConnection, params};
use scrobl::client::{Play, Writer};
use scrobl::{ApiKey, ApiSecret, Password, Retry};
use std::cmp;
use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::runtime::Builder as TokioRuntimeBuilder;
use tokio::sync::watch;

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
    Auto,
}

#[derive(Clone, Copy, Debug)]
enum ShairportBackend {
    Pipe,
    Dbus,
    Mpris,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SourceKind {
    Shairport,
    Mpd,
}

#[derive(Clone, Debug, PartialEq)]
struct PlaybackSnapshot {
    title: String,
    artist: String,
    album: String,
    duration: Option<Duration>,
    is_playing: bool,
}

#[derive(Clone, Debug)]
struct SourceEvent {
    source: SourceKind,
    snapshot: PlaybackSnapshot,
}

struct SourceArbiter {
    active_source: Option<SourceKind>,
    latest: HashMap<SourceKind, PlaybackSnapshot>,
}

#[derive(Clone)]
struct LastfmCredentials {
    username: String,
    password: String,
}

struct LastfmScrobbler {
    base_writer: Writer,
    writer: Writer,
    credentials: LastfmCredentials,
}

impl LastfmScrobbler {
    async fn new(
        api_key: &str,
        api_secret: &str,
        credentials: LastfmCredentials,
    ) -> Result<Self, scrobl::Error> {
        let base_writer = scrobl::Client::builder(ApiKey::new(api_key))
            .secret(ApiSecret::new(api_secret))
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .build_writer()?;
        let session = base_writer
            .auth()
            .mobile_session(
                credentials.username.as_str(),
                Password::new(credentials.password.as_str()),
            )
            .send()
            .await?
            .into_value();
        let writer = base_writer.with_session(session.into_key());

        Ok(Self {
            base_writer,
            writer,
            credentials,
        })
    }

    async fn reauthenticate(&mut self) -> Result<(), scrobl::Error> {
        let session = self
            .base_writer
            .auth()
            .mobile_session(
                self.credentials.username.as_str(),
                Password::new(self.credentials.password.as_str()),
            )
            .send()
            .await?;
        self.writer = self
            .base_writer
            .with_session(session.into_value().into_key());
        Ok(())
    }

    async fn scrobble_batch(&mut self, plays: &[Play]) -> Result<(usize, usize), scrobl::Error> {
        match self.writer.scrobble(plays).send().await {
            Ok(response) => {
                let report = response.into_value();
                Ok((report.accepted(), report.ignored()))
            }
            Err(err) if should_reauthenticate(err.retry()) => {
                if let Err(auth_error) = self.reauthenticate().await {
                    error!("Failed to re-authenticate with Last.fm: {}", auth_error);
                    return Err(err);
                }
                info!("Re-authenticated with Last.fm.");
                self.writer.scrobble(plays).send().await.map(|response| {
                    let report = response.into_value();
                    (report.accepted(), report.ignored())
                })
            }
            Err(err) => Err(err),
        }
    }

    async fn now_playing(&mut self, track: &TrackInfo) -> Result<(), scrobl::Error> {
        match self
            .writer
            .now_playing(track.artist.as_str(), track.title.as_str())
            .album(track.album.as_str())
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(err) if should_reauthenticate(err.retry()) => {
                if let Err(auth_error) = self.reauthenticate().await {
                    error!("Failed to re-authenticate with Last.fm: {}", auth_error);
                    return Err(err);
                }
                info!("Re-authenticated with Last.fm.");
                self.writer
                    .now_playing(track.artist.as_str(), track.title.as_str())
                    .album(track.album.as_str())
                    .send()
                    .await
                    .map(|_| ())
            }
            Err(err) => Err(err),
        }
    }
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
    retry_count: i64,
    last_error: Option<String>,
    created_at_unix: i64,
}

#[derive(Clone, Copy)]
enum DurationUnit {
    Millis,
    Micros,
}

#[derive(Clone, Copy, Debug)]
struct QueuePolicy {
    busy_timeout: Duration,
    queue_ttl: Duration,
    dead_letter_ttl: Duration,
    max_queue_items: i64,
    max_retry_count: i64,
}

impl QueuePolicy {
    fn from_env() -> Self {
        let busy_timeout_ms = parse_env_u64("LASTFM_QUEUE_BUSY_TIMEOUT_MS", 5_000);
        let queue_ttl_secs = parse_env_u64("LASTFM_QUEUE_TTL_SECS", 7 * 24 * 60 * 60);
        let dead_letter_ttl_secs = parse_env_u64("LASTFM_DEAD_LETTER_TTL_SECS", 30 * 24 * 60 * 60);
        let max_queue_items = parse_env_i64("LASTFM_QUEUE_MAX_ITEMS", 10_000);
        let max_retry_count = parse_env_i64("LASTFM_QUEUE_MAX_RETRIES", 20);

        if max_queue_items <= 0 {
            panic!("LASTFM_QUEUE_MAX_ITEMS must be greater than zero.");
        }
        if max_retry_count <= 0 {
            panic!("LASTFM_QUEUE_MAX_RETRIES must be greater than zero.");
        }

        Self {
            busy_timeout: Duration::from_millis(busy_timeout_ms),
            queue_ttl: Duration::from_secs(queue_ttl_secs),
            dead_letter_ttl: Duration::from_secs(dead_letter_ttl_secs),
            max_queue_items,
            max_retry_count,
        }
    }
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

impl SourceArbiter {
    fn new() -> Self {
        Self {
            active_source: None,
            latest: HashMap::new(),
        }
    }

    fn ingest(&mut self, event: &SourceEvent) -> bool {
        self.latest.insert(event.source, event.snapshot.clone());
        self.active_source = self.choose_active_source();
        self.active_source == Some(event.source)
    }

    fn choose_active_source(&self) -> Option<SourceKind> {
        if let Some(active_source) = self.active_source
            && self
                .latest
                .get(&active_source)
                .is_some_and(is_source_snapshot_active)
        {
            return Some(active_source);
        }

        [SourceKind::Shairport, SourceKind::Mpd]
            .into_iter()
            .find(|source| {
                self.latest
                    .get(source)
                    .is_some_and(is_source_snapshot_active)
            })
    }
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

fn main() {
    init_logging();
    let input_backend = parse_input_backend();
    let pipe_path =
        env::var("SHAIRPORT_PIPE").unwrap_or_else(|_| "/tmp/shairport-sync-metadata".to_string());
    let queue_db_path = env::var("LASTFM_QUEUE_DB")
        .unwrap_or_else(|_| "/var/lib/shairport-scrobbler/scrobble-queue.sqlite3".to_string());
    let queue_policy = QueuePolicy::from_env();
    let offline_mode = env::var("LASTFM_OFFLINE_MODE")
        .ok()
        .is_some_and(|value| is_truthy_flag(&value));

    let scrobbler_config = if offline_mode {
        info!("Last.fm offline mode enabled; skipping authentication and API calls.");
        None
    } else {
        let api_key =
            load_required_secret("LASTFM_API_KEY", "LASTFM_API_KEY_FILE", "lastfm_api_key");
        let api_secret = load_required_secret(
            "LASTFM_API_SECRET",
            "LASTFM_API_SECRET_FILE",
            "lastfm_api_secret",
        );
        let username =
            load_required_secret("LASTFM_USERNAME", "LASTFM_USERNAME_FILE", "lastfm_username");
        let password =
            load_required_secret("LASTFM_PASSWORD", "LASTFM_PASSWORD_FILE", "lastfm_password");
        Some((
            api_key,
            api_secret,
            LastfmCredentials { username, password },
        ))
    };

    let state = Arc::new(Mutex::new(PlayerState::default()));
    let queue_conn = if offline_mode {
        None
    } else {
        Some(
            init_queue(&queue_db_path, queue_policy)
                .unwrap_or_else(|err| panic!("Failed to initialize scrobble queue: {}", err)),
        )
    };
    let (now_playing_tx, now_playing_rx) = watch::channel(None);
    let runtime = TokioRuntimeBuilder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("Failed to create Tokio runtime");
    let (worker_ready_tx, worker_ready_rx) = mpsc::sync_channel(1);
    let worker_state = Arc::clone(&state);
    runtime.spawn(async move {
        let scrobbler = match scrobbler_config {
            Some((api_key, api_secret, credentials)) => {
                match LastfmScrobbler::new(&api_key, &api_secret, credentials).await {
                    Ok(scrobbler) => Some(scrobbler),
                    Err(err) => {
                        let _ = worker_ready_tx.send(Err(err.to_string()));
                        return;
                    }
                }
            }
            None => None,
        };

        if worker_ready_tx.send(Ok(())).is_err() {
            return;
        }
        if scrobbler.is_some() {
            info!("Successfully authenticated with Last.fm.");
        }
        run_lastfm_worker(
            worker_state,
            now_playing_rx,
            queue_conn,
            scrobbler,
            queue_policy,
        )
        .await;
    });

    worker_ready_rx
        .recv()
        .expect("Last.fm worker stopped before startup")
        .unwrap_or_else(|err| panic!("Last.fm authentication failed: {}", err));
    let now_playing = (!offline_mode).then_some(&now_playing_tx);
    let (source_tx, source_rx) = mpsc::channel::<SourceEvent>();

    match input_backend {
        InputBackend::Pipe => spawn_pipe_worker(pipe_path, source_tx, SourceKind::Shairport),
        InputBackend::Dbus => spawn_native_dbus_worker(source_tx, SourceKind::Shairport),
        InputBackend::Mpris => spawn_mpris_worker(source_tx, SourceKind::Shairport),
        InputBackend::Auto => {
            let shairport_backend = parse_auto_shairport_backend();
            info!(
                "Auto input mode enabled. Shairport backend={}, MPD idle-events enabled.",
                match shairport_backend {
                    ShairportBackend::Pipe => "pipe",
                    ShairportBackend::Dbus => "dbus",
                    ShairportBackend::Mpris => "mpris",
                }
            );
            spawn_mpd_worker(source_tx.clone(), SourceKind::Mpd);
            match shairport_backend {
                ShairportBackend::Pipe => {
                    spawn_pipe_worker(pipe_path, source_tx, SourceKind::Shairport)
                }
                ShairportBackend::Dbus => {
                    spawn_native_dbus_worker(source_tx, SourceKind::Shairport)
                }
                ShairportBackend::Mpris => spawn_mpris_worker(source_tx, SourceKind::Shairport),
            }
        }
    }

    run_source_event_loop(source_rx, &state, now_playing);
}

async fn run_lastfm_worker(
    state: Arc<Mutex<PlayerState>>,
    mut now_playing_rx: watch::Receiver<Option<TrackInfo>>,
    mut queue_conn: Option<SqliteConnection>,
    mut scrobbler: Option<LastfmScrobbler>,
    queue_policy: QueuePolicy,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await;
    let mut last_queue_retry = Instant::now();

    loop {
        tokio::select! {
            changed = now_playing_rx.changed(), if scrobbler.is_some() => {
                if changed.is_err() {
                    break;
                }
                let track = now_playing_rx.borrow_and_update().clone();
                if let Some(track) = track
                    && let Some(scrobbler) = scrobbler.as_mut()
                {
                    info!(
                        "Sending Last.fm now playing: title='{}', artist='{}', album='{}'",
                        track.title, track.artist, track.album
                    );
                    match scrobbler.now_playing(&track).await {
                        Ok(()) => info!(
                            "Last.fm accepted now playing: title='{}', artist='{}', album='{}'",
                            track.title, track.artist, track.album
                        ),
                        Err(err) => warn!("Last.fm now playing rejected: {}", err),
                    }
                }
            }
            _ = ticker.tick() => {
                let pending_scrobble = {
                    let mut state = state.lock().unwrap();
                    maybe_ready_for_scrobble(&mut state)
                };
                let Some(pending_scrobble) = pending_scrobble else {
                    if let (Some(scrobbler), Some(conn)) =
                        (scrobbler.as_mut(), queue_conn.as_mut())
                        && last_queue_retry.elapsed() >= Duration::from_secs(15)
                    {
                        if let Err(err) = apply_queue_retention(conn, &queue_policy) {
                            error!("Queue retention error: {}", err);
                        }
                        match queue_has_items(conn) {
                            Ok(true) => {
                                if let Err(err) =
                                    retry_queued_scrobbles(conn, scrobbler, &queue_policy).await
                                {
                                    error!("Queue retry error: {}", err);
                                }
                            }
                            Ok(false) => {}
                            Err(err) => error!("Failed to inspect scrobble queue: {}", err),
                        }
                        last_queue_retry = Instant::now();
                    }
                    continue;
                };

                let Some(scrobbler) = scrobbler.as_mut() else {
                    debug!(
                        "Offline mode: skipping scrobble for {} - {}",
                        pending_scrobble.artist, pending_scrobble.title
                    );
                    continue;
                };

                info!(
                    "Sending Last.fm scrobble: title='{}', artist='{}', album='{}'",
                    pending_scrobble.title, pending_scrobble.artist, pending_scrobble.album
                );
                let play = Play::new(
                    &pending_scrobble.artist,
                    &pending_scrobble.title,
                    pending_scrobble.played_at_unix,
                )
                .album(&pending_scrobble.album);

                match scrobbler.scrobble_batch(std::slice::from_ref(&play)).await {
                    Ok((1, ignored)) => info!(
                        "Last.fm accepted scrobble: title='{}', artist='{}', album='{}', timestamp={} (ignored={})",
                        pending_scrobble.title,
                        pending_scrobble.artist,
                        pending_scrobble.album,
                        pending_scrobble.played_at_unix,
                        ignored
                    ),
                    Ok((accepted, ignored)) => warn!(
                        "Last.fm did not accept scrobble: title='{}', artist='{}', album='{}', accepted={}, ignored={}",
                        pending_scrobble.title,
                        pending_scrobble.artist,
                        pending_scrobble.album,
                        accepted,
                        ignored
                    ),
                    Err(err) => {
                        let error_text = err.to_string();
                        if is_retriable_scrobble_error(err.retry()) {
                            if let Some(conn) = queue_conn.as_mut() {
                                if let Err(queue_err) = enqueue_scrobble(
                                    conn,
                                    &pending_scrobble,
                                    Some(error_text.as_str()),
                                ) {
                                    error!("Failed to queue retriable scrobble: {}", queue_err);
                                    reset_scrobble_state_if_current(&state, &pending_scrobble);
                                } else {
                                    if let Err(retention_err) = apply_queue_retention(conn, &queue_policy)
                                    {
                                        error!("Queue retention error: {}", retention_err);
                                    }
                                    warn!(
                                        "Queued retriable scrobble after send failure: {}",
                                        error_text
                                    );
                                }
                            } else {
                                error!(
                                    "Retriable scrobble failure without durable queue availability: {}",
                                    error_text
                                );
                                reset_scrobble_state_if_current(&state, &pending_scrobble);
                            }
                        } else {
                            warn!(
                                "Not retrying non-retriable scrobble failure for {} - {}: {}",
                                pending_scrobble.artist, pending_scrobble.title, error_text
                            );
                        }
                    }
                }
            }
        }
    }
}

fn reset_scrobble_state_if_current(
    state: &Arc<Mutex<PlayerState>>,
    pending_scrobble: &PendingScrobble,
) {
    let mut state = state.lock().unwrap();
    if state.track_started_unix == Some(pending_scrobble.played_at_unix)
        && state.current.artist == pending_scrobble.artist
        && state.current.title == pending_scrobble.title
        && state.current.album == pending_scrobble.album
    {
        state.scrobbled = false;
    }
}

fn load_required_secret(env_key: &str, file_env_key: &str, cred_file_name: &str) -> String {
    if let Ok(value) = env::var(env_key) {
        return value;
    }

    let secret_path = env::var(file_env_key).ok().map(PathBuf::from).or_else(|| {
        env::var("CREDENTIALS_DIRECTORY")
            .ok()
            .map(|dir| Path::new(&dir).join(cred_file_name))
    });

    let Some(secret_path) = secret_path else {
        panic!(
            "{} is missing. Set {}, {} or provide {} in CREDENTIALS_DIRECTORY.",
            env_key, env_key, file_env_key, cred_file_name
        );
    };

    let value = fs::read_to_string(&secret_path).unwrap_or_else(|err| {
        panic!(
            "Failed to read {} from {}: {}",
            env_key,
            secret_path.display(),
            err
        )
    });

    let value = value.trim_end_matches(['\n', '\r']).to_string();
    if value.is_empty() {
        panic!(
            "{} credential file is empty: {}",
            env_key,
            secret_path.display()
        );
    }

    value
}

fn parse_env_u64(env_key: &str, default: u64) -> u64 {
    match env::var(env_key) {
        Ok(value) => value.parse::<u64>().unwrap_or_else(|_| {
            panic!("{} must be an unsigned integer; got {:?}.", env_key, value)
        }),
        Err(_) => default,
    }
}

fn parse_env_i64(env_key: &str, default: i64) -> i64 {
    match env::var(env_key) {
        Ok(value) => value
            .parse::<i64>()
            .unwrap_or_else(|_| panic!("{} must be an integer; got {:?}.", env_key, value)),
        Err(_) => default,
    }
}

fn parse_input_backend() -> InputBackend {
    let input_backend = env::var("LASTFM_INPUT_BACKEND").unwrap_or_else(|_| "pipe".to_string());
    match input_backend.trim().to_ascii_lowercase().as_str() {
        "pipe" => InputBackend::Pipe,
        "dbus" => InputBackend::Dbus,
        "mpris" => InputBackend::Mpris,
        "auto" => InputBackend::Auto,
        other => panic!(
            "Unsupported LASTFM_INPUT_BACKEND value: {}. Use one of: pipe, dbus, mpris, auto.",
            other
        ),
    }
}

fn parse_auto_shairport_backend() -> ShairportBackend {
    let backend = env::var("LASTFM_AUTO_SHAIRPORT_BACKEND").unwrap_or_else(|_| "pipe".to_string());
    match backend.trim().to_ascii_lowercase().as_str() {
        "pipe" => ShairportBackend::Pipe,
        "dbus" => ShairportBackend::Dbus,
        "mpris" => ShairportBackend::Mpris,
        other => panic!(
            "Unsupported LASTFM_AUTO_SHAIRPORT_BACKEND value: {}. Use one of: pipe, dbus, mpris.",
            other
        ),
    }
}

fn run_source_event_loop(
    source_rx: mpsc::Receiver<SourceEvent>,
    state: &Arc<Mutex<PlayerState>>,
    now_playing: Option<&watch::Sender<Option<TrackInfo>>>,
) -> ! {
    let mut arbiter = SourceArbiter::new();
    loop {
        match source_rx.recv() {
            Ok(event) => {
                let was_active = arbiter.active_source;
                let is_active_event = arbiter.ingest(&event);
                let is_last_event_from_previous_active =
                    was_active == Some(event.source) && arbiter.active_source.is_none();
                if was_active != arbiter.active_source
                    && let Some(active_source) = arbiter.active_source
                {
                    info!("Active playback source switched to {:?}", active_source);
                }
                if is_active_event || is_last_event_from_previous_active {
                    apply_playback_snapshot(&event.snapshot, state, now_playing);
                }
            }
            Err(err) => panic!("All source workers exited unexpectedly: {}", err),
        }
    }
}

fn spawn_pipe_worker(pipe_path: String, source_tx: mpsc::Sender<SourceEvent>, source: SourceKind) {
    thread::spawn(move || run_pipe_worker_loop(pipe_path, source_tx, source));
}

fn run_pipe_worker_loop(
    pipe_path: String,
    source_tx: mpsc::Sender<SourceEvent>,
    source: SourceKind,
) -> ! {
    let mut worker_state = PlayerState::default();
    let mut last_snapshot = None;
    loop {
        debug!("Opening pipe: {}", pipe_path);
        let file = match File::open(&pipe_path) {
            Ok(f) => f,
            Err(e) => {
                warn!("Waiting for pipe ({}): {}", pipe_path, e);
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
                process_xml_item(&item_buffer, &mut worker_state);
                let snapshot = snapshot_from_player_state(&worker_state);
                emit_snapshot_if_changed(source, &snapshot, &source_tx, &mut last_snapshot);
                item_buffer.clear();
            }
            line.clear();
        }
    }
}

fn spawn_mpris_worker(source_tx: mpsc::Sender<SourceEvent>, source: SourceKind) {
    thread::spawn(move || run_mpris_worker_loop(source_tx, source));
}

fn run_mpris_worker_loop(source_tx: mpsc::Sender<SourceEvent>, source: SourceKind) -> ! {
    let bus = env::var("MPRIS_BUS").unwrap_or_else(|_| "system".to_string());
    let nametag = env::var("SHAIRPORT_DBUS_NAMETAG").ok();
    let default_service = match nametag {
        Some(tag) if !tag.trim().is_empty() => {
            format!("org.mpris.MediaPlayer2.ShairportSync.{tag}")
        }
        _ => "org.mpris.MediaPlayer2.ShairportSync".to_string(),
    };
    let service = env::var("MPRIS_SERVICE").unwrap_or(default_service);
    let path = env::var("MPRIS_PATH").unwrap_or_else(|_| "/org/mpris/MediaPlayer2".to_string());
    let conn = open_dbus_connection(&bus);

    info!(
        "Using MPRIS backend on {} bus: service={}, path={}",
        bus, service, path
    );
    let mut last_snapshot = None;

    loop {
        match read_mpris_snapshot(&conn, &service, &path) {
            Ok(snapshot) => {
                emit_snapshot_if_changed(source, &snapshot, &source_tx, &mut last_snapshot)
            }
            Err(err) => warn!("MPRIS read error: {}", err),
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn spawn_native_dbus_worker(source_tx: mpsc::Sender<SourceEvent>, source: SourceKind) {
    thread::spawn(move || run_native_dbus_worker_loop(source_tx, source));
}

fn run_native_dbus_worker_loop(source_tx: mpsc::Sender<SourceEvent>, source: SourceKind) -> ! {
    let bus = env::var("SHAIRPORT_DBUS_BUS").unwrap_or_else(|_| "system".to_string());
    let nametag = env::var("SHAIRPORT_DBUS_NAMETAG").ok();
    let default_service = match nametag {
        Some(tag) if !tag.trim().is_empty() => format!("org.gnome.ShairportSync.{tag}"),
        _ => "org.gnome.ShairportSync".to_string(),
    };
    let service = env::var("SHAIRPORT_DBUS_SERVICE").unwrap_or(default_service);
    let path =
        env::var("SHAIRPORT_DBUS_PATH").unwrap_or_else(|_| "/org/gnome/ShairportSync".to_string());
    let conn = open_dbus_connection(&bus);

    info!(
        "Using native DBus backend on {} bus: service={}, path={}",
        bus, service, path
    );
    let mut last_snapshot = None;

    loop {
        match read_native_dbus_snapshot(&conn, &service, &path) {
            Ok(snapshot) => {
                emit_snapshot_if_changed(source, &snapshot, &source_tx, &mut last_snapshot)
            }
            Err(err) => warn!("DBus read error: {}", err),
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn spawn_mpd_worker(source_tx: mpsc::Sender<SourceEvent>, source: SourceKind) {
    thread::spawn(move || run_mpd_worker_loop(source_tx, source));
}

fn run_mpd_worker_loop(source_tx: mpsc::Sender<SourceEvent>, source: SourceKind) -> ! {
    let host = env::var("MPD_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = env::var("MPD_PORT")
        .unwrap_or_else(|_| "6600".to_string())
        .parse::<u16>()
        .unwrap_or_else(|_| panic!("MPD_PORT must be a valid TCP port"));

    let mut last_snapshot = None;
    loop {
        match connect_mpd_session(&host, port) {
            Ok((mut writer, mut reader)) => {
                info!("Connected to MPD at {}:{}", host, port);
                if let Some(password) = env::var("MPD_PASSWORD").ok()
                    && let Err(err) = mpd_send_command(
                        &mut writer,
                        &mut reader,
                        &format!("password {}", password),
                    )
                {
                    warn!("MPD password authentication failed: {}", err);
                    thread::sleep(Duration::from_secs(3));
                    continue;
                }
                if let Err(err) = poll_mpd_events(
                    &mut writer,
                    &mut reader,
                    source,
                    &source_tx,
                    &mut last_snapshot,
                ) {
                    warn!("MPD event loop error: {}", err);
                    thread::sleep(Duration::from_secs(2));
                }
            }
            Err(err) => {
                warn!("Waiting for MPD at {}:{}: {}", host, port, err);
                thread::sleep(Duration::from_secs(3));
            }
        }
    }
}

fn connect_mpd_session(host: &str, port: u16) -> Result<(TcpStream, BufReader<TcpStream>), String> {
    let stream = TcpStream::connect((host, port)).map_err(|err| err.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|err| err.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|err| err.to_string())?;
    let reader_stream = stream.try_clone().map_err(|err| err.to_string())?;
    let mut reader = BufReader::new(reader_stream);
    let mut greeting = String::new();
    reader
        .read_line(&mut greeting)
        .map_err(|err| err.to_string())?;
    if !greeting.starts_with("OK MPD") {
        return Err(format!("unexpected MPD greeting: {}", greeting.trim_end()));
    }
    stream.set_read_timeout(None).map_err(|err| err.to_string())?;
    reader
        .get_mut()
        .set_read_timeout(None)
        .map_err(|err| err.to_string())?;
    Ok((stream, reader))
}

fn poll_mpd_events(
    writer: &mut TcpStream,
    reader: &mut BufReader<TcpStream>,
    source: SourceKind,
    source_tx: &mpsc::Sender<SourceEvent>,
    last_snapshot: &mut Option<PlaybackSnapshot>,
) -> Result<(), String> {
    let initial_snapshot = read_mpd_snapshot(writer, reader)?;
    emit_snapshot_if_changed(source, &initial_snapshot, source_tx, last_snapshot);

    loop {
        let events = mpd_send_command(writer, reader, "idle player")?;
        if !events.iter().any(|line| line == "changed: player") {
            continue;
        }
        let snapshot = read_mpd_snapshot(writer, reader)?;
        emit_snapshot_if_changed(source, &snapshot, source_tx, last_snapshot);
    }
}

fn read_mpd_snapshot(
    writer: &mut TcpStream,
    reader: &mut BufReader<TcpStream>,
) -> Result<PlaybackSnapshot, String> {
    let status_lines = mpd_send_command(writer, reader, "status")?;
    let song_lines = mpd_send_command(writer, reader, "currentsong")?;

    let mut is_playing = false;
    for line in status_lines {
        if let Some(value) = line.strip_prefix("state:") {
            is_playing = value.trim().eq_ignore_ascii_case("play");
        }
    }

    let mut title = String::new();
    let mut artist = String::new();
    let mut album = String::new();
    let mut duration = None;

    for line in song_lines {
        if let Some(value) = line.strip_prefix("Title:") {
            title = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("Artist:") {
            artist = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("Album:") {
            album = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("duration:") {
            if let Ok(seconds) = value.trim().parse::<f64>()
                && seconds.is_finite()
                && seconds > 0.0
            {
                duration = Some(Duration::from_millis((seconds * 1000.0) as u64));
            }
        } else if let Some(value) = line.strip_prefix("Time:")
            && duration.is_none()
            && let Ok(seconds) = value.trim().parse::<u64>()
        {
            duration = Some(Duration::from_secs(seconds));
        }
    }

    Ok(PlaybackSnapshot {
        title,
        artist,
        album,
        duration,
        is_playing,
    })
}

fn mpd_send_command(
    writer: &mut TcpStream,
    reader: &mut BufReader<TcpStream>,
    command: &str,
) -> Result<Vec<String>, String> {
    writer
        .write_all(command.as_bytes())
        .map_err(|err| err.to_string())?;
    writer.write_all(b"\n").map_err(|err| err.to_string())?;
    writer.flush().map_err(|err| err.to_string())?;
    mpd_read_response(reader)
}

fn mpd_read_response(reader: &mut BufReader<TcpStream>) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).map_err(|err| err.to_string())?;
        if bytes == 0 {
            return Err("MPD closed the connection".to_string());
        }
        let trimmed = line.trim_end().to_string();
        if trimmed == "OK" {
            return Ok(lines);
        }
        if trimmed.starts_with("ACK ") {
            return Err(trimmed);
        }
        lines.push(trimmed);
    }
}

fn emit_snapshot_if_changed(
    source: SourceKind,
    snapshot: &PlaybackSnapshot,
    source_tx: &mpsc::Sender<SourceEvent>,
    last_snapshot: &mut Option<PlaybackSnapshot>,
) {
    if last_snapshot.as_ref() == Some(snapshot) {
        return;
    }

    *last_snapshot = Some(snapshot.clone());
    if let Err(err) = source_tx.send(SourceEvent {
        source,
        snapshot: snapshot.clone(),
    }) {
        panic!("Failed to send source event: {}", err);
    }
}

fn is_source_snapshot_active(snapshot: &PlaybackSnapshot) -> bool {
    snapshot.is_playing && !snapshot.artist.is_empty() && !snapshot.title.is_empty()
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
            &[
                ("mpris:length", DurationUnit::Micros),
                ("duration", DurationUnit::Millis),
            ],
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
    now_playing: Option<&watch::Sender<Option<TrackInfo>>>,
) {
    let mut st = state.lock().unwrap();
    if apply_direct_playback_snapshot(snapshot, &mut st) {
        info!(
            "Now Playing: {} - {} ({})",
            st.current.artist, st.current.title, st.current.album
        );
        send_now_playing(now_playing, &st.current);
    }
}

fn process_xml_item(xml: &str, state: &mut PlayerState) {
    let item = match parse_xml_item(xml) {
        Ok(item) => item,
        Err(err) => {
            warn!("Ignoring invalid Shairport metadata item: {}", err);
            return;
        }
    };

    apply_metadata_item(&item, state);
}

fn snapshot_from_player_state(st: &PlayerState) -> PlaybackSnapshot {
    PlaybackSnapshot {
        title: st.current.title.clone(),
        artist: st.current.artist.clone(),
        album: st.current.album.clone(),
        duration: st.current_duration,
        is_playing: st.is_playing,
    }
}

fn send_now_playing(
    now_playing: Option<&watch::Sender<Option<TrackInfo>>>,
    track_info: &TrackInfo,
) {
    if let Some(sender) = now_playing {
        sender.send_replace(Some(track_info.clone()));
    } else {
        debug!("Offline mode: skipping Last.fm now playing update.");
    }
}

fn propmap_first_string(map: &PropMap, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        map.get(*key)
            .and_then(|value| refarg_to_string(value.0.as_ref()))
    })
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
    status.trim().eq_ignore_ascii_case("playing") || status.trim().eq_ignore_ascii_case("play")
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
        info!(
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
    if st.is_playing
        && let Some(segment_started_at) = st.segment_started_at
    {
        return st.played_duration + segment_started_at.elapsed();
    }
    st.played_duration
}

fn set_playing_state(st: &mut PlayerState, is_playing: bool) {
    if st.is_playing
        && !is_playing
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

    let mut bytes = [0; 4];
    for (index, pair) in hex_str.as_bytes().chunks_exact(2).enumerate() {
        let pair = std::str::from_utf8(pair).ok()?;
        bytes[index] = u8::from_str_radix(pair, 16).ok()?;
    }
    String::from_utf8(bytes.to_vec()).ok()
}

fn parse_xml_item(xml: &str) -> Result<MetadataItem, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut current_tag = String::new();
    let mut item_type = String::new();
    let mut item_code = String::new();
    let mut data_b64 = String::new();
    let mut item_closed = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                current_tag = e.name().as_ref().to_string();
            }
            Ok(Event::Text(e)) => {
                append_metadata_xml_text(
                    &current_tag,
                    &e.xml10_content(),
                    &mut item_type,
                    &mut item_code,
                    &mut data_b64,
                );
            }
            Ok(Event::CData(e)) => {
                append_metadata_xml_text(
                    &current_tag,
                    &e.xml10_content(),
                    &mut item_type,
                    &mut item_code,
                    &mut data_b64,
                );
            }
            Ok(Event::GeneralRef(e)) => {
                let text = match e
                    .resolve_char_ref()
                    .map_err(|err| format!("invalid XML character reference: {err}"))?
                {
                    Some(character) => character.to_string(),
                    None => quick_xml::escape::resolve_predefined_entity(e.as_ref())
                        .ok_or_else(|| format!("unknown XML entity: &{};", e.as_ref()))?
                        .to_string(),
                };
                append_metadata_xml_text(
                    &current_tag,
                    &text,
                    &mut item_type,
                    &mut item_code,
                    &mut data_b64,
                );
            }
            Ok(Event::End(e)) => {
                if e.name().as_ref() == "item" {
                    item_closed = true;
                    break;
                }
                current_tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(format!("invalid XML metadata: {err}")),
            _ => {}
        }
    }

    if !item_closed {
        return Err("metadata item is missing its closing </item> tag".to_string());
    }
    let item_type = hex_to_fourcc(&item_type)
        .ok_or_else(|| format!("metadata item has an invalid type code: {item_type:?}"))?;
    let item_code = hex_to_fourcc(&item_code)
        .ok_or_else(|| format!("metadata item has an invalid item code: {item_code:?}"))?;

    let encoded_data = data_b64
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect::<String>();
    let decoded_bytes = if encoded_data.is_empty() {
        Vec::new()
    } else {
        STANDARD
            .decode(encoded_data)
            .map_err(|err| format!("metadata item contains invalid Base64 data: {err}"))?
    };
    let decoded_text = String::from_utf8(decoded_bytes.clone())
        .ok()
        .unwrap_or_default();

    Ok(MetadataItem {
        item_type,
        item_code,
        decoded_text,
        decoded_bytes,
    })
}

fn append_metadata_xml_text(
    tag: &str,
    text: &str,
    item_type: &mut String,
    item_code: &mut String,
    data_b64: &mut String,
) {
    match tag {
        "type" => item_type.push_str(text),
        "code" => item_code.push_str(text),
        "data" => data_b64.push_str(text),
        _ => {}
    }
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

fn init_queue(path: &str, policy: QueuePolicy) -> Result<SqliteConnection, String> {
    let queue_path = Path::new(path);
    if let Some(parent) = queue_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let conn = SqliteConnection::open(queue_path).map_err(|e| e.to_string())?;
    conn.busy_timeout(policy.busy_timeout)
        .map_err(|e| e.to_string())?;
    if path != ":memory:" {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| e.to_string())?;
    }
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
    conn.execute(
        "CREATE TABLE IF NOT EXISTS dead_letter_scrobbles (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            queue_row_id INTEGER,
            artist TEXT NOT NULL,
            title TEXT NOT NULL,
            album TEXT NOT NULL,
            played_at_unix INTEGER NOT NULL,
            retry_count INTEGER NOT NULL,
            last_error TEXT,
            created_at_unix INTEGER NOT NULL,
            dead_letter_reason TEXT NOT NULL,
            dead_lettered_at_unix INTEGER NOT NULL
        )",
        [],
    )
    .map_err(|e| e.to_string())?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_scrobble_queue_created_at ON scrobble_queue(created_at_unix)",
        [],
    )
    .map_err(|e| e.to_string())?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_dead_lettered_at ON dead_letter_scrobbles(dead_lettered_at_unix)",
        [],
    )
    .map_err(|e| e.to_string())?;

    Ok(conn)
}

fn enqueue_scrobble(
    conn: &mut SqliteConnection,
    track: &PendingScrobble,
    last_error: Option<&str>,
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
            last_error,
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
        "SELECT id, artist, title, album, played_at_unix, retry_count, last_error, created_at_unix
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
            retry_count: row.get(5)?,
            last_error: row.get(6)?,
            created_at_unix: row.get(7)?,
        })
    })?;

    queued_rows.collect::<Result<Vec<_>, rusqlite::Error>>()
}

fn move_queued_scrobbles_to_dead_letter(
    conn: &mut SqliteConnection,
    queued_scrobbles: &[QueuedScrobble],
    dead_letter_reason: &str,
    last_error: Option<&str>,
) -> Result<(), rusqlite::Error> {
    if queued_scrobbles.is_empty() {
        return Ok(());
    }

    let dead_lettered_at_unix = current_unix_timestamp() as i64;
    let tx = conn.transaction()?;
    for queued in queued_scrobbles {
        tx.execute(
            "INSERT INTO dead_letter_scrobbles (
                queue_row_id, artist, title, album, played_at_unix, retry_count,
                last_error, created_at_unix, dead_letter_reason, dead_lettered_at_unix
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, COALESCE(?7, ?8), ?9, ?10, ?11)",
            params![
                queued.id,
                queued.artist,
                queued.title,
                queued.album,
                queued.played_at_unix as i64,
                queued.retry_count,
                last_error,
                queued.last_error.as_deref(),
                queued.created_at_unix,
                dead_letter_reason,
                dead_lettered_at_unix
            ],
        )?;
        tx.execute(
            "DELETE FROM scrobble_queue WHERE id = ?1",
            params![queued.id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn move_expired_queued_scrobbles_to_dead_letter(
    conn: &mut SqliteConnection,
    min_created_at_unix: i64,
) -> Result<usize, rusqlite::Error> {
    let expired = {
        let mut stmt = conn.prepare(
            "SELECT id, artist, title, album, played_at_unix, retry_count, last_error, created_at_unix
             FROM scrobble_queue
             WHERE created_at_unix < ?1
             ORDER BY id ASC",
        )?;
        stmt.query_map(params![min_created_at_unix], |row| {
            Ok(QueuedScrobble {
                id: row.get(0)?,
                artist: row.get(1)?,
                title: row.get(2)?,
                album: row.get(3)?,
                played_at_unix: row.get::<_, i64>(4)? as u64,
                retry_count: row.get(5)?,
                last_error: row.get(6)?,
                created_at_unix: row.get(7)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?
    };

    let count = expired.len();
    move_queued_scrobbles_to_dead_letter(conn, &expired, "expired_ttl", None)?;
    Ok(count)
}

fn move_excess_queued_scrobbles_to_dead_letter(
    conn: &mut SqliteConnection,
    max_queue_items: i64,
) -> Result<usize, rusqlite::Error> {
    let total: i64 = conn.query_row("SELECT COUNT(1) FROM scrobble_queue", [], |row| row.get(0))?;
    if total <= max_queue_items {
        return Ok(0);
    }

    let overflow = total - max_queue_items;
    let overflow_rows = pop_queued_scrobbles(conn, overflow)?;
    let count = overflow_rows.len();
    move_queued_scrobbles_to_dead_letter(conn, &overflow_rows, "queue_capacity_exceeded", None)?;
    Ok(count)
}

fn move_retry_exhausted_scrobbles_to_dead_letter(
    conn: &mut SqliteConnection,
    max_retry_count: i64,
) -> Result<usize, rusqlite::Error> {
    let exhausted = {
        let mut stmt = conn.prepare(
            "SELECT id, artist, title, album, played_at_unix, retry_count, last_error, created_at_unix
             FROM scrobble_queue
             WHERE retry_count >= ?1
             ORDER BY id ASC",
        )?;
        stmt.query_map(params![max_retry_count], |row| {
            Ok(QueuedScrobble {
                id: row.get(0)?,
                artist: row.get(1)?,
                title: row.get(2)?,
                album: row.get(3)?,
                played_at_unix: row.get::<_, i64>(4)? as u64,
                retry_count: row.get(5)?,
                last_error: row.get(6)?,
                created_at_unix: row.get(7)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?
    };

    let count = exhausted.len();
    move_queued_scrobbles_to_dead_letter(conn, &exhausted, "max_retries_exceeded", None)?;
    Ok(count)
}

fn purge_expired_dead_letters(
    conn: &mut SqliteConnection,
    min_dead_lettered_at_unix: i64,
) -> Result<usize, rusqlite::Error> {
    conn.execute(
        "DELETE FROM dead_letter_scrobbles WHERE dead_lettered_at_unix < ?1",
        params![min_dead_lettered_at_unix],
    )
}

fn apply_queue_retention(
    conn: &mut SqliteConnection,
    queue_policy: &QueuePolicy,
) -> Result<(), rusqlite::Error> {
    let now_unix = current_unix_timestamp() as i64;
    let min_created_at = now_unix - queue_policy.queue_ttl.as_secs() as i64;
    let min_dead_lettered_at = now_unix - queue_policy.dead_letter_ttl.as_secs() as i64;

    let expired_count = move_expired_queued_scrobbles_to_dead_letter(conn, min_created_at)?;
    if expired_count > 0 {
        warn!(
            "Moved {} expired queued scrobbles to dead letter queue.",
            expired_count
        );
    }

    let overflow_count =
        move_excess_queued_scrobbles_to_dead_letter(conn, queue_policy.max_queue_items)?;
    if overflow_count > 0 {
        warn!(
            "Moved {} queued scrobbles to dead letter queue due to queue capacity.",
            overflow_count
        );
    }

    let purged_dead_letters = purge_expired_dead_letters(conn, min_dead_lettered_at)?;
    if purged_dead_letters > 0 {
        info!(
            "Purged {} expired dead letter queue rows.",
            purged_dead_letters
        );
    }

    Ok(())
}

fn increment_retry_count_for_queued_scrobbles(
    conn: &mut SqliteConnection,
    queued_scrobbles: &[QueuedScrobble],
    error_text: &str,
) -> Result<(), rusqlite::Error> {
    let tx = conn.transaction()?;
    for queued in queued_scrobbles {
        tx.execute(
            "UPDATE scrobble_queue
             SET retry_count = retry_count + 1, last_error = ?1
             WHERE id = ?2",
            params![error_text, queued.id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn delete_queued_scrobbles(
    conn: &mut SqliteConnection,
    queued_scrobbles: &[QueuedScrobble],
) -> Result<(), rusqlite::Error> {
    let tx = conn.transaction()?;
    for queued in queued_scrobbles {
        tx.execute(
            "DELETE FROM scrobble_queue WHERE id = ?1",
            params![queued.id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn is_retriable_scrobble_error(retry: Retry) -> bool {
    matches!(
        retry,
        Retry::Later | Retry::AfterBackoff | Retry::AfterReauthentication
    )
}

fn should_reauthenticate(retry: Retry) -> bool {
    retry == Retry::AfterReauthentication
}

async fn retry_queued_scrobbles(
    conn: &mut SqliteConnection,
    scrobbler: &mut LastfmScrobbler,
    queue_policy: &QueuePolicy,
) -> Result<(), rusqlite::Error> {
    let queued_scrobbles = pop_queued_scrobbles(conn, 50)?;
    if queued_scrobbles.is_empty() {
        return Ok(());
    }

    let plays = queued_scrobbles
        .iter()
        .map(|queued| {
            Play::new(&queued.artist, &queued.title, queued.played_at_unix).album(&queued.album)
        })
        .collect::<Vec<_>>();

    match scrobbler.scrobble_batch(&plays).await {
        Ok((accepted, ignored)) => {
            delete_queued_scrobbles(conn, &queued_scrobbles)?;
            info!(
                "Last.fm processed queued scrobble batch: accepted={}, ignored={}, requested={}",
                accepted,
                ignored,
                queued_scrobbles.len()
            );
        }
        Err(err) => {
            let error_text = err.to_string();
            if is_retriable_scrobble_error(err.retry()) {
                increment_retry_count_for_queued_scrobbles(
                    conn,
                    &queued_scrobbles,
                    error_text.as_str(),
                )?;
                warn!(
                    "Retrying queued scrobble batch after retryable failure: {}",
                    error_text
                );
                let exhausted = move_retry_exhausted_scrobbles_to_dead_letter(
                    conn,
                    queue_policy.max_retry_count,
                )?;
                if exhausted > 0 {
                    warn!(
                        "Moved {} queued scrobbles to dead letter queue after retry limit.",
                        exhausted
                    );
                }
            } else {
                move_queued_scrobbles_to_dead_letter(
                    conn,
                    &queued_scrobbles,
                    "non_retriable_failure",
                    Some(error_text.as_str()),
                )?;
                warn!(
                    "Moved non-retriable queued scrobble batch to dead letter queue: {}",
                    error_text
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        MetadataItem, PendingScrobble, PlaybackSnapshot, PlayerState, QueuePolicy, Retry,
        SourceArbiter, SourceEvent, SourceKind, TrackInfo, apply_direct_playback_snapshot,
        apply_metadata_item, apply_playback_snapshot, apply_queue_retention, decode_duration_ms,
        delete_queued_scrobbles, enqueue_scrobble, hex_to_fourcc,
        increment_retry_count_for_queued_scrobbles, init_queue, is_playing_status,
        is_retriable_scrobble_error, is_truthy_flag, maybe_ready_for_scrobble,
        move_queued_scrobbles_to_dead_letter, parse_xml_item, pop_queued_scrobbles,
        queue_has_items, required_scrobble_play_time, send_now_playing, should_reauthenticate,
    };
    use rusqlite::params;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use tokio::sync::watch;

    fn queue_policy_for_tests() -> QueuePolicy {
        QueuePolicy {
            busy_timeout: Duration::from_secs(5),
            queue_ttl: Duration::from_secs(120),
            dead_letter_ttl: Duration::from_secs(120),
            max_queue_items: 100,
            max_retry_count: 3,
        }
    }

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
        assert!(hex_to_fourcc("éééé").is_none());
    }

    #[test]
    fn parses_xml_character_references_and_cdata() {
        let item = parse_xml_item(
            "<item><type>636f7&#x32;65</type><code><![CDATA[6d696e6d]]></code>\
             <data encoding=\"base64\">TmFz<![CDATA[Y2hrYXR6ZQ==]]></data></item>",
        )
        .expect("metadata item should parse");

        assert_eq!(item.item_type, "core");
        assert_eq!(item.item_code, "minm");
        assert_eq!(item.decoded_text, "Naschkatze");
    }

    #[test]
    fn rejects_incomplete_xml_metadata_items() {
        let err = parse_xml_item("<item><type>636f7265</type><code>6d696e6d</code>")
            .err()
            .expect("unclosed metadata item should be rejected");

        assert!(err.contains("closing </item>"));
    }

    #[test]
    fn rejects_invalid_base64_metadata() {
        let err = parse_xml_item(
            "<item><type>636f7265</type><code>6d696e6d</code><data>!!!</data></item>",
        )
        .err()
        .expect("invalid Base64 should be rejected");

        assert!(err.contains("invalid Base64"));
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
        assert!(is_retriable_scrobble_error(Retry::Later));
        assert!(is_retriable_scrobble_error(Retry::AfterBackoff));
        assert!(is_retriable_scrobble_error(Retry::AfterReauthentication));
        assert!(!is_retriable_scrobble_error(Retry::No));
    }

    #[test]
    fn detects_reauth_errors() {
        assert!(should_reauthenticate(Retry::AfterReauthentication));
        assert!(!should_reauthenticate(Retry::Later));
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
        assert_eq!(
            required_scrobble_play_time(Some(Duration::from_secs(30))),
            None
        );
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
        let mut conn =
            init_queue(":memory:", queue_policy_for_tests()).expect("queue init should work");
        assert!(!queue_has_items(&conn).expect("queue check should work"));

        for idx in 0..3 {
            let pending = PendingScrobble {
                artist: "A".to_string(),
                title: format!("Track-{idx}"),
                album: "Album".to_string(),
                played_at_unix: 1_000 + idx as u64,
            };
            enqueue_scrobble(&mut conn, &pending, Some("err")).expect("enqueue should work");
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
    fn newly_queued_scrobbles_are_durable_before_retry_metadata_exists() {
        let mut conn =
            init_queue(":memory:", queue_policy_for_tests()).expect("queue init should work");
        let pending = PendingScrobble {
            artist: "Artist".to_string(),
            title: "Track".to_string(),
            album: "Album".to_string(),
            played_at_unix: 1_700_000_000,
        };

        enqueue_scrobble(&mut conn, &pending, None).expect("enqueue should work");

        let queued = pop_queued_scrobbles(&mut conn, 1).expect("queue read should work");
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].title, pending.title);
        let (retry_count, last_error): (i64, Option<String>) = conn
            .query_row(
                "SELECT retry_count, last_error FROM scrobble_queue WHERE id = ?1",
                [queued[0].id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("queued row should remain persisted");
        assert_eq!(retry_count, 0);
        assert_eq!(last_error, None);
    }

    #[test]
    fn dead_letter_move_removes_rows_from_queue() {
        let mut conn =
            init_queue(":memory:", queue_policy_for_tests()).expect("queue init should work");
        let pending = PendingScrobble {
            artist: "Artist".to_string(),
            title: "Track".to_string(),
            album: "Album".to_string(),
            played_at_unix: 1_700_000_000,
        };

        enqueue_scrobble(&mut conn, &pending, Some("initial")).expect("enqueue should work");
        let queued = pop_queued_scrobbles(&mut conn, 10).expect("queue read should work");
        move_queued_scrobbles_to_dead_letter(
            &mut conn,
            &queued,
            "non_retriable_failure",
            Some("fatal"),
        )
        .expect("dead letter move should work");

        assert!(!queue_has_items(&conn).expect("queue should be empty"));
        let moved_count: i64 = conn
            .query_row("SELECT COUNT(1) FROM dead_letter_scrobbles", [], |row| {
                row.get(0)
            })
            .expect("dead letter count query should work");
        assert_eq!(moved_count, 1);
    }

    #[test]
    fn queue_retention_applies_ttl_capacity_and_dead_letter_ttl() {
        let mut conn =
            init_queue(":memory:", queue_policy_for_tests()).expect("queue init should work");
        let now = super::current_unix_timestamp() as i64;
        let old_created = now - 1_000;
        let recent_created = now - 5;

        conn.execute(
            "INSERT INTO scrobble_queue
                (artist, title, album, played_at_unix, retry_count, last_error, created_at_unix)
             VALUES
                ('a','old','x',1,0,NULL,?1),
                ('a','new-1','x',2,0,NULL,?2),
                ('a','new-2','x',3,0,NULL,?2),
                ('a','new-3','x',4,0,NULL,?2)",
            params![old_created, recent_created],
        )
        .expect("seed queue rows should work");
        conn.execute(
            "INSERT INTO dead_letter_scrobbles
                (queue_row_id, artist, title, album, played_at_unix, retry_count, last_error, created_at_unix, dead_letter_reason, dead_lettered_at_unix)
             VALUES
                (NULL,'d','old-dead','x',1,0,NULL,?1,'expired_ttl',?2)",
            params![old_created, old_created],
        )
        .expect("seed dead letter row should work");

        let policy = QueuePolicy {
            busy_timeout: Duration::from_secs(5),
            queue_ttl: Duration::from_secs(60),
            dead_letter_ttl: Duration::from_secs(60),
            max_queue_items: 2,
            max_retry_count: 3,
        };
        apply_queue_retention(&mut conn, &policy).expect("queue retention should work");

        let remaining_queue_titles = {
            let mut stmt = conn
                .prepare("SELECT title FROM scrobble_queue ORDER BY id ASC")
                .expect("prepare should work");
            stmt.query_map([], |row| row.get::<_, String>(0))
                .expect("query should work")
                .collect::<Result<Vec<_>, _>>()
                .expect("collect should work")
        };
        assert_eq!(
            remaining_queue_titles,
            vec!["new-2".to_string(), "new-3".to_string()]
        );

        let dead_letter_titles = {
            let mut stmt = conn
                .prepare("SELECT title FROM dead_letter_scrobbles ORDER BY id ASC")
                .expect("prepare should work");
            stmt.query_map([], |row| row.get::<_, String>(0))
                .expect("query should work")
                .collect::<Result<Vec<_>, _>>()
                .expect("collect should work")
        };
        assert_eq!(
            dead_letter_titles,
            vec!["old".to_string(), "new-1".to_string()]
        );
    }

    #[test]
    fn now_playing_channel_keeps_the_latest_update() {
        let (sender, mut receiver) = watch::channel(None);
        let first = TrackInfo {
            title: "First".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
        };
        let latest = TrackInfo {
            title: "Latest".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
        };

        send_now_playing(Some(&sender), &first);
        send_now_playing(Some(&sender), &latest);

        assert_eq!(receiver.borrow_and_update().as_ref(), Some(&latest));
    }

    #[test]
    fn source_arbiter_keeps_active_source_until_it_stops() {
        let mut arbiter = SourceArbiter::new();
        let shairport_playing = PlaybackSnapshot {
            title: "A".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            duration: Some(Duration::from_secs(120)),
            is_playing: true,
        };
        let mpd_playing = PlaybackSnapshot {
            title: "B".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            duration: Some(Duration::from_secs(90)),
            is_playing: true,
        };

        assert!(arbiter.ingest(&SourceEvent {
            source: SourceKind::Shairport,
            snapshot: shairport_playing.clone(),
        }));
        assert_eq!(arbiter.active_source, Some(SourceKind::Shairport));
        assert!(!arbiter.ingest(&SourceEvent {
            source: SourceKind::Mpd,
            snapshot: mpd_playing.clone(),
        }));
        assert_eq!(arbiter.active_source, Some(SourceKind::Shairport));

        assert!(!arbiter.ingest(&SourceEvent {
            source: SourceKind::Shairport,
            snapshot: PlaybackSnapshot {
                is_playing: false,
                ..shairport_playing
            },
        }));
        assert_eq!(arbiter.active_source, Some(SourceKind::Mpd));
        assert!(arbiter.ingest(&SourceEvent {
            source: SourceKind::Mpd,
            snapshot: mpd_playing,
        }));
    }

    #[test]
    fn source_switch_with_same_track_does_not_emit_duplicate_now_playing() {
        let state = Arc::new(Mutex::new(PlayerState::default()));
        let (sender, mut receiver) = watch::channel(None);
        let mut arbiter = SourceArbiter::new();

        let shared_track = PlaybackSnapshot {
            title: "Song".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            duration: Some(Duration::from_secs(180)),
            is_playing: true,
        };

        let shairport_event = SourceEvent {
            source: SourceKind::Shairport,
            snapshot: shared_track.clone(),
        };
        if arbiter.ingest(&shairport_event) {
            apply_playback_snapshot(&shairport_event.snapshot, &state, Some(&sender));
        }
        assert_eq!(
            receiver.borrow_and_update().as_ref().cloned(),
            Some(TrackInfo {
                title: "Song".to_string(),
                artist: "Artist".to_string(),
                album: "Album".to_string(),
            })
        );

        let shairport_stopped = SourceEvent {
            source: SourceKind::Shairport,
            snapshot: PlaybackSnapshot {
                is_playing: false,
                ..shared_track.clone()
            },
        };
        let _ = arbiter.ingest(&shairport_stopped);

        let mpd_event = SourceEvent {
            source: SourceKind::Mpd,
            snapshot: shared_track.clone(),
        };
        if arbiter.ingest(&mpd_event) {
            apply_playback_snapshot(&mpd_event.snapshot, &state, Some(&sender));
        }
        assert!(
            !receiver
                .has_changed()
                .expect("watch channel should be open")
        );
        assert_eq!(
            state.lock().expect("state lock should succeed").current,
            TrackInfo {
                title: "Song".to_string(),
                artist: "Artist".to_string(),
                album: "Album".to_string(),
            }
        );
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
    fn metadata_reset_clears_pending_track_and_duration() {
        let mut state = PlayerState::default();
        state.pending.title = "Queued song".to_string();
        state.pending.artist = "Queued artist".to_string();
        state.pending.album = "Queued album".to_string();
        state.pending_duration = Some(Duration::from_secs(90));

        let reset_item = MetadataItem {
            item_type: "ssnc".to_string(),
            item_code: "mdst".to_string(),
            decoded_text: String::new(),
            decoded_bytes: Vec::new(),
        };

        assert!(!apply_metadata_item(&reset_item, &mut state));
        assert_eq!(state.pending, super::TrackInfo::default());
        assert_eq!(state.pending_duration, None);
    }

    #[test]
    fn metadata_pause_and_resume_transitions_update_playing_state() {
        let mut state = PlayerState::default();
        state.is_playing = true;
        state.segment_started_at = Some(Instant::now() - Duration::from_millis(20));

        let pause_item = MetadataItem {
            item_type: "ssnc".to_string(),
            item_code: "pend".to_string(),
            decoded_text: String::new(),
            decoded_bytes: Vec::new(),
        };
        let resume_item = MetadataItem {
            item_type: "ssnc".to_string(),
            item_code: "prsm".to_string(),
            decoded_text: String::new(),
            decoded_bytes: Vec::new(),
        };

        assert!(!apply_metadata_item(&pause_item, &mut state));
        assert!(!state.is_playing);
        assert!(state.segment_started_at.is_none());
        assert!(state.played_duration >= Duration::from_millis(1));
        let played_after_pause = state.played_duration;

        assert!(!apply_metadata_item(&resume_item, &mut state));
        assert!(state.is_playing);
        assert!(state.segment_started_at.is_some());
        assert!(state.played_duration >= played_after_pause);
    }

    #[test]
    fn retry_queue_helpers_increment_and_delete_rows() {
        let mut conn =
            init_queue(":memory:", queue_policy_for_tests()).expect("queue init should work");

        for idx in 0..2 {
            let pending = PendingScrobble {
                artist: "A".to_string(),
                title: format!("Track-{idx}"),
                album: "Album".to_string(),
                played_at_unix: 1_000 + idx as u64,
            };
            enqueue_scrobble(&mut conn, &pending, Some("initial")).expect("enqueue should work");
        }

        let queued = pop_queued_scrobbles(&mut conn, 50).expect("read should work");
        increment_retry_count_for_queued_scrobbles(&mut conn, &queued, "timeout")
            .expect("retry increment should work");

        let rows = {
            let mut stmt = conn
                .prepare(
                    "SELECT retry_count, last_error
                     FROM scrobble_queue
                     ORDER BY id ASC",
                )
                .expect("query should prepare");
            stmt.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query should execute")
            .collect::<Result<Vec<_>, _>>()
            .expect("rows should collect")
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], (1, "timeout".to_string()));
        assert_eq!(rows[1], (1, "timeout".to_string()));

        delete_queued_scrobbles(&mut conn, &queued).expect("delete should work");
        assert!(!queue_has_items(&conn).expect("queue check should work"));
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
            let item = parse_xml_item(xml_item).expect("fixture metadata item should parse");
            if apply_metadata_item(&item, &mut state) {
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
