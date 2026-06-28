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
    film_title: Option<String>,
    year: Option<u16>,
    source_label: Option<String>,
    duration: Option<String>,
}

#[derive(Clone)]
struct MovieCacheInfo {
    dir_name: String,
    metadata: Option<MovieMetadata>,
    total_size_bytes: u64,
    logical_size_bytes: u64,
    film_key: String,
    film_title: String,
    year: Option<u16>,
    source_label: String,
}

#[derive(Clone)]
struct MovieGroup {
    key: String,
    title: String,
    year: Option<u16>,
    torrents: Vec<MovieCacheInfo>,
    total_size_bytes: u64,
    total_logical_size_bytes: u64,
}

struct TorrentStatus {
    speed: String,
    downloaded: String,
    peers: String,
    active: bool,
    active_dir: Option<String>,
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

fn remove_empty_dirs_up_to(mut dir: PathBuf, stop_at: &std::path::Path) {
    let stop_at = stop_at
        .canonicalize()
        .unwrap_or_else(|_| stop_at.to_path_buf());

    loop {
        let current = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if current == stop_at {
            break;
        }
        if fs::remove_dir(&dir).is_err() {
            break;
        }
        if !dir.pop() {
            break;
        }
    }
}

fn delete_local_media_file(cache_dir: &std::path::Path, media_path: &std::path::Path) {
    let _ = fs::remove_file(media_path);
    let _ = fs::remove_file(progress_file_path(cache_dir));
    if let Some(parent) = media_path.parent() {
        remove_empty_dirs_up_to(parent.to_path_buf(), cache_dir);
    }
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

fn is_webtorrent_seeding_line(line: &str) -> bool {
    line.trim_start().starts_with("Seeding:")
}

fn torrent_info_name(cache_dir: &std::path::Path) -> Option<String> {
    let torrent_bytes = fs::read(cache_dir.join("movie.torrent")).ok()?;
    let root = parse_bencode(&torrent_bytes).and_then(|value| match value {
        BValue::Dict(dict) => Some(dict),
        _ => None,
    })?;
    let info = bdict_get(&root, b"info").and_then(bvalue_dict)?;
    bdict_get(info, b"name")
        .and_then(bvalue_bytes)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .map(ToOwned::to_owned)
}

fn clean_release_token(token: &str) -> String {
    token
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '\'' {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
}

fn title_case_words(text: &str) -> String {
    text.split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    let mut out = String::new();
                    out.extend(first.to_uppercase());
                    out.push_str(&chars.as_str().to_lowercase());
                    out
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_film_key(title: &str, year: Option<u16>) -> String {
    let title_key = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    match year {
        Some(year) => format!("{title_key} {year}"),
        None => title_key,
    }
}

fn parse_film_identity(raw_name: &str) -> (String, Option<u16>, String) {
    let cleaned = clean_release_token(raw_name);
    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    let year_idx = tokens.iter().position(|token| {
        token.len() == 4
            && token
                .parse::<u16>()
                .is_ok_and(|year| (1900..=2099).contains(&year))
    });

    let year = year_idx.and_then(|idx| tokens[idx].parse::<u16>().ok());
    let title_tokens = match year_idx {
        Some(idx) if idx > 0 => &tokens[..idx],
        _ => tokens.as_slice(),
    };
    let title = title_case_words(&title_tokens.join(" "));
    let title = if title.is_empty() {
        raw_name.trim().to_string()
    } else {
        title
    };
    let source_label = raw_name.trim().to_string();

    (title, year, source_label)
}

fn cache_movie_identity(
    cache_dir: &std::path::Path,
    metadata: Option<&MovieMetadata>,
) -> (String, Option<u16>, String) {
    if let Some(meta) = metadata {
        if let Some(film_title) = meta
            .film_title
            .as_ref()
            .filter(|title| !title.trim().is_empty())
        {
            return (
                film_title.clone(),
                meta.year,
                meta.source_label
                    .clone()
                    .unwrap_or_else(|| meta.title.clone()),
            );
        }
    }

    let raw_name = torrent_info_name(cache_dir)
        .or_else(|| {
            find_media_file(cache_dir).and_then(|path| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(ToOwned::to_owned)
            })
        })
        .or_else(|| metadata.map(|meta| meta.title.clone()))
        .unwrap_or_else(|| {
            cache_dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });

    parse_film_identity(&raw_name)
}

fn scan_caches() -> Vec<MovieGroup> {
    let cache_dir = get_cache_dir();
    let mut movies: Vec<MovieCacheInfo> = Vec::new();

    if !cache_dir.exists() {
        return Vec::new();
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
                let mut metadata: Option<MovieMetadata> = if metadata_path.exists() {
                    fs::read_to_string(&metadata_path)
                        .ok()
                        .and_then(|content| serde_json::from_str(&content).ok())
                } else {
                    None
                };

                // Lazy-load video duration and cache it in metadata.json
                let local_media = find_media_file(&path);
                if let Some(ref media_path) = local_media {
                    let mut needs_save = false;
                    let duration = metadata.as_ref().and_then(|m| m.duration.clone());
                    if duration.is_none() {
                        if let Some(dur_str) = get_video_duration(media_path) {
                            if let Some(ref mut meta) = metadata {
                                meta.duration = Some(dur_str);
                                needs_save = true;
                            } else {
                                let (film_title, year, source_label) = cache_movie_identity(&path, None);
                                let display_title = match year {
                                    Some(y) => format!("{} ({})", film_title, y),
                                    None => film_title.clone(),
                                };
                                metadata = Some(MovieMetadata {
                                    title: display_title,
                                    url: "".to_string(),
                                    film_title: Some(film_title),
                                    year,
                                    source_label: Some(source_label),
                                    duration: Some(dur_str),
                                });
                                needs_save = true;
                            }
                        }
                    }
                    if needs_save {
                        if let Some(ref meta) = metadata {
                            if let Ok(content) = serde_json::to_string_pretty(meta) {
                                let _ = fs::write(&metadata_path, content);
                            }
                        }
                    }
                }

                let mut total_size_bytes = 0;
                let mut logical_size_bytes = 0;

                // Recursive helper to scan directory size (handles Torrent nested files)
                fn scan_dir_sizes(dir_path: &std::path::Path, size_bytes: &mut u64, logical_bytes: &mut u64) {
                    #[cfg(unix)]
                    use std::os::unix::fs::MetadataExt;

                    if let Ok(files) = fs::read_dir(dir_path) {
                        for file in files.flatten() {
                            let file_path = file.path();
                            if file_path.is_file() {
                                if let Ok(meta) = file.metadata() {
                                    *logical_bytes += meta.len();
                                    #[cfg(unix)]
                                    {
                                        *size_bytes += meta.blocks() * 512;
                                    }
                                    #[cfg(not(unix))]
                                    {
                                        *size_bytes += meta.len();
                                    }
                                }
                            } else if file_path.is_dir() {
                                scan_dir_sizes(&file_path, size_bytes, logical_bytes);
                            }
                        }
                    }
                }

                scan_dir_sizes(&path, &mut total_size_bytes, &mut logical_size_bytes);
                let (film_title, year, source_label) =
                    cache_movie_identity(&path, metadata.as_ref());
                let film_key = normalize_film_key(&film_title, year);

                movies.push(MovieCacheInfo {
                    dir_name,
                    metadata,
                    total_size_bytes,
                    logical_size_bytes,
                    film_key,
                    film_title,
                    year,
                    source_label,
                });
            }
        }
    }

    let mut groups: Vec<MovieGroup> = Vec::new();
    for movie in movies {
        if let Some(group) = groups.iter_mut().find(|group| group.key == movie.film_key) {
            group.total_size_bytes += movie.total_size_bytes;
            group.total_logical_size_bytes += movie.logical_size_bytes;
            group.torrents.push(movie);
        } else {
            groups.push(MovieGroup {
                key: movie.film_key.clone(),
                title: movie.film_title.clone(),
                year: movie.year,
                total_size_bytes: movie.total_size_bytes,
                total_logical_size_bytes: movie.logical_size_bytes,
                torrents: vec![movie],
            });
        }
    }

    for group in &mut groups {
        group
            .torrents
            .sort_by(|a, b| a.source_label.cmp(&b.source_label));
    }
    groups.sort_by(|a, b| {
        a.title
            .cmp(&b.title)
            .then_with(|| a.year.cmp(&b.year))
            .then_with(|| a.key.cmp(&b.key))
    });

    groups
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
        .join("osc.lua");
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



fn direct_mpv_args(progress_file: &std::path::Path) -> Vec<String> {
    let mut args = vec![
        "--load-scripts=no".to_string(),
        "--osc=no".to_string(),
        "--no-resume-playback".to_string(),
        format!(
            "--script-opts=osc-layout=bottombar,osc-seekbarstyle=bar,osc-torrent_progress_file={}",
            progress_file.to_string_lossy()
        ),
    ];

    if let Some(script) = mpv_script_path() {
        args.push(format!("--script={}", script));
    }

    args
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct YtsTorrent {
    url: String,
    hash: String,
    quality: String,
    size: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct YtsMovie {
    title_long: Option<String>,
    title: String,
    year: Option<u16>,
    rating: Option<f32>,
    genres: Option<Vec<String>>,
    runtime: Option<u32>,
    torrents: Vec<YtsTorrent>,
}

fn load_mirrors() -> Vec<String> {
    let mut mirrors = Vec::new();
    // Always prepend the official new API endpoint first
    mirrors.push("https://movies-api.accel.li".to_string());

    if let Ok(content) = fs::read_to_string("yify_mirrors.txt") {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.contains("yts") || line.contains("yify") {
                let domain = line.to_string();
                if !mirrors.contains(&domain) {
                    mirrors.push(domain);
                }
            }
        }
    }
    // Prioritize HTTPS (excluding our hardcoded official API which is already first)
    let mut other_mirrors = mirrors.split_off(1);
    other_mirrors.sort_by_key(|m| !m.starts_with("https://"));
    mirrors.extend(other_mirrors);
    mirrors
}

fn percent_encode(text: &str) -> String {
    let mut encoded = String::new();
    for b in text.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*b as char);
            }
            b' ' => {
                encoded.push('+');
            }
            _ => {
                encoded.push_str(&format!("%{:02X}", b));
            }
        }
    }
    encoded
}

fn make_magnet_link(info_hash: &str, title: &str) -> String {
    let trackers = [
        "udp://open.demonii.com:1337/announce",
        "udp://tracker.openbittorrent.com:80",
        "udp://tracker.coppersurfer.tk:6969",
        "udp://glotorrents.pw:6969/announce",
        "udp://tracker.opentrackr.org:1337/announce",
        "udp://p4p.arenabg.com:1337",
        "udp://tracker.leechers-paradise.org:6969"
    ];
    let mut tracker_args = String::new();
    for t in trackers {
        tracker_args.push_str(&format!("&tr={}", percent_encode(t)));
    }
    format!(
        "magnet:?xt=urn:btih:{}&dn={}{}",
        info_hash,
        percent_encode(title),
        tracker_args
    )
}

fn get_free_port() -> Option<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|listener| listener.local_addr().ok())
        .map(|addr| addr.port())
}

fn get_video_duration(media_path: &std::path::Path) -> Option<String> {
    let output = Command::new("ffprobe")
        .args(&[
            "-v", "error",
            "-show_entries", "format=duration",
            "-of", "default=noprint_wrappers=1:nokey=1",
            media_path.to_str().unwrap_or_default()
        ])
        .output()
        .ok()?;
    
    if output.status.success() {
        let dur_str = std::str::from_utf8(&output.stdout).ok()?.trim();
        if let Ok(duration_secs) = dur_str.parse::<f64>() {
            let hours = (duration_secs / 3600.0) as u32;
            let minutes = ((duration_secs % 3600.0) / 60.0) as u32;
            if hours > 0 {
                return Some(format!("{}h {}m", hours, minutes));
            } else {
                return Some(format!("{}m", minutes));
            }
        }
    }
    None
}

fn get_torrent_downloaded_and_total(cache_dir: &std::path::Path) -> Option<(u64, u64)> {
    let progress_file = progress_file_path(cache_dir);
    if progress_file.exists() {
        if let Ok(content) = fs::read_to_string(&progress_file) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                let dl = val.get("downloaded_bytes").and_then(|v| v.as_u64());
                let tot = val.get("total_bytes").and_then(|v| v.as_u64());
                if let (Some(dl), Some(tot)) = (dl, tot) {
                    return Some((dl, tot));
                }
            }
        }
    }
    None
}

fn has_subtitles(cache_dir: &std::path::Path) -> (bool, bool) {
    let mut has_subs = false;
    let mut has_english = false;

    fn check_dir(dir: &std::path::Path, has_subs: &mut bool, has_english: &mut bool) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                        let ext_lower = ext.to_lowercase();
                        if ext_lower == "srt" || ext_lower == "vtt" {
                            *has_subs = true;
                            let name_lower = path.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
                            if name_lower.contains("english") || name_lower.contains("eng") || name_lower.contains("en") {
                                *has_english = true;
                            }
                        }
                    }
                } else if path.is_dir() {
                    check_dir(&path, has_subs, has_english);
                }
            }
        }
    }
    check_dir(cache_dir, &mut has_subs, &mut has_english);
    (has_subs, has_english)
}

fn torrent_contains_subtitles(cache_dir: &std::path::Path) -> Option<(bool, bool)> {
    let torrent_path = cache_dir.join("movie.torrent");
    if !torrent_path.exists() {
        return None;
    }
    let torrent_bytes = fs::read(torrent_path).ok()?;
    let root = parse_bencode(&torrent_bytes).and_then(|value| match value {
        BValue::Dict(dict) => Some(dict),
        _ => None,
    })?;
    let info = bdict_get(&root, b"info").and_then(bvalue_dict)?;

    let mut has_subs = false;
    let mut has_english = false;

    if let Some(files) = bdict_get(info, b"files").and_then(bvalue_list) {
        for f in files {
            if let BValue::Dict(f_dict) = f {
                if let Some(path_val) = bdict_get(&f_dict, b"path") {
                    match path_val {
                        BValue::List(path_list) => {
                            for p in path_list {
                                if let BValue::Bytes(p_bytes) = p {
                                    if let Ok(p_str) = std::str::from_utf8(p_bytes) {
                                        let p_lower = p_str.to_lowercase();
                                        if p_lower.ends_with(".srt") || p_lower.ends_with(".vtt") {
                                            has_subs = true;
                                            if p_lower.contains("english") || p_lower.contains("eng") || p_lower.contains("en") {
                                                has_english = true;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Some((has_subs, has_english))
}

struct AppState {
    movies: Vec<MovieGroup>,
    selected_idx: Option<usize>,
    new_movie_url: String,
    status_message: String,

    torrent_status: Arc<Mutex<TorrentStatus>>,
    spawned_children: Arc<Mutex<Vec<std::process::Child>>>,

    search_query: String,
    search_results: Arc<Mutex<Vec<YtsMovie>>>,
    is_searching: Arc<Mutex<bool>>,
    search_status: Arc<Mutex<String>>,
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
            active_dir: None,
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
            search_query: String::new(),
            search_results: Arc::new(Mutex::new(Vec::new())),
            is_searching: Arc::new(Mutex::new(false)),
            search_status: Arc::new(Mutex::new(String::new())),
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

                if ui.button("➕ Add Torrent").clicked() {
                    let url = self.new_movie_url.trim().to_string();
                    if !url.is_empty() {
                        let hash = get_infohash(&url).unwrap_or_else(|| {
                            let cleaned: String = url.chars()
                                .map(|c| if c.is_alphanumeric() { c } else { '_' })
                                .collect();
                            if cleaned.len() > 30 { cleaned[cleaned.len()-30..].to_string() } else { cleaned }
                        });
                        let dir_name = format!("torrent_{}", hash);
                        let dest_dir = get_cache_dir().join(&dir_name);
                        let _ = fs::create_dir_all(&dest_dir);

                        let magnet_uri = get_magnet_uri(&url);

                        let meta_path = dest_dir.join("metadata.json");
                        let (film_title, year, source_label) =
                            cache_movie_identity(&dest_dir, None);
                        let display_title = match year {
                            Some(year) => format!("{film_title} ({year})"),
                            None => film_title.clone(),
                        };
                        let meta_json = serde_json::json!({
                            "title": display_title,
                            "url": magnet_uri,
                            "film_title": film_title,
                            "year": year,
                            "source_label": source_label,
                        });
                        if let Ok(content) = serde_json::to_string_pretty(&meta_json) {
                            let _ = fs::write(&meta_path, content);
                        }

                        self.refresh();

                        if let Some(pos) = self.movies.iter().position(|m| m.key == dir_name) {
                            self.selected_idx = Some(pos);
                        }

                        self.new_movie_url.clear();
                        self.status_message = format!("Added torrent to Cache Library. Click 'Start Torrent' to download!");
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

                if ui.button("🔍 Search YIFY Database").clicked() {
                    self.selected_idx = None;
                    self.status_message = "Opened movie search dashboard".to_string();
                }
                ui.add_space(8.0);
                ui.separator();
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
                        for (idx, group) in self.movies.iter().enumerate() {
                            let is_selected = self.selected_idx == Some(idx);
                            let title = match group.year {
                                Some(year) => format!("{} ({year})", group.title),
                                None => group.title.clone(),
                            };
                            let item_text = format!(
                                "🎞 {}\n{} torrent{} - {}",
                                title,
                                group.torrents.len(),
                                if group.torrents.len() == 1 { "" } else { "s" },
                                format_size(group.total_size_bytes)
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
                    let group = self.movies[idx].clone();
                    self.central_panel_rendering(ui, ctx, group);
                }
            } else {
                ui.vertical(|ui| {
                    ui.heading(egui::RichText::new("🔍 Search YIFY / YTS Database").font(egui::FontId::proportional(20.0)).strong());
                    ui.add_space(8.0);
                    ui.label("Search online for torrents across verified mirrors, or select a cached library on the left.");
                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        let search_input = ui.add(
                            egui::TextEdit::singleline(&mut self.search_query)
                                .hint_text("Type movie title (e.g. Inception)...")
                                .desired_width(320.0)
                        );

                        let enter_pressed = search_input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                        let searching = *self.is_searching.lock().unwrap();
                        if searching {
                            ui.add(egui::widgets::Spinner::new());
                            let status = self.search_status.lock().unwrap().clone();
                            ui.label(status);
                        } else {
                            if ui.button("🔍 Search Database").clicked() || enter_pressed {
                                let query = self.search_query.trim().to_string();
                                if !query.is_empty() {
                                    let results_clone = self.search_results.clone();
                                    let is_searching_clone = self.is_searching.clone();
                                    let status_clone = self.search_status.clone();
                                    let ctx_clone = ctx.clone();

                                    *self.is_searching.lock().unwrap() = true;
                                    *self.search_status.lock().unwrap() = "Searching mirrors in parallel...".to_string();
                                    results_clone.lock().unwrap().clear();
                                    thread::spawn(move || {
                                        let mirrors = load_mirrors();
                                        if mirrors.is_empty() {
                                            *status_clone.lock().unwrap() = "Error: yify_mirrors.txt not found!".to_string();
                                            *is_searching_clone.lock().unwrap() = false;
                                            ctx_clone.request_repaint();
                                            return;
                                        }

                                        let total_mirrors = mirrors.len();
                                        let active_threads = Arc::new(Mutex::new(total_mirrors));

                                        for mirror in mirrors {
                                            let query = query.clone();
                                            let results_clone = results_clone.clone();
                                            let is_searching_clone = is_searching_clone.clone();
                                            let status_clone = status_clone.clone();
                                            let ctx_clone = ctx_clone.clone();
                                            let active_threads = active_threads.clone();

                                            thread::spawn(move || {
                                                let config = ureq::Agent::config_builder()
                                                    .timeout_global(Some(std::time::Duration::from_secs(6)))
                                                    .build();
                                                let agent: ureq::Agent = config.into();

                                                let encoded = percent_encode(&query);
                                                let url = format!("{}/api/v2/list_movies.json?query_term={}&limit=10", mirror, encoded);

                                                let res = agent.get(&url).call();

                                                if let Ok(mut resp) = res {
                                                    if let Ok(body) = resp.body_mut().read_to_string() {
                                                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) {
                                                            if val["status"] == "ok" {
                                                                if let Some(movies_arr) = val["data"]["movies"].as_array() {
                                                                    let mut parsed_movies = Vec::new();
                                                                    for m_val in movies_arr {
                                                                        if let Ok(movie) = serde_json::from_value::<YtsMovie>(m_val.clone()) {
                                                                            parsed_movies.push(movie);
                                                                        }
                                                                    }
                                                                    if !parsed_movies.is_empty() {
                                                                        let mut results = results_clone.lock().unwrap();
                                                                        let old_count = results.len();
                                                                        for movie in parsed_movies {
                                                                            if !results.iter().any(|m| m.title == movie.title && m.year == movie.year) {
                                                                                results.push(movie);
                                                                            }
                                                                        }
                                                                        if results.len() > old_count {
                                                                            let mut status = status_clone.lock().unwrap();
                                                                            *status = format!("Updating results (found {} matches)...", results.len());
                                                                            ctx_clone.request_repaint();
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }

                                                let mut active = active_threads.lock().unwrap();
                                                *active -= 1;
                                                if *active == 0 {
                                                    *is_searching_clone.lock().unwrap() = false;
                                                    let mut status = status_clone.lock().unwrap();
                                                    let final_results = results_clone.lock().unwrap().len();
                                                    if final_results == 0 {
                                                        *status = "No results found across any mirrors.".to_string();
                                                    } else {
                                                        *status = format!("Search completed. Found {} matches.", final_results);
                                                    }
                                                    ctx_clone.request_repaint();
                                                }
                                            });
                                        }
                                    });
                                }
                            }
                        }
                    });

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);

                    // Show status message if search is not active
                    let is_searching = *self.is_searching.lock().unwrap();
                    let status_msg = self.search_status.lock().unwrap().clone();
                    if !status_msg.is_empty() && !is_searching {
                        ui.label(egui::RichText::new(&status_msg).italics().color(egui::Color32::from_rgb(180, 180, 200)));
                        ui.add_space(6.0);
                    }

                    // Render search results scroll area
                    let results = self.search_results.lock().unwrap().clone();
                    if !results.is_empty() {
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            for movie in results {
                                let title = match movie.year {
                                    Some(y) => format!("{} ({})", movie.title, y),
                                    None => movie.title.clone(),
                                };

                                ui.group(|ui| {
                                    ui.vertical(|ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(egui::RichText::new(title).strong().font(egui::FontId::proportional(16.0)));
                                            if let Some(r) = movie.rating {
                                                ui.label(egui::RichText::new(format!("★ {}/10", r)).color(egui::Color32::from_rgb(255, 200, 0)));
                                            }
                                            if let Some(rt) = movie.runtime {
                                                ui.label(format!("⏱ {} min", rt));
                                            }
                                        });

                                        if let Some(genres) = movie.genres {
                                            ui.label(egui::RichText::new(genres.join(", ")).weak());
                                        }

                                        ui.add_space(4.0);
                                        ui.horizontal(|ui| {
                                            ui.label("Stream Option:");
                                            for torrent in movie.torrents {
                                                let btn_text = format!("📥 {} ({})", torrent.quality, torrent.size);
                                                if ui.button(btn_text).clicked() {
                                                    let magnet = make_magnet_link(&torrent.hash, &movie.title);
                                                    let hash = torrent.hash.to_uppercase();
                                                    let dir_name = format!("torrent_{}", hash);
                                                    let dest_dir = get_cache_dir().join(&dir_name);
                                                    let _ = fs::create_dir_all(&dest_dir);

                                                    let display_title = match movie.year {
                                                        Some(y) => format!("{} ({})", movie.title, y),
                                                        None => movie.title.clone(),
                                                    };
                                                    let meta_json = serde_json::json!({
                                                        "title": display_title,
                                                        "url": magnet,
                                                        "film_title": movie.title,
                                                        "year": movie.year,
                                                        "source_label": "YIFY",
                                                    });
                                                    if let Ok(content) = serde_json::to_string_pretty(&meta_json) {
                                                        let _ = fs::write(dest_dir.join("metadata.json"), content);
                                                    }

                                                    self.refresh();

                                                    if let Some(pos) = self.movies.iter().position(|m| m.key == dir_name) {
                                                        self.selected_idx = Some(pos);
                                                    }

                                                    self.status_message = format!("Added {} [{}] to Cache Library. Click 'Start Torrent' to download!", movie.title, torrent.quality);
                                                }
                                            }
                                        });
                                    });
                                });
                                ui.add_space(6.0);
                            }
                        });
                    }
                });
            }
        });
    }
}

// Extension trait helper for app rendering to avoid borrow checkers issues
trait PanelRenderHelper {
    fn central_panel_rendering(&self, ui: &mut egui::Ui, ctx: &egui::Context, group: MovieGroup);
}

impl PanelRenderHelper for AppState {
    fn central_panel_rendering(&self, ui: &mut egui::Ui, ctx: &egui::Context, group: MovieGroup) {
        let title = match group.year {
            Some(year) => format!("{} ({year})", group.title),
            None => group.title.clone(),
        };
        ui.heading(
            egui::RichText::new(format!("🎞 {}", title))
                .font(egui::FontId::proportional(20.0))
                .strong(),
        );
        ui.add_space(8.0);


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

        ui.label(egui::RichText::new("Torrent Links").strong());
        ui.add_space(6.0);

        for torrent in group.torrents {
            let cache_dir_path = get_cache_dir().join(&torrent.dir_name);
            let local_media = find_media_file(&cache_dir_path);
            let url = torrent
                .metadata
                .as_ref()
                .map(|meta| meta.url.clone())
                .unwrap_or_else(|| "Unknown source URL".to_string());

            ui.group(|ui| {
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(&torrent.source_label).strong());
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new("Magnet Link:").strong());
                        ui.label(&url);
                    });

                    let hash_opt = if torrent.dir_name.starts_with("torrent_") {
                        Some(&torrent.dir_name[8..])
                    } else {
                        None
                    };
                    if let Some(hash) = hash_opt {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(egui::RichText::new("Direct Torrent Link:").strong());
                            ui.label(format!("https://yts.gg/torrent/download/{}", hash.to_uppercase()));
                        });
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new("Cache Directory:").strong());
                        ui.label(format!("./stream_cache/{}", torrent.dir_name));
                    });
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Disk Space Used:").strong());
                        ui.label(format_size(torrent.total_size_bytes));
                    });
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Total File Size:").strong());
                        ui.label(format_size(torrent.logical_size_bytes));
                    });

                    if let Some(ref meta) = torrent.metadata {
                        if let Some(ref dur) = meta.duration {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("Video Duration:").strong());
                                ui.label(dur);
                            });
                        }
                    }

                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Subtitles:").strong());
                        let (subs_cached, eng_cached) = has_subtitles(&cache_dir_path);
                        if subs_cached {
                            let label_text = if eng_cached {
                                "Available (cached: English)"
                            } else {
                                "Available (cached: Other)"
                            };
                            ui.label(
                                egui::RichText::new(label_text)
                                    .color(egui::Color32::from_rgb(0, 200, 100))
                                    .strong(),
                            );
                        } else {
                            match torrent_contains_subtitles(&cache_dir_path) {
                                Some((true, has_eng)) => {
                                    let label_text = if has_eng {
                                        "Included in torrent (English)"
                                    } else {
                                        "Included in torrent (Other)"
                                    };
                                    ui.label(
                                        egui::RichText::new(label_text)
                                            .color(egui::Color32::from_rgb(200, 160, 0))
                                            .strong(),
                                    );
                                }
                                Some((false, _)) => {
                                    ui.label(egui::RichText::new("None in torrent").weak());
                                }
                                None => {
                                    ui.label(egui::RichText::new("Unknown (start torrent to check)").weak());
                                }
                            }
                        }
                    });

                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Torrented Status:").strong());

                        let is_active = {
                            let status = self.torrent_status.lock().unwrap();
                            status.active && status.active_dir.as_ref() == Some(&torrent.dir_name)
                        };

                        if is_active {
                            let status = self.torrent_status.lock().unwrap();
                            ui.label(format!(
                                "{} / {} (Active - {})",
                                status.downloaded,
                                format_size(torrent.total_size_bytes),
                                status.speed
                            ));
                        } else if let Some((dl, total)) = get_torrent_downloaded_and_total(&cache_dir_path) {
                            let pct = if total > 0 { (dl as f64 / total as f64) * 100.0 } else { 0.0 };
                            ui.label(format!(
                                "{} / {} ({:.2}%)",
                                format_size(dl),
                                format_size(total),
                                pct
                            ));
                        } else {
                            ui.label(format!("0 B / {} (0.00%)", format_size(torrent.total_size_bytes)));
                        }
                    });

                    if let Some(ref path) = local_media {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(egui::RichText::new("Largest Media File:").strong());
                            ui.label(path.file_name().unwrap_or_default().to_string_lossy());
                        });
                    }
                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        let play_btn = ui.add_sized(
                            [160.0, 40.0],
                            egui::Button::new(
                                egui::RichText::new("🧲 Start Torrent")
                                    .font(egui::FontId::proportional(14.0))
                                    .strong(),
                            )
                            .fill(egui::Color32::from_rgb(0, 160, 80)),
                        );
                        if play_btn.clicked() {
                            let url_to_play = url.clone();
                            let dir_to_play = torrent.dir_name.clone();
                            let children_clone = self.spawned_children.clone();
                            let torrent_status_clone = self.torrent_status.clone();
                            let ctx_clone = ctx.clone();
                            thread::spawn(move || {
                                let mut children = children_clone.lock().unwrap();
                                for child in children.iter_mut() {
                                    let _ = child.kill();
                                }
                                children.clear();

                                let _ = Command::new("pkill")
                                    .args(&["-9", "-i", "-f", "webtorrent"])
                                    .status();

                                let dest = format!("./stream_cache/{}", dir_to_play);
                                let dest_dir = PathBuf::from(&dest);
                                let local_torrent_path =
                                    format!("./stream_cache/{}/movie.torrent", dir_to_play);
                                let mut torrent_source = url_to_play.clone();

                                let _ = fs::create_dir_all(&dest_dir);

                                if std::path::Path::new(&local_torrent_path).exists() {
                                    torrent_source = local_torrent_path;
                                } else if let Some(h) = get_infohash(&url_to_play) {
                                    let torrent_url =
                                        format!("https://itorrents.net/torrent/{}.torrent", h);
                                    let download_res = Command::new("curl")
                                        .args(&[
                                            "-s",
                                            "-L",
                                            "-o",
                                            &local_torrent_path,
                                            &torrent_url,
                                        ])
                                        .status();
                                    if download_res.is_ok()
                                        && std::path::Path::new(&local_torrent_path).exists()
                                    {
                                        torrent_source = local_torrent_path;
                                    }
                                }

                                {
                                    let mut status = torrent_status_clone.lock().unwrap();
                                    status.active = true;
                                    status.active_dir = Some(dir_to_play.clone());
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

                                let free_port = get_free_port().unwrap_or(8080);
                                let mut cmd = Command::new("npx");
                                cmd.args(&[
                                    "-y",
                                    "webtorrent-cli",
                                    "download",
                                    &torrent_source,
                                    "--port",
                                    &free_port.to_string(),
                                    "--out",
                                    &dest,
                                ]);
                                cmd.stdout(std::process::Stdio::piped());
                                cmd.stderr(std::process::Stdio::piped());

                                let spawn_res = cmd.spawn();
                                if let Ok(mut child) = spawn_res {
                                    let child_pid = child.id();
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
                                                if is_webtorrent_seeding_line(&buffer) {
                                                    println!(
                                                        "[WEBTORRENT] Download complete, stopping before seeding"
                                                    );
                                                    let _ = Command::new("kill")
                                                        .args(["-TERM", &child_pid.to_string()])
                                                        .status();
                                                    let mut status = status_clone_2.lock().unwrap();
                                                    status.active = false;
                                                    ctx_clone_2.request_repaint();
                                                    buffer.clear();
                                                    break;
                                                }
                                                if let Some((speed, downloaded, peers)) =
                                                    parse_webtorrent_line(&buffer)
                                                {
                                                    write_torrent_progress(
                                                        &progress_dir,
                                                        &downloaded,
                                                    );
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

                        if let Some(media_path) = local_media.clone() {
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

                            let delete_file_btn = ui.add_sized(
                                [150.0, 40.0],
                                egui::Button::new("🗑 Delete File")
                                    .fill(egui::Color32::from_rgb(145, 70, 35)),
                            );
                            if delete_file_btn.clicked() {
                                delete_local_media_file(&cache_dir_path, &media_path);
                            }
                        }

                        let delete_btn = ui.add_sized(
                            [120.0, 40.0],
                            egui::Button::new("🗑 Delete Link")
                                .fill(egui::Color32::from_rgb(180, 40, 40)),
                        );
                        if delete_btn.clicked() {
                            let path = get_cache_dir().join(&torrent.dir_name);
                            if path.exists() {
                                let _ = fs::remove_dir_all(&path);
                            }
                        }
                    });
                });
            });
            ui.add_space(8.0);
        }
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
