use env_logger::{Builder, Env, Target};
use id3::frame::PictureType as Id3PictureType;
use id3::TagLike;
use image::image_dimensions;
use image::{GenericImageView, load_from_memory};
use log::{debug, error, info, trace, warn};
use metaflac::block::PictureType;
use metaflac::Tag;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Default)]
struct DirAudit {
    has_audio_files: bool,
    has_front_jpg_1000: bool,
    has_folder_jpg_1000: bool,
    total_tracks: usize,
    tracks_with_front_cover_1000: usize,
}

#[derive(Default)]
struct SummaryStats {
    total_tracks: usize,
    with_art: usize,
    missing_art: usize,
    with_front_cover_1000: usize,
    missing_front_cover_1000: usize,
}

struct DirectoryScan {
    has_audio_files: bool,
    has_front_jpg_1000: bool,
    has_folder_jpg_1000: bool,
    image_files: Vec<(String, String, u32, u32)>,
}

fn print_usage() {
    info!("Usage: maintain_album_art [--skip-complete] <directory-to-scan>");
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

fn parse_args() -> (String, bool) {
    let mut directory = None;
    let mut skip_complete_in_summary = false;

    for arg in env::args().skip(1) {
        match arg.as_str() {
            "--skip-complete" => skip_complete_in_summary = true,
            "-h" | "--help" => {
                info!("Usage: maintain_album_art [--skip-complete] <directory-to-scan>");
                info!(
                    "\nOptions:\n  --skip-complete    Omit fully complete directories and OK tracks from summary output"
                );
                std::process::exit(0);
            }
            _ if arg.starts_with('-') => {
                error!("Unknown option: {}", arg);
                print_usage();
                std::process::exit(1);
            }
            _ => {
                if directory.is_some() {
                    error!("Multiple directories provided. Please pass exactly one.");
                    print_usage();
                    std::process::exit(1);
                }
                directory = Some(arg);
            }
        }
    }

    let Some(directory) = directory else {
        print_usage();
        std::process::exit(1);
    };

    (directory, skip_complete_in_summary)
}

fn is_audio_extension(ext: &str) -> bool {
    ext == "flac" || ext == "mp3"
}

fn is_image_extension(ext: &str) -> bool {
    ext == "jpg" || ext == "jpeg" || ext == "png"
}

fn scan_directory(path: &Path) -> Option<DirectoryScan> {
    match fs::read_dir(path) {
        Ok(entries) => {
            let mut image_files = Vec::new();
            let mut has_front_jpg_1000 = false;
            let mut has_folder_jpg_1000 = false;
            let mut has_audio_files = false;

            for child_result in entries {
                let child = match child_result {
                    Ok(entry) => entry,
                    Err(err) => {
                        warn!(
                            "Could not read entry in directory '{}': {}",
                            path.display(),
                            err
                        );
                        continue;
                    }
                };
                let child_path = child.path();
                if !child_path.is_file() {
                    continue;
                }

                let file_type = child_path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|s| s.to_ascii_lowercase());

                let Some(file_type) = file_type else {
                    continue;
                };

                if is_audio_extension(&file_type) {
                    has_audio_files = true;
                }

                if !is_image_extension(&file_type) {
                    continue;
                }

                match image_dimensions(&child_path) {
                    Ok((width, height)) => {
                        let file_name = child_path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string();
                        let file_name_lower = file_name.to_ascii_lowercase();
                        if file_name_lower == "front.jpg" && width == 1000 && height == 1000 {
                            has_front_jpg_1000 = true;
                        }
                        if file_name_lower == "folder.jpg" && width >= 1000 && height >= 1000 {
                            has_folder_jpg_1000 = true;
                        }
                        image_files.push((file_name, file_type, width, height));
                    }
                    Err(err) => {
                        warn!(
                            "Could not read dimensions for image file '{}': {}",
                            child_path.display(),
                            err
                        );
                    }
                }
            }

            Some(DirectoryScan {
                has_audio_files,
                has_front_jpg_1000,
                has_folder_jpg_1000,
                image_files,
            })
        }
        Err(err) => {
            error!(
                "Could not read directory '{}': {}",
                path.display(),
                err
            );
            None
        }
    }
}

fn process_directory(
    path: &Path,
    depth: usize,
    dir_audits: &mut BTreeMap<PathBuf, DirAudit>,
) {
    let Some(scan) = scan_directory(path) else {
        return;
    };

    let audit = dir_audits.entry(path.to_path_buf()).or_default();
    audit.has_audio_files = scan.has_audio_files;
    audit.has_front_jpg_1000 = scan.has_front_jpg_1000;
    audit.has_folder_jpg_1000 = scan.has_folder_jpg_1000;

    let skip_dir_level_report = scan.has_front_jpg_1000 && scan.has_folder_jpg_1000;
    if scan.has_audio_files && depth > 0 {
        info!("{}", "-".repeat(30));
        info!("{}", path.display());
    }

    if !skip_dir_level_report && !scan.image_files.is_empty() {
        info!("[DIR IMAGES] {}", path.display());
        for (name, img_type, width, height) in scan.image_files {
            info!(
                "   - name='{}' type='{}' dimensions={}×{}",
                name, img_type, width, height
            );
        }
    }
}

fn process_flac_file(
    path: &Path,
    root_path: &Path,
    skip_complete_in_summary: bool,
    stats: &mut SummaryStats,
    dir_audits: &mut BTreeMap<PathBuf, DirAudit>,
) {
    stats.total_tracks += 1;

    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    debug!("Processing FLAC: {}", file_name);
    let parent_dir = path.parent().unwrap_or(root_path).to_path_buf();

    let audit = dir_audits.entry(parent_dir).or_default();
    audit.has_audio_files = true;
    audit.total_tracks += 1;

    match Tag::read_from_path(path) {
        Ok(tag) => {
            let album_artist = tag
                .get_vorbis("ALBUMARTIST")
                .and_then(|mut values| values.next())
                .unwrap_or("<missing>");
            let album = tag
                .get_vorbis("ALBUM")
                .and_then(|mut values| values.next())
                .unwrap_or("<missing>");
            debug!(
                "   [TAGS] album_artist='{}' album='{}'",
                album_artist, album
            );

            let pictures: Vec<_> = tag.pictures().collect();
            let art_count = pictures.len();
            let has_front_cover_1000 = pictures.iter().any(|pic| {
                pic.picture_type == PictureType::CoverFront
                    && pic.width == 1000
                    && pic.height == 1000
            });

            if art_count > 0 {
                stats.with_art += 1;
                if has_front_cover_1000 {
                    stats.with_front_cover_1000 += 1;
                    audit.tracks_with_front_cover_1000 += 1;
                    if !skip_complete_in_summary {
                        info!("[FRONT 1000x1000] {}", file_name);
                    }
                } else {
                    stats.missing_front_cover_1000 += 1;
                    let dimensions = pictures
                        .iter()
                        .map(|pic| format!("{}x{}", pic.width, pic.height))
                        .collect::<Vec<_>>()
                        .join(", ");
                    warn!(
                        "[HAS ART, NO FRONT 1000x1000] {} ({} image(s), dimensions: {})",
                        file_name, art_count, dimensions
                    );
                }
            } else {
                stats.missing_art += 1;
                stats.missing_front_cover_1000 += 1;
                warn!("[MISSING] {}", file_name);
            }
        }
        Err(err) => {
            error!("Could not read metadata for: {}", file_name);
            debug!("FLAC metadata error for '{}': {}", path.display(), err);
            stats.missing_art += 1;
            stats.missing_front_cover_1000 += 1;
        }
    }
}

fn picture_dimensions_from_apic(data: &[u8]) -> Option<(u32, u32)> {
    let dimensions = load_from_memory(data).ok().map(|img| img.dimensions());
    trace!("Decoded APIC dimensions: {:?}", dimensions);
    dimensions
}

fn process_mp3_file(
    path: &Path,
    root_path: &Path,
    skip_complete_in_summary: bool,
    stats: &mut SummaryStats,
    dir_audits: &mut BTreeMap<PathBuf, DirAudit>,
) {
    stats.total_tracks += 1;

    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    debug!("Processing MP3: {}", file_name);
    let parent_dir = path.parent().unwrap_or(root_path).to_path_buf();

    let audit = dir_audits.entry(parent_dir).or_default();
    audit.has_audio_files = true;
    audit.total_tracks += 1;

    match id3::Tag::read_from_path(path) {
        Ok(tag) => {
            let album_artist = tag.album_artist().unwrap_or("<missing>");
            let album = tag.album().unwrap_or("<missing>");
            debug!(
                "   [TAGS] album_artist='{}' album='{}'",
                album_artist, album
            );

            let pictures: Vec<_> = tag.pictures().collect();
            let art_count = pictures.len();
            let has_front_cover_1000 = pictures.iter().any(|pic| {
                pic.picture_type == Id3PictureType::CoverFront
                    && picture_dimensions_from_apic(&pic.data) == Some((1000, 1000))
            });

            if art_count > 0 {
                stats.with_art += 1;
                if has_front_cover_1000 {
                    stats.with_front_cover_1000 += 1;
                    audit.tracks_with_front_cover_1000 += 1;
                    if !skip_complete_in_summary {
                        info!("[FRONT 1000x1000] {}", file_name);
                    }
                } else {
                    stats.missing_front_cover_1000 += 1;
                    let dimensions = pictures
                        .iter()
                        .map(|pic| {
                            picture_dimensions_from_apic(&pic.data)
                                .map(|(w, h)| format!("{}x{}", w, h))
                                .unwrap_or_else(|| "unknown".to_string())
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    warn!(
                        "[HAS ART, NO FRONT 1000x1000] {} ({} image(s), dimensions: {})",
                        file_name, art_count, dimensions
                    );
                }
            } else {
                stats.missing_art += 1;
                stats.missing_front_cover_1000 += 1;
                warn!("[MISSING] {}", file_name);
            }
        }
        Err(err) => {
            error!("Could not read metadata for: {}", file_name);
            debug!("MP3 metadata error for '{}': {}", path.display(), err);
            stats.missing_art += 1;
            stats.missing_front_cover_1000 += 1;
        }
    }
}

fn print_final_summary(
    stats: &SummaryStats,
    dir_audits: &BTreeMap<PathBuf, DirAudit>,
    skip_complete_in_summary: bool,
) {
    info!("\n=== Summary ===");
    info!("Total audio files found: {}", stats.total_tracks);
    info!("Files with cover art:  {}", stats.with_art);
    info!("Files missing art:     {}", stats.missing_art);
    if !skip_complete_in_summary {
        info!(
            "Files with FRONT 1000x1000:    {}",
            stats.with_front_cover_1000
        );
    }
    info!(
        "Files without FRONT 1000x1000: {}",
        stats.missing_front_cover_1000
    );

    info!("\n=== Directory Summary ===");
    let mut shown_dirs = 0;
    for (path, audit) in dir_audits {
        if !audit.has_audio_files {
            continue;
        }

        let complete_ok = audit.has_front_jpg_1000
            && audit.has_folder_jpg_1000
            && audit.total_tracks > 0
            && audit.total_tracks == audit.tracks_with_front_cover_1000;

        if skip_complete_in_summary && complete_ok {
            continue;
        }

        shown_dirs += 1;
        info!("{}", path.display());
        info!(
            "  status: {}",
            if complete_ok { "COMPLETE OK" } else { "INCOMPLETE" }
        );
        info!(
            "  front.jpg 1000x1000: {}",
            if audit.has_front_jpg_1000 { "yes" } else { "no" }
        );
        info!(
            "  folder.jpg >=1000x1000: {}",
            if audit.has_folder_jpg_1000 { "yes" } else { "no" }
        );
        let missing_front_cover_1000 = audit.total_tracks - audit.tracks_with_front_cover_1000;
        if skip_complete_in_summary {
            if missing_front_cover_1000 > 0 {
                info!(
                    "  tracks without FRONT 1000x1000: {}",
                    missing_front_cover_1000
                );
            }
        } else {
            info!(
                "  tracks with FRONT 1000x1000: {}/{}",
                audit.tracks_with_front_cover_1000, audit.total_tracks
            );
        }
    }

    if shown_dirs == 0 {
        info!("No directories to show in summary.");
    }
}

fn main() {
    init_logging();
    let (dir_path, skip_complete_in_summary) = parse_args();
    let dir_path = PathBuf::from(dir_path);

    info!(
        "Current working directory: {}",
        env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<unavailable>".to_string())
    );

    if !dir_path.is_dir() {
        error!("'{}' is not a valid directory.", dir_path.display());
        std::process::exit(1);
    }

    info!("Scanning directory: {}\n", dir_path.display());
    if skip_complete_in_summary {
        info!("Summary mode: skipping fully complete directories\n");
    }

    let mut stats = SummaryStats::default();
    let mut dir_audits: BTreeMap<PathBuf, DirAudit> = BTreeMap::new();

    for entry_result in WalkDir::new(&dir_path) {
        let entry = match entry_result {
            Ok(entry) => entry,
            Err(err) => {
                warn!("Skipping unreadable filesystem entry: {}", err);
                continue;
            }
        };
        let path = entry.path();
        trace!("WalkDir entry: {}", path.display());

        if path.is_dir() {
            process_directory(path, entry.depth(), &mut dir_audits);
        }

        if path.is_file() {
            let ext = path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_ascii_lowercase());
            match ext.as_deref() {
                Some("flac") => process_flac_file(
                    path,
                    &dir_path,
                    skip_complete_in_summary,
                    &mut stats,
                    &mut dir_audits,
                ),
                Some("mp3") => process_mp3_file(
                    path,
                    &dir_path,
                    skip_complete_in_summary,
                    &mut stats,
                    &mut dir_audits,
                ),
                _ => {}
            }
        }
    }

    print_final_summary(&stats, &dir_audits, skip_complete_in_summary);
}
