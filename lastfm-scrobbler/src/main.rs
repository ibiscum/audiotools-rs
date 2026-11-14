use base64::{Engine as _, engine::general_purpose::STANDARD};
use quick_xml::Reader;
use quick_xml::events::Event;
use rustfm_scrobble::{Scrobble, Scrobbler};
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Default, Clone, Debug, PartialEq)]
struct TrackInfo {
    title: String,
    artist: String,
    album: String,
}

struct PlayerState {
    pending: TrackInfo,
    current: TrackInfo,
    started_at: Option<Instant>,
    scrobbled: bool,
    is_playing: bool,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            pending: TrackInfo::default(),
            current: TrackInfo::default(),
            started_at: None,
            scrobbled: false,
            is_playing: true,
        }
    }
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

fn main() {
    let api_key = env::var("LASTFM_API_KEY").expect("LASTFM_API_KEY is missing");
    let api_secret = env::var("LASTFM_API_SECRET").expect("LASTFM_API_SECRET is missing");
    let username = env::var("LASTFM_USERNAME").expect("LASTFM_USERNAME is missing");
    let password = env::var("LASTFM_PASSWORD").expect("LASTFM_PASSWORD is missing");
    let pipe_path =
        env::var("SHAIRPORT_PIPE").unwrap_or_else(|_| "/tmp/shairport-sync-metadata".to_string());

    let mut scrobbler = Scrobbler::new(&api_key, &api_secret);
    scrobbler
        .authenticate_with_password(&username, &password)
        .expect("Last.fm authentication failed");
    println!("Successfully authenticated with Last.fm.");

    let scrobbler = Arc::new(Mutex::new(scrobbler));
    let state = Arc::new(Mutex::new(PlayerState::default()));

    let bg_state = Arc::clone(&state);
    let bg_scrobbler = Arc::clone(&scrobbler);
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(1));
            let mut st = bg_state.lock().unwrap();
            if st.is_playing
                && !st.scrobbled
                && !st.current.title.is_empty()
                && !st.current.artist.is_empty()
            {
                if let Some(started) = st.started_at {
                    if started.elapsed() >= Duration::from_secs(30) {
                        let track =
                            Scrobble::new(&st.current.artist, &st.current.title, &st.current.album);
                        if let Ok(scr) = bg_scrobbler.lock() {
                            match scr.scrobble(&track) {
                                Ok(_) => println!(
                                    "Scrobbled: {} - {}",
                                    st.current.artist, st.current.title
                                ),
                                Err(e) => eprintln!("Scrobble error: {:?}", e),
                            }
                        }
                        st.scrobbled = true;
                    }
                }
            }
        }
    });

    loop {
        println!("Opening pipe: {}", pipe_path);
        let file = match File::open(&pipe_path) {
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
                process_xml_item(&item_buffer, &state, &scrobbler);
                item_buffer.clear();
            }
            line.clear();
        }
    }
}

fn process_xml_item(xml: &str, state: &Arc<Mutex<PlayerState>>, scrobbler: &Arc<Mutex<Scrobbler>>) {
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
            Err(_) => return,
            _ => {}
        }
    }

    let t_str = match hex_to_fourcc(&item_type) {
        Some(v) => v,
        None => return,
    };
    let c_str = match hex_to_fourcc(&item_code) {
        Some(v) => v,
        None => return,
    };

    let decoded_text = if !data_b64.is_empty() {
        STANDARD
            .decode(data_b64.trim())
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .unwrap_or_default()
    } else {
        String::new()
    };

    let mut st = state.lock().unwrap();

    match (t_str.as_str(), c_str.as_str()) {
        ("core", "minm") => st.pending.title = decoded_text,
        ("core", "asar") => st.pending.artist = decoded_text,
        ("core", "asal") => st.pending.album = decoded_text,
        ("ssnc", "mdst") => {
            st.pending = TrackInfo::default();
        }
        ("ssnc", "mden") => {
            if !st.pending.title.is_empty() && st.pending != st.current {
                st.current = st.pending.clone();
                st.started_at = Some(Instant::now());
                st.scrobbled = false;
                st.is_playing = true;

                println!(
                    "Now Playing: {} - {} ({})",
                    st.current.artist, st.current.title, st.current.album
                );
                let track = Scrobble::new(&st.current.artist, &st.current.title, &st.current.album);
                if let Ok(scr) = scrobbler.lock() {
                    let _ = scr.now_playing(&track);
                }
            }
        }
        ("ssnc", "pend") | ("ssnc", "pfls") => {
            st.is_playing = false;
        }
        ("ssnc", "prsm") | ("ssnc", "pbeg") => {
            st.is_playing = true;
        }
        _ => {}
    }
}
