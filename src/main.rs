use eframe::egui;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::fs;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Debug, Clone)]
struct MovieMetadata {
    title: String,
    url: String,
}

#[derive(Clone)]
struct MovieCacheInfo {
    dir_name: String,
    metadata: Option<MovieMetadata>,
    total_size_bytes: u64,
}

struct TorrentStatus {
    speed: String,
    downloaded: String,
    peers: String,
    active: bool,
}

#[derive(Debug)]
enum BValue {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<BValue>),
    Dict(std::collections::BTreeMap<Vec<u8>, BValue>),
}

const PROGRESS_VERIFIER_VERSION: u64 = 2;

fn get_cache_dir() -> PathBuf {
    PathBuf::from("./stream_cache")
}

// Helper to extract a 40-character SHA1 infohash from a torrent or magnet link
fn get_infohash(url: &str) -> Option<String> {
    for part in url.split(&['/', '?', '=', '&', ':', '-'][..]) {
        if part.len() == 40 && part.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(part.to_uppercase());
        }
    }
    None
}

// Convert any direct torrent link containing a 40-char infohash into a robust magnet URI to bypass Cloudflare website blocks
fn get_magnet_uri(url: &str) -> String {
    if url.starts_with("magnet:") {
        return url.to_string();
    }

    if let Some(h) = get_infohash(url) {
        // Construct a magnet URI with popular public trackers
        format!(
            "magnet:?xt=urn:btih:{}&dn=Movie&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce&tr=udp%3A%2F%2Fopen.demonii.com%3A1337%2Fannounce&tr%3A%2F%2Ftracker.coppersurfer.tk%3A6969%2Fannounce&tr=udp%3A%2F%2Ftracker.leechers-paradise.org%3A6969%2Fannounce",
            h
        )
    } else {
        url.to_string()
    }
}

// Helper to locate the largest media file inside a directory recursively
fn find_media_file(dir: &std::path::Path) -> Option<PathBuf> {
    let mut largest_file = None;
    let mut max_size = 0;

    fn visit_dirs(dir: &std::path::Path, largest: &mut Option<PathBuf>, max_sz: &mut u64) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    visit_dirs(&path, largest, max_sz);
                } else if path.is_file() {
                    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                        let ext_lower = ext.to_lowercase();
                        if ["mp4", "mkv", "avi", "m4v", "mov", "flv", "webm"]
                            .contains(&ext_lower.as_str())
                        {
                            if let Ok(meta) = path.metadata() {
                                if meta.len() > *max_sz {
                                    *max_sz = meta.len();
                                    *largest = Some(path.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    visit_dirs(dir, &mut largest_file, &mut max_size);
    largest_file
}

// Simple line parser to extract speed, downloaded bytes, and peer count from WebTorrent CLI stdout logs
fn parse_webtorrent_line(line: &str) -> Option<(String, String, String)> {
    if let Some(speed_idx) = line.find("Speed:") {
        let speed_part = &line[speed_idx + 6..];
        let dl_idx = speed_part.find("Downloaded:");

        let speed = if let Some(idx) = dl_idx {
            speed_part[..idx].trim().to_string()
        } else {
            "".to_string()
        };

        let dl_part = if let Some(idx) = dl_idx {
            &speed_part[idx + 11..]
        } else {
            ""
        };

        let up_idx = dl_part.find("Uploaded:");
        let downloaded = if let Some(idx) = up_idx {
            dl_part[..idx].trim().to_string()
        } else {
            "".to_string()
        };

        let peers_idx = line.find("Peers:");
        let peers = if let Some(idx) = peers_idx {
            line[idx + 6..].trim().to_string()
        } else {
            "0".to_string()
        };

        return Some((speed, downloaded, peers));
    }
    None
}

fn scan_caches() -> Vec<MovieCacheInfo> {
    let cache_dir = get_cache_dir();
    let mut movies = Vec::new();

    if !cache_dir.exists() {
        return movies;
    }

    if let Ok(entries) = fs::read_dir(cache_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let dir_name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();

                let metadata_path = path.join("metadata.json");
                let metadata: Option<MovieMetadata> = if metadata_path.exists() {
                    fs::read_to_string(metadata_path)
                        .ok()
                        .and_then(|content| serde_json::from_str(&content).ok())
                } else {
                    None
                };

                let mut total_size_bytes = 0;

                // Recursive helper to scan directory size (handles Torrent nested files)
                fn scan_dir_size(dir_path: &std::path::Path, size_bytes: &mut u64) {
                    if let Ok(files) = fs::read_dir(dir_path) {
                        for file in files.flatten() {
                            let file_path = file.path();
                            if file_path.is_file() {
                                if let Ok(meta) = file.metadata() {
                                    *size_bytes += meta.len();
                                }
                            } else if file_path.is_dir() {
                                scan_dir_size(&file_path, size_bytes);
                            }
                        }
                    }
                }

                scan_dir_size(&path, &mut total_size_bytes);

                movies.push(MovieCacheInfo {
                    dir_name,
                    metadata,
                    total_size_bytes,
                });
            }
        }
    }

    movies
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn parse_size_to_bytes(text: &str) -> Option<u64> {
    let mut number = String::new();
    let mut unit = String::new();
    let mut seen_number = false;

    for c in text.chars() {
        if c.is_ascii_digit() || c == '.' {
            number.push(c);
            seen_number = true;
        } else if seen_number && c.is_ascii_alphabetic() {
            unit.push(c);
        } else if seen_number && !unit.is_empty() {
            break;
        }
    }

    let value: f64 = number.parse().ok()?;
    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "b" | "byte" | "bytes" => 1.0,
        "kb" | "kib" => 1024.0,
        "mb" | "mib" => 1024.0 * 1024.0,
        "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };

    Some((value * multiplier).round() as u64)
}

fn write_torrent_progress(dest_dir: &std::path::Path, downloaded: &str) {
    let mut size_parts = downloaded.splitn(2, '/');
    let downloaded_part = size_parts.next().unwrap_or(downloaded);
    let total_part = size_parts.next();

    if let Some(downloaded_bytes) = parse_size_to_bytes(downloaded_part) {
        let progress_path = dest_dir.join(".torrent_progress.json");
        let mut progress_json = fs::read_to_string(&progress_path)
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        progress_json.insert(
            "downloaded_bytes".to_string(),
            serde_json::json!(downloaded_bytes),
        );
        progress_json.remove("playable_prefix_ratio");
        let has_verified_ranges = progress_json
            .get("ranges")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|ranges| !ranges.is_empty());

        let total_bytes = total_part.and_then(parse_size_to_bytes).or_else(|| {
            find_media_file(dest_dir).and_then(|path| path.metadata().ok().map(|m| m.len()))
        });

        if !has_verified_ranges {
            if let Some(total_bytes) = total_bytes {
                progress_json.insert("total_bytes".to_string(), serde_json::json!(total_bytes));
            }
        }
        if let Ok(content) = serde_json::to_string(&serde_json::Value::Object(progress_json)) {
            let _ = fs::write(progress_path, content);
        }
    }
}

fn parse_bencode(data: &[u8]) -> Option<BValue> {
    fn parse_value(data: &[u8], pos: &mut usize) -> Option<BValue> {
        match data.get(*pos)? {
            b'i' => {
                *pos += 1;
                let start = *pos;
                while *data.get(*pos)? != b'e' {
                    *pos += 1;
                }
                let value = std::str::from_utf8(&data[start..*pos]).ok()?.parse().ok()?;
                *pos += 1;
                Some(BValue::Int(value))
            }
            b'l' => {
                *pos += 1;
                let mut values = Vec::new();
                while *data.get(*pos)? != b'e' {
                    values.push(parse_value(data, pos)?);
                }
                *pos += 1;
                Some(BValue::List(values))
            }
            b'd' => {
                *pos += 1;
                let mut values = std::collections::BTreeMap::new();
                while *data.get(*pos)? != b'e' {
                    let key = match parse_value(data, pos)? {
                        BValue::Bytes(bytes) => bytes,
                        _ => return None,
                    };
                    let value = parse_value(data, pos)?;
                    values.insert(key, value);
                }
                *pos += 1;
                Some(BValue::Dict(values))
            }
            b'0'..=b'9' => {
                let start = *pos;
                while *data.get(*pos)? != b':' {
                    *pos += 1;
                }
                let len: usize = std::str::from_utf8(&data[start..*pos]).ok()?.parse().ok()?;
                *pos += 1;
                let end = *pos + len;
                if end > data.len() {
                    return None;
                }
                let bytes = data[*pos..end].to_vec();
                *pos = end;
                Some(BValue::Bytes(bytes))
            }
            _ => None,
        }
    }

    let mut pos = 0;
    parse_value(data, &mut pos)
}

fn bdict_get<'a>(
    dict: &'a std::collections::BTreeMap<Vec<u8>, BValue>,
    key: &[u8],
) -> Option<&'a BValue> {
    dict.get(key)
}

fn bvalue_int(value: &BValue) -> Option<u64> {
    match value {
        BValue::Int(v) if *v >= 0 => Some(*v as u64),
        _ => None,
    }
}

fn bvalue_bytes(value: &BValue) -> Option<&[u8]> {
    match value {
        BValue::Bytes(bytes) => Some(bytes),
        _ => None,
    }
}

fn bvalue_list(value: &BValue) -> Option<&[BValue]> {
    match value {
        BValue::List(values) => Some(values),
        _ => None,
    }
}

fn bvalue_dict(value: &BValue) -> Option<&std::collections::BTreeMap<Vec<u8>, BValue>> {
    match value {
        BValue::Dict(values) => Some(values),
        _ => None,
    }
}

#[derive(Clone)]
struct TorrentFileEntry {
    path: PathBuf,
    start: u64,
    len: u64,
}

fn torrent_files_from_info(
    cache_dir: &std::path::Path,
    info: &std::collections::BTreeMap<Vec<u8>, BValue>,
) -> Option<Vec<TorrentFileEntry>> {
    let name = bdict_get(info, b"name")
        .and_then(bvalue_bytes)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())?;

    if let Some(length) = bdict_get(info, b"length").and_then(bvalue_int) {
        return Some(vec![TorrentFileEntry {
            path: cache_dir.join(name),
            start: 0,
            len: length,
        }]);
    }

    let files = bdict_get(info, b"files").and_then(bvalue_list)?;
    let mut entries = Vec::new();
    let mut offset = 0;

    for file_value in files {
        let file = bvalue_dict(file_value)?;
        let length = bdict_get(file, b"length").and_then(bvalue_int)?;
        let path_parts = bdict_get(file, b"path").and_then(bvalue_list)?;
        let mut path = cache_dir.join(name);
        for part in path_parts {
            let part = bvalue_bytes(part).and_then(|bytes| std::str::from_utf8(bytes).ok())?;
            path.push(part);
        }

        entries.push(TorrentFileEntry {
            path,
            start: offset,
            len: length,
        });
        offset += length;
    }

    Some(entries)
}

fn read_torrent_range(files: &[TorrentFileEntry], start: u64, len: u64) -> Option<Vec<u8>> {
    let end = start.checked_add(len)?;
    let mut out = Vec::with_capacity(len as usize);

    for entry in files {
        let file_start = entry.start;
        let file_end = entry.start.checked_add(entry.len)?;
        let overlap_start = start.max(file_start);
        let overlap_end = end.min(file_end);
        if overlap_start >= overlap_end {
            continue;
        }

        let mut file = fs::File::open(&entry.path).ok()?;
        file.seek(SeekFrom::Start(overlap_start - file_start))
            .ok()?;
        let overlap_len = (overlap_end - overlap_start) as usize;
        let mut buf = vec![0; overlap_len];
        file.read_exact(&mut buf).ok()?;
        out.extend_from_slice(&buf);
    }

    if out.len() == len as usize {
        Some(out)
    } else {
        None
    }
}

fn file_len_and_mtime_ns(path: &std::path::Path) -> Option<(u64, u64)> {
    let metadata = path.metadata().ok()?;
    let modified = metadata.modified().ok()?;
    let mtime_ns = modified.duration_since(UNIX_EPOCH).ok()?.as_nanos() as u64;
    Some((metadata.len(), mtime_ns))
}

fn verified_progress_is_current(cache_dir: &std::path::Path, media_path: &std::path::Path) -> bool {
    let torrent_path = cache_dir.join("movie.torrent");
    let progress_path = progress_file_path(cache_dir);
    let (media_size, media_mtime_ns) = match file_len_and_mtime_ns(media_path) {
        Some(stamp) => stamp,
        None => return false,
    };
    let (torrent_size, torrent_mtime_ns) = match file_len_and_mtime_ns(&torrent_path) {
        Some(stamp) => stamp,
        None => return false,
    };

    let progress = match fs::read_to_string(progress_path)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
    {
        Some(progress) => progress,
        None => return false,
    };

    progress
        .get("verifier_version")
        .and_then(serde_json::Value::as_u64)
        == Some(PROGRESS_VERIFIER_VERSION)
        && progress
            .get("media_size_bytes")
            .and_then(serde_json::Value::as_u64)
            == Some(media_size)
        && progress
            .get("media_mtime_ns")
            .and_then(serde_json::Value::as_u64)
            == Some(media_mtime_ns)
        && progress
            .get("torrent_size_bytes")
            .and_then(serde_json::Value::as_u64)
            == Some(torrent_size)
        && progress
            .get("torrent_mtime_ns")
            .and_then(serde_json::Value::as_u64)
            == Some(torrent_mtime_ns)
        && progress
            .get("total_bytes")
            .and_then(serde_json::Value::as_u64)
            .is_some()
        && progress
            .get("downloaded_bytes")
            .and_then(serde_json::Value::as_u64)
            .is_some()
        && progress
            .get("ranges")
            .and_then(serde_json::Value::as_array)
            .is_some()
        && progress
            .get("playable_prefix_ratio")
            .and_then(serde_json::Value::as_f64)
            .is_some()
}

fn refresh_verified_torrent_progress_if_needed(
    cache_dir: &std::path::Path,
    media_path: &std::path::Path,
) {
    if !verified_progress_is_current(cache_dir, media_path) {
        write_verified_torrent_progress(cache_dir, media_path);
    }
}

fn write_verified_torrent_progress(cache_dir: &std::path::Path, media_path: &std::path::Path) {
    write_verified_torrent_progress_with_mode(cache_dir, media_path, true);
}

fn write_verified_torrent_ranges_progress(
    cache_dir: &std::path::Path,
    media_path: &std::path::Path,
) {
    write_verified_torrent_progress_with_mode(cache_dir, media_path, false);
}

fn write_verified_torrent_progress_with_mode(
    cache_dir: &std::path::Path,
    media_path: &std::path::Path,
    include_playable_probe: bool,
) {
    let torrent_path = cache_dir.join("movie.torrent");
    let (media_size, media_mtime_ns) = match file_len_and_mtime_ns(media_path) {
        Some(stamp) => stamp,
        None => return,
    };
    let (torrent_size, torrent_mtime_ns) = match file_len_and_mtime_ns(&torrent_path) {
        Some(stamp) => stamp,
        None => return,
    };
    let torrent_bytes = match fs::read(torrent_path) {
        Ok(bytes) => bytes,
        Err(_) => return,
    };

    let root = match parse_bencode(&torrent_bytes).and_then(|value| match value {
        BValue::Dict(dict) => Some(dict),
        _ => None,
    }) {
        Some(root) => root,
        None => return,
    };
    let info = match bdict_get(&root, b"info").and_then(bvalue_dict) {
        Some(info) => info,
        None => return,
    };
    let piece_len = match bdict_get(info, b"piece length").and_then(bvalue_int) {
        Some(piece_len) if piece_len > 0 => piece_len,
        _ => return,
    };
    let pieces = match bdict_get(info, b"pieces").and_then(bvalue_bytes) {
        Some(pieces) if pieces.len() % 20 == 0 => pieces,
        _ => return,
    };
    let files = match torrent_files_from_info(cache_dir, info) {
        Some(files) => files,
        None => return,
    };
    let total_torrent_len: u64 = files.iter().map(|file| file.len).sum();

    let media_abs = media_path
        .canonicalize()
        .unwrap_or_else(|_| media_path.to_path_buf());
    let target = match files.iter().find(|file| {
        file.path
            .canonicalize()
            .unwrap_or_else(|_| file.path.clone())
            == media_abs
    }) {
        Some(target) => target,
        None => return,
    };

    let mut verified_bytes = 0;
    let mut verified_ranges: Vec<(u64, u64)> = Vec::new();
    let target_start = target.start;
    let target_end = target.start + target.len;

    for (idx, expected) in pieces.chunks(20).enumerate() {
        let piece_start = idx as u64 * piece_len;
        let piece_end = (piece_start + piece_len).min(total_torrent_len);
        let overlap_start = piece_start.max(target_start);
        let overlap_end = piece_end.min(target_end);
        if overlap_start >= overlap_end {
            continue;
        }

        let piece_size = piece_end - piece_start;
        if let Some(piece_data) = read_torrent_range(&files, piece_start, piece_size) {
            let digest = Sha1::digest(&piece_data);
            if digest.as_slice() == expected {
                let rel_start = overlap_start - target_start;
                let rel_end = overlap_end - target_start;
                verified_ranges.push((rel_start, rel_end));
                verified_bytes += rel_end - rel_start;
            }
        }
    }

    verified_ranges.sort_by_key(|range| range.0);
    let mut merged_ranges: Vec<(u64, u64)> = Vec::new();
    for (start, end) in verified_ranges {
        if let Some(last) = merged_ranges.last_mut() {
            if start <= last.1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        merged_ranges.push((start, end));
    }

    let contiguous_prefix_end = merged_ranges
        .first()
        .and_then(|(start, end)| if *start == 0 { Some(*end) } else { None })
        .unwrap_or(0);

    let ranges_json: Vec<_> = merged_ranges
        .iter()
        .map(|(start, end)| serde_json::json!([start, end]))
        .collect();

    let mut progress_json = serde_json::json!({
        "verifier_version": PROGRESS_VERIFIER_VERSION,
        "media_size_bytes": media_size,
        "media_mtime_ns": media_mtime_ns,
        "torrent_size_bytes": torrent_size,
        "torrent_mtime_ns": torrent_mtime_ns,
        "downloaded_bytes": verified_bytes,
        "total_bytes": target.len,
        "contiguous_prefix_bytes": contiguous_prefix_end,
        "ranges": ranges_json,
    });
    if include_playable_probe {
        progress_json["playable_prefix_ratio"] =
            serde_json::json!(probe_mpv_playable_prefix(media_path));
    }
    if let Ok(content) = serde_json::to_string(&progress_json) {
        let _ = fs::write(progress_file_path(cache_dir), content);
    }
}

fn spawn_gap_aware_progress_scanner(dest_dir: PathBuf, torrent_status: Arc<Mutex<TorrentStatus>>) {
    thread::spawn(move || {
        let mut last_media_stamp = None;

        loop {
            let active = torrent_status
                .lock()
                .map(|status| status.active)
                .unwrap_or(false);
            if !active {
                break;
            }

            if let Some(media_path) = find_media_file(&dest_dir) {
                let media_stamp = file_len_and_mtime_ns(&media_path);
                if media_stamp.is_some() && media_stamp != last_media_stamp {
                    write_verified_torrent_ranges_progress(&dest_dir, &media_path);
                    last_media_stamp = media_stamp;
                }
            }

            thread::sleep(Duration::from_secs(2));
        }
    });
}

fn probe_mpv_playable_prefix(media_path: &std::path::Path) -> f64 {
    fn can_start_at(media_path: &std::path::Path, ratio: f64) -> bool {
        let start = format!("--start={:.3}%", ratio * 100.0);
        Command::new("mpv")
            .args([
                "--no-config",
                "--really-quiet",
                "--vo=null",
                "--ao=null",
                "--frames=120",
                "--no-resume-playback",
                &start,
                media_path.to_str().unwrap_or_default(),
            ])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    let mut low = 0.0;
    let mut high = 1.0;

    for _ in 0..8 {
        let mid = (low + high) / 2.0;
        if can_start_at(media_path, mid) {
            low = mid;
        } else {
            high = mid;
        }
    }

    (low * 0.95).clamp(0.0, 1.0)
}

fn mpv_script_path() -> Option<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join("torrent_cache_indicator.lua");
    path.to_str().map(|s| s.to_string())
}

fn progress_file_path(dest_dir: &std::path::Path) -> PathBuf {
    if dest_dir.is_absolute() {
        dest_dir.join(".torrent_progress.json")
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(dest_dir)
            .join(".torrent_progress.json")
    }
}

fn webtorrent_mpv_player_args(dest_dir: &std::path::Path) -> Option<String> {
    mpv_script_path().map(|script| {
        let progress_file = progress_file_path(dest_dir);
        format!(
            "--load-scripts=no --script={} --script-opts=osc-layout=bottombar,osc-seekbarstyle=bar,torrent_cache_indicator-progress_file={}",
            script,
            progress_file.to_string_lossy()
        )
    })
}

fn direct_mpv_args(progress_file: &std::path::Path) -> Vec<String> {
    let mut args = vec![
        "--load-scripts=no".to_string(),
        format!(
            "--script-opts=osc-layout=bottombar,osc-seekbarstyle=bar,torrent_cache_indicator-progress_file={}",
            progress_file.to_string_lossy()
        ),
    ];

    if let Some(script) = mpv_script_path() {
        args.push(format!("--script={}", script));
    }

    args
}

struct AppState {
    movies: Vec<MovieCacheInfo>,
    selected_idx: Option<usize>,
    new_movie_url: String,
    status_message: String,

    torrent_status: Arc<Mutex<TorrentStatus>>,
    spawned_children: Arc<Mutex<Vec<std::process::Child>>>,
}

impl AppState {
    fn new(ctx: egui::Context) -> Self {
        let movies = scan_caches();
        let selected_idx = if movies.is_empty() { None } else { Some(0) };

        let torrent_status = Arc::new(Mutex::new(TorrentStatus {
            speed: "0 B/s".to_string(),
            downloaded: "0 MB".to_string(),
            peers: "0/0".to_string(),
            active: false,
        }));
        let spawned_children = Arc::new(Mutex::new(Vec::new()));

        // Spawn background polling thread to repaint the UI periodically (every 1 second) to capture real-time file size increases
        thread::spawn(move || {
            loop {
                ctx.request_repaint();
                thread::sleep(std::time::Duration::from_secs(1));
            }
        });

        Self {
            movies,
            selected_idx,
            new_movie_url: String::new(),
            status_message: "Ready".to_string(),
            torrent_status,
            spawned_children,
        }
    }

    fn refresh(&mut self) {
        self.movies = scan_caches();
        if let Some(idx) = self.selected_idx {
            if idx >= self.movies.len() {
                self.selected_idx = if self.movies.is_empty() {
                    None
                } else {
                    Some(0)
                };
            }
        } else if !self.movies.is_empty() {
            self.selected_idx = Some(0);
        }
    }
}

impl eframe::App for AppState {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Kill any background child processes spawned by this application instance
        let mut children = self.spawned_children.lock().unwrap();
        println!("Cleaning up {} spawned child processes...", children.len());
        for child in children.iter_mut() {
            let _ = child.kill();
        }
        // Force SIGKILL to clear all orphaned Node/webtorrent components
        let _ = Command::new("pkill")
            .args(&["-9", "-i", "-f", "webtorrent"])
            .status();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Set custom visual aesthetics
        let mut style = (*ctx.style()).clone();
        style.visuals.dark_mode = true;
        style.visuals.override_text_color = Some(egui::Color32::from_rgb(220, 220, 225));
        style.visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(24, 24, 28);
        style.visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(36, 36, 42);
        style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(48, 48, 56);
        style.visuals.widgets.active.bg_fill = egui::Color32::from_rgb(60, 60, 70);
        ctx.set_style(style);

        // Header Panel
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(8.0);
                ui.heading(
                    egui::RichText::new("🎬 Film Torrent Streaming Manager")
                        .font(egui::FontId::proportional(22.0))
                        .strong(),
                );
                ui.add_space(8.0);
            });
        });

        // Bottom Status / Controller Panel
        egui::TopBottomPanel::bottom("bottom_panel").show(ctx, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Torrent / Magnet URL:").strong());
                let _text_edit = ui.add(
                    egui::TextEdit::singleline(&mut self.new_movie_url)
                        .hint_text("Paste magnet link OR torrent download URL...")
                        .desired_width(400.0)
                );

                if ui.button("🧲 Stream Torrent").clicked() {
                    let url = self.new_movie_url.trim().to_string();
                    if !url.is_empty() {
                        self.status_message = format!("Resolving torrent metadata...");

                        // Kill any previously running player/proxy sessions first
                        let children_clone = self.spawned_children.clone();
                        let torrent_status_clone = self.torrent_status.clone();
                        let ctx_clone = ctx.clone();
                        thread::spawn(move || {
                            let mut children = children_clone.lock().unwrap();
                            for child in children.iter_mut() {
                                let _ = child.kill();
                            }
                            children.clear();

                            // Free standard ports
                            let _ = Command::new("pkill").args(&["-9", "-i", "-f", "webtorrent"]).status();

                            // Extract hash/slug to create unique cache folder
                            let hash = get_infohash(&url).unwrap_or_else(|| {
                                let cleaned: String = url.chars()
                                    .map(|c| if c.is_alphanumeric() { c } else { '_' })
                                    .collect();
                                if cleaned.len() > 30 { cleaned[cleaned.len()-30..].to_string() } else { cleaned }
                            });
                            let dir_name = format!("torrent_{}", hash);
                            let dest_dir = get_cache_dir().join(&dir_name);
                            let _ = fs::create_dir_all(&dest_dir);

                            // Convert torrent link to raw magnet link to bypass Cloudflare blocks
                            let magnet_uri = get_magnet_uri(&url);

                            // Write metadata.json for this torrent cache to integrate with GUI
                            let meta_path = dest_dir.join("metadata.json");
                            let meta_json = serde_json::json!({
                                "title": format!("Torrent: {}", hash),
                                "url": magnet_uri,
                            });
                            if let Ok(content) = serde_json::to_string_pretty(&meta_json) {
                                let _ = fs::write(meta_path, content);
                            }

                            // Download .torrent file directly to bypass Cloudflare web block and bootstrap metadata instantly
                            let mut torrent_source = url.clone();
                            if let Some(h) = get_infohash(&url) {
                                let torrent_url = format!("https://itorrents.net/torrent/{}.torrent", h);
                                let local_torrent = dest_dir.join("movie.torrent");

                                println!("[TORRENT] Ensuring cache directory exists: {:?}", dest_dir);
                                let _ = fs::create_dir_all(&dest_dir);

                                println!("[TORRENT] Downloading .torrent metadata file: {}", torrent_url);
                                let download_res = Command::new("curl")
                                    .args(&["-s", "-L", "-o", local_torrent.to_str().unwrap(), &torrent_url])
                                    .status();

                                if download_res.is_ok() && local_torrent.exists() {
                                    println!("[TORRENT] Metadata loaded successfully. Launching WebTorrent player...");
                                    torrent_source = local_torrent.to_str().unwrap().to_string();
                                }
                            }

                            // Initialize active status
                            {
                                let mut status = torrent_status_clone.lock().unwrap();
                                status.active = true;
                                status.speed = "Connecting...".to_string();
                                status.downloaded = "0 MB".to_string();
                                status.peers = "0/0".to_string();
                            }
                            ctx_clone.request_repaint();

                            write_torrent_progress(&dest_dir, "0 B");
                            spawn_gap_aware_progress_scanner(
                                dest_dir.clone(),
                                torrent_status_clone.clone(),
                            );

                            let mut cmd = Command::new("npx");
                            cmd.args(&["-y", "webtorrent-cli", "download", &torrent_source, "--mpv", "--out", dest_dir.to_str().unwrap()]);
                            if let Some(player_args) = webtorrent_mpv_player_args(&dest_dir) {
                                cmd.arg(format!("--player-args={player_args}"));
                            }
                            cmd.stdout(std::process::Stdio::piped());
                            cmd.stderr(std::process::Stdio::piped());

                            if let Ok(mut child) = cmd.spawn() {
                                if let Some(stderr) = child.stderr.take() {
                                    let err_reader = std::io::BufReader::new(stderr);
                                    thread::spawn(move || {
                                        for line_res in err_reader.lines().flatten() {
                                            if !line_res.trim().is_empty() {
                                                println!("[WEBTORRENT ERR] {}", line_res);
                                            }
                                        }
                                    });
                                }
                                if let Some(stdout) = child.stdout.take() {
                                    let reader = std::io::BufReader::new(stdout);
                                    let status_clone_2 = torrent_status_clone.clone();
                                    let ctx_clone_2 = ctx_clone.clone();
                                    let progress_dir = dest_dir.clone();
                                    thread::spawn(move || {
                                        for line_res in reader.lines().flatten() {
                                            if let Some((speed, downloaded, peers)) = parse_webtorrent_line(&line_res) {
                                                write_torrent_progress(&progress_dir, &downloaded);
                                                let mut status = status_clone_2.lock().unwrap();
                                                status.speed = speed;
                                                status.downloaded = downloaded;
                                                status.peers = peers;
                                                ctx_clone_2.request_repaint();
                                            } else if !line_res.trim().is_empty() {
                                                println!("[WEBTORRENT] {}", line_res);
                                            }
                                        }
                                        println!("[WEBTORRENT] stdout closed");
                                        let mut status = status_clone_2.lock().unwrap();
                                        status.active = false;
                                        ctx_clone_2.request_repaint();
                                    });
                                }
                                children.push(child);
                            } else {
                                let mut status = torrent_status_clone.lock().unwrap();
                                status.active = false;
                            }
                        });
                        self.new_movie_url.clear();
                    } else {
                        self.status_message = "Error: Torrent URL cannot be empty!".to_string();
                    }
                }

                if ui.button("🔄 Refresh Cache").clicked() {
                    self.refresh();
                    self.status_message = "Cache status refreshed".to_string();
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Status:").weak());
                ui.label(egui::RichText::new(&self.status_message).italics().color(egui::Color32::from_rgb(100, 200, 255)));
            });
            ui.add_space(8.0);
        });

        // Left Sidebar: Movie Selection List
        egui::SidePanel::left("left_panel")
            .resizable(true)
            .default_width(260.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.heading("📂 Cache Libraries");
                ui.add_space(8.0);

                egui::ScrollArea::vertical().show(ui, |ui| {
                    if self.movies.is_empty() {
                        ui.label(
                            egui::RichText::new(
                                "No cached torrents found.\nPaste a link below to start!",
                            )
                            .weak()
                            .italics(),
                        );
                    } else {
                        for (idx, movie) in self.movies.iter().enumerate() {
                            let title = match &movie.metadata {
                                Some(meta) => meta.title.clone(),
                                None => movie.dir_name.clone(),
                            };

                            let is_selected = self.selected_idx == Some(idx);
                            let item_text = format!(
                                "🧲 {}\n[Torrent Cache - {}]",
                                title,
                                format_size(movie.total_size_bytes)
                            );

                            if ui.selectable_label(is_selected, item_text).clicked() {
                                self.selected_idx = Some(idx);
                                self.status_message = format!("Selected: {}", title);
                            }
                            ui.add_space(4.0);
                        }
                    }
                });
            });

        // Central Panel
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(8.0);
            if let Some(idx) = self.selected_idx {
                if idx < self.movies.len() {
                    let movie = &self.movies[idx];
                    let (title, url) = match &movie.metadata {
                        Some(meta) => (meta.title.clone(), meta.url.clone()),
                        None => (movie.dir_name.clone(), "Unknown source URL".to_string()),
                    };
                    let dir_name = movie.dir_name.clone();
                    let total_size_bytes = movie.total_size_bytes;

                    self.central_panel_rendering(ui, ctx, title, url, dir_name, total_size_bytes);
                }
            } else {
                ui.vertical_centered(|ui| {
                    ui.add_space(100.0);
                    ui.label(
                        egui::RichText::new("No Video Cache Selected")
                            .font(egui::FontId::proportional(18.0))
                            .strong(),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new("Enter a Torrent link or Magnet URI below to start.")
                            .weak(),
                    );
                });
            }
        });
    }
}

// Extension trait helper for app rendering to avoid borrow checkers issues
trait PanelRenderHelper {
    fn central_panel_rendering(
        &self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        title: String,
        url: String,
        dir_name: String,
        total_size_bytes: u64,
    );
}

impl PanelRenderHelper for AppState {
    fn central_panel_rendering(
        &self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        title: String,
        url: String,
        dir_name: String,
        total_size_bytes: u64,
    ) {
        ui.heading(
            egui::RichText::new(format!("🧲 {}", title))
                .font(egui::FontId::proportional(20.0))
                .strong(),
        );
        ui.add_space(8.0);

        ui.group(|ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Original URL/Magnet:").strong());
                    ui.label(&url);
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Direct Cache Directory:").strong());
                    ui.label(format!("./stream_cache/{}", dir_name));
                });
            });
        });
        ui.add_space(12.0);

        let cache_dir_path = get_cache_dir().join(&dir_name);
        let local_media = find_media_file(&cache_dir_path);

        ui.group(|ui| {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("Torrent Streaming Engine:").strong());
                ui.add_space(4.0);

                let status = self.torrent_status.lock().unwrap();
                if status.active {
                    ui.horizontal(|ui| {
                        ui.label("Status:");
                        ui.label(
                            egui::RichText::new("Buffering & Downloading")
                                .color(egui::Color32::from_rgb(100, 200, 255))
                                .strong(),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("Download Speed:");
                        ui.label(
                            egui::RichText::new(&status.speed)
                                .color(egui::Color32::from_rgb(0, 220, 100))
                                .strong(),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("Downloaded Size:");
                        ui.label(&status.downloaded);
                    });
                    ui.horizontal(|ui| {
                        ui.label("Connected Peers:");
                        ui.label(&status.peers);
                    });
                } else {
                    ui.horizontal(|ui| {
                        ui.label("Status:");
                        ui.label(egui::RichText::new("Idle").weak());
                    });
                }
            });
        });
        ui.add_space(12.0);

        ui.group(|ui| {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("Torrent Storage Info:").strong());
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Pre-allocated Disk Space:").strong());
                    ui.label(format_size(total_size_bytes));
                });
                if let Some(ref path) = local_media {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Largest Media File:").strong());
                        ui.label(path.file_name().unwrap_or_default().to_string_lossy());
                    });
                }
            });
        });
        ui.add_space(16.0);

        // Action Buttons
        ui.horizontal(|ui| {
            let play_btn = ui.add_sized(
                [160.0, 40.0],
                egui::Button::new(
                    egui::RichText::new("🧲 Start/Resume Stream")
                        .font(egui::FontId::proportional(14.0))
                        .strong(),
                )
                .fill(egui::Color32::from_rgb(0, 160, 80)),
            );
            if play_btn.clicked() {
                let url_to_play = url.clone();
                let dir_to_play = dir_name.clone();
                let children_clone = self.spawned_children.clone();
                let torrent_status_clone = self.torrent_status.clone();
                let ctx_clone = ctx.clone();
                thread::spawn(move || {
                    let mut children = children_clone.lock().unwrap();
                    for child in children.iter_mut() {
                        let _ = child.kill();
                    }
                    children.clear();

                    // Free standard ports
                    let _ = Command::new("pkill")
                        .args(&["-9", "-i", "-f", "webtorrent"])
                        .status();

                    let dest = format!("./stream_cache/{}", dir_to_play);
                    let dest_dir = PathBuf::from(&dest);
                    let local_torrent_path =
                        format!("./stream_cache/{}/movie.torrent", dir_to_play);

                    let mut torrent_source = url_to_play.clone();

                    // Create cache folder first to ensure curl can save the file
                    let _ = fs::create_dir_all(&dest_dir);

                    if std::path::Path::new(&local_torrent_path).exists() {
                        torrent_source = local_torrent_path;
                    } else if let Some(h) = get_infohash(&url_to_play) {
                        let torrent_url = format!("https://itorrents.net/torrent/{}.torrent", h);
                        let download_res = Command::new("curl")
                            .args(&["-s", "-L", "-o", &local_torrent_path, &torrent_url])
                            .status();
                        if download_res.is_ok()
                            && std::path::Path::new(&local_torrent_path).exists()
                        {
                            torrent_source = local_torrent_path;
                        }
                    }

                    // Initialize active status
                    {
                        let mut status = torrent_status_clone.lock().unwrap();
                        status.active = true;
                        status.speed = "Connecting...".to_string();
                        status.downloaded = "0 MB".to_string();
                        status.peers = "0/0".to_string();
                    }
                    ctx_clone.request_repaint();

                    write_torrent_progress(&dest_dir, "0 B");
                    spawn_gap_aware_progress_scanner(
                        dest_dir.clone(),
                        torrent_status_clone.clone(),
                    );

                    let mut cmd = Command::new("npx");
                    cmd.args(&[
                        "-y",
                        "webtorrent-cli",
                        "download",
                        &torrent_source,
                        "--mpv",
                        "--out",
                        &dest,
                    ]);
                    if let Some(player_args) = webtorrent_mpv_player_args(&dest_dir) {
                        cmd.arg(format!("--player-args={player_args}"));
                    }
                    cmd.stdout(std::process::Stdio::piped());
                    cmd.stderr(std::process::Stdio::piped());

                    let spawn_res = cmd.spawn();
                    if let Ok(mut child) = spawn_res {
                        if let Some(stderr) = child.stderr.take() {
                            let err_reader = std::io::BufReader::new(stderr);
                            thread::spawn(move || {
                                for line_res in err_reader.lines().flatten() {
                                    if !line_res.trim().is_empty() {
                                        println!("[WEBTORRENT ERR] {}", line_res);
                                    }
                                }
                            });
                        }
                        if let Some(stdout) = child.stdout.take() {
                            let reader = std::io::BufReader::new(stdout);
                            let status_clone_2 = torrent_status_clone.clone();
                            let ctx_clone_2 = ctx_clone.clone();
                            let progress_dir = dest_dir.clone();
                            thread::spawn(move || {
                                let mut buffer = String::new();
                                let mut br = std::io::BufReader::new(reader);
                                while let Ok(bytes) = br.read_line(&mut buffer) {
                                    if bytes == 0 {
                                        break;
                                    }
                                    if let Some((speed, downloaded, peers)) =
                                        parse_webtorrent_line(&buffer)
                                    {
                                        write_torrent_progress(&progress_dir, &downloaded);
                                        let mut status = status_clone_2.lock().unwrap();
                                        status.speed = speed;
                                        status.downloaded = downloaded;
                                        status.peers = peers;
                                        ctx_clone_2.request_repaint();
                                    } else if !buffer.trim().is_empty() {
                                        println!("[WEBTORRENT] {}", buffer.trim_end());
                                    }
                                    buffer.clear();
                                }
                                println!("[WEBTORRENT] stdout closed");
                                let mut status = status_clone_2.lock().unwrap();
                                status.active = false;
                                ctx_clone_2.request_repaint();
                            });
                        }
                        children.push(child);
                    } else {
                        let mut status = torrent_status_clone.lock().unwrap();
                        status.active = false;
                    }
                });
            }

            if let Some(media_path) = local_media {
                let play_local_btn = ui.add_sized(
                    [160.0, 40.0],
                    egui::Button::new(
                        egui::RichText::new("▶ Play Local File")
                            .font(egui::FontId::proportional(14.0))
                            .strong(),
                    )
                    .fill(egui::Color32::from_rgb(0, 120, 200)),
                );
                if play_local_btn.clicked() {
                    let path_str = media_path.to_str().unwrap_or_default().to_string();
                    let media_for_verify = media_path.clone();
                    let cache_for_verify = cache_dir_path.clone();
                    let progress_file = progress_file_path(&cache_dir_path);
                    let children_clone = self.spawned_children.clone();
                    thread::spawn(move || {
                        let mut cmd = Command::new("mpv");
                        cmd.args(direct_mpv_args(&progress_file));
                        cmd.arg(&path_str);
                        if let Ok(child) = cmd.spawn() {
                            children_clone.lock().unwrap().push(child);
                        }
                        refresh_verified_torrent_progress_if_needed(
                            &cache_for_verify,
                            &media_for_verify,
                        );
                    });
                }
            }

            let delete_btn = ui.add_sized(
                [120.0, 40.0],
                egui::Button::new("🗑 Delete Cache").fill(egui::Color32::from_rgb(180, 40, 40)),
            );
            if delete_btn.clicked() {
                let path = get_cache_dir().join(&dir_name);
                if path.exists() {
                    let _ = fs::remove_dir_all(&path);
                }
            }
        });
    }
}

fn main() -> eframe::Result<()> {
    // Clean up any stale background instances at startup
    let _ = Command::new("pkill")
        .args(&["-9", "-i", "-f", "webtorrent"])
        .status();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Film Torrent Streaming Manager")
            .with_inner_size([800.0, 500.0]),
        ..Default::default()
    };

    let spawned_children = Arc::new(Mutex::new(Vec::<std::process::Child>::new()));

    // Register Ctrl+C terminal signal handler to kill spawned proxies
    let children_ctrlc = spawned_children.clone();
    let _ = ctrlc::set_handler(move || {
        println!("\n[SIGINT] Intercepted Ctrl+C. Cleaning up child processes...");
        if let Ok(mut children) = children_ctrlc.lock() {
            println!("Terminating {} spawned child processes...", children.len());
            for child in children.iter_mut() {
                let _ = child.kill();
            }
        }
        let _ = Command::new("pkill")
            .args(&["-9", "-i", "-f", "webtorrent"])
            .status();
        std::process::exit(0);
    });

    eframe::run_native(
        "film_cache_manager",
        options,
        Box::new(move |cc| {
            let mut app = AppState::new(cc.egui_ctx.clone());
            app.spawned_children = spawned_children;
            Ok(Box::new(app))
        }),
    )
}
