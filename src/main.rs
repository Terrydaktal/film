use eframe::egui;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, Read};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Serialize, Deserialize, Debug, Clone)]
struct TorrentOption {
    quality: String,
    size: String,
    hash: String,
    url: String,
    #[serde(default)]
    source_url: String,
    seeds: Option<u32>,
    peers: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct MovieMetadata {
    title: String,
    url: String,
    #[serde(default)]
    source_url: String,
    film_title: Option<String>,
    year: Option<u16>,
    source_label: Option<String>,
    #[serde(default)]
    media_kind: MediaKind,
    duration: Option<String>,
    seeds: Option<u32>,
    peers: Option<u32>,
    torrent_options: Option<Vec<TorrentOption>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
enum MediaKind {
    Movie,
    Episodic,
    Video,
    Other,
    #[default]
    Unclassified,
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
    media_kind: MediaKind,
}

#[derive(Clone)]
struct MovieGroup {
    key: String,
    title: String,
    year: Option<u16>,
    torrents: Vec<MovieCacheInfo>,
    total_size_bytes: u64,
    total_logical_size_bytes: u64,
    media_kind: MediaKind,
}

#[derive(Clone)]
struct PerTorrentStatus {
    speed: String,
    downloaded: String,
    peers: String,
    detail: String,
    mode: String,
    active: bool,
}

struct TorrentClientStatusUpdate {
    speed: Option<String>,
    downloaded: Option<String>,
    peers: Option<String>,
    complete: bool,
}

#[derive(Deserialize)]
struct LibtorrentWorkerEvent {
    event: String,
    downloaded: Option<String>,
    speed: Option<String>,
    peers: Option<String>,
    detail: Option<String>,
    complete: Option<bool>,
    level: Option<String>,
    message: Option<String>,
    path: Option<String>,
}

type TorrentStatusMap = HashMap<String, PerTorrentStatus>;

#[derive(Debug)]
enum BValue {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<BValue>),
    Dict(std::collections::BTreeMap<Vec<u8>, BValue>),
}

fn app_base_dir() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    for ancestor in exe_dir.ancestors() {
        if ancestor.join("Cargo.toml").exists()
            && ancestor.join("tools").is_dir()
            && ancestor.join("scripts").is_dir()
        {
            return ancestor.to_path_buf();
        }
        if ancestor.join("1337x_mirrors.txt").exists()
            || ancestor.join("yify_mirrors.txt").exists()
            || ancestor.join("solidtorrents_mirrors.txt").exists()
        {
            return ancestor.to_path_buf();
        }
    }

    exe_dir
}

fn app_resource_path(relative: impl AsRef<std::path::Path>) -> PathBuf {
    app_base_dir().join(relative)
}

fn get_cache_dir() -> PathBuf {
    app_resource_path("stream_cache")
}

fn cache_item_path(dir_name: &str) -> PathBuf {
    get_cache_dir().join(dir_name)
}

fn slugify_cache_title(title: &str) -> String {
    let mut slug = String::new();
    let mut last_was_sep = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep {
            slug.push('-');
            last_was_sep = true;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "film".to_string()
    } else {
        slug.chars().take(80).collect()
    }
}

fn filesystem_safe_title(title: &str) -> String {
    let cleaned = title
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                ' '
            } else {
                c
            }
        })
        .collect::<String>();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cache_root_dir_name(title: &str, year: Option<u16>, hash: &str) -> String {
    let mut display = filesystem_safe_title(title.trim());
    if let Some(year) = year {
        display = if display.is_empty() {
            format!("({})", year)
        } else {
            format!("{} ({})", display, year)
        };
    }
    if display.is_empty() {
        let clean_hash = hash
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .collect::<String>()
            .to_uppercase();
        if clean_hash.len() == 40 {
            clean_hash
        } else {
            slugify_cache_title(hash)
        }
    } else {
        display
    }
}

fn cache_root_hash_from_dir_name(dir_name: &str) -> Option<String> {
    dir_name
        .strip_prefix("torrent_")
        .or_else(|| dir_name.rsplit_once('_').map(|(_, hash)| hash))
        .and_then(|hash| {
            let clean_hash = hash
                .chars()
                .filter(|c| c.is_ascii_hexdigit())
                .collect::<String>()
                .to_uppercase();
            (clean_hash.len() == 40).then_some(clean_hash)
        })
}

fn metadata_root_hash(metadata: Option<&MovieMetadata>) -> Option<String> {
    let metadata = metadata?;
    get_infohash(&metadata.url)
        .or_else(|| get_infohash(&metadata.source_url))
        .or_else(|| {
            metadata.torrent_options.as_ref().and_then(|options| {
                options
                    .iter()
                    .find_map(|opt| {
                        get_infohash(&opt.url)
                            .or_else(|| get_infohash(&opt.source_url))
                            .or_else(|| {
                                let hash = opt.hash.trim();
                                (hash.len() == 40
                                    && hash.chars().all(|c| c.is_ascii_hexdigit()))
                                .then(|| hash.to_uppercase())
                            })
                    })
            })
        })
}

fn cache_root_hash(cache_dir: &std::path::Path, metadata: Option<&MovieMetadata>) -> Option<String> {
    torrent_file_info_hash(cache_dir)
        .or_else(|| metadata_root_hash(metadata))
        .or_else(|| {
            cache_dir
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(cache_root_hash_from_dir_name)
        })
}

fn torrent_option_cache_subdir(hash: &str) -> String {
    let clean_hash: String = hash
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_uppercase();
    if clean_hash.len() == 40 {
        clean_hash
    } else {
        let safe_hash: String = hash
            .chars()
            .map(|c| if matches!(c, '/' | '\\' | ':') { '_' } else { c })
            .collect();
        safe_hash
    }
}

fn torrent_file_info_hash(cache_dir: &std::path::Path) -> Option<String> {
    let torrent_bytes = fs::read(cache_dir.join("movie.torrent")).ok()?;
    let info_bytes = bencoded_info_dict_bytes(&torrent_bytes)?;
    let digest = Sha1::digest(info_bytes);
    Some(digest.iter().map(|byte| format!("{:02X}", byte)).collect())
}

fn skip_bencoded_value(data: &[u8], pos: &mut usize) -> Option<()> {
    match *data.get(*pos)? {
        b'i' => {
            *pos += 1;
            while *data.get(*pos)? != b'e' {
                *pos += 1;
            }
            *pos += 1;
            Some(())
        }
        b'l' | b'd' => {
            *pos += 1;
            while *data.get(*pos)? != b'e' {
                skip_bencoded_value(data, pos)?;
            }
            *pos += 1;
            Some(())
        }
        b'0'..=b'9' => {
            let start = *pos;
            while *data.get(*pos)? != b':' {
                *pos += 1;
            }
            let len: usize = std::str::from_utf8(&data[start..*pos]).ok()?.parse().ok()?;
            *pos += 1;
            *pos = pos.checked_add(len)?;
            if *pos <= data.len() {
                Some(())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn bencoded_info_dict_bytes(data: &[u8]) -> Option<&[u8]> {
    if *data.first()? != b'd' {
        return None;
    }

    let mut pos = 1;
    while *data.get(pos)? != b'e' {
        let key_start = pos;
        while *data.get(pos)? != b':' {
            pos += 1;
        }
        let key_len: usize = std::str::from_utf8(&data[key_start..pos])
            .ok()?
            .parse()
            .ok()?;
        pos += 1;
        let key_end = pos.checked_add(key_len)?;
        if key_end > data.len() {
            return None;
        }
        let key = &data[pos..key_end];
        pos = key_end;

        let value_start = pos;
        skip_bencoded_value(data, &mut pos)?;
        if key == b"info" {
            return Some(&data[value_start..pos]);
        }
    }

    None
}

fn torrent_option_cache_path(
    parent_cache_path: &std::path::Path,
    quality: &str,
    hash: &str,
    size_hint: Option<u64>,
    allow_legacy_size_match: bool,
) -> PathBuf {
    let hash_path = parent_cache_path.join(torrent_option_cache_subdir(hash));
    if hash_path.exists() {
        return hash_path;
    }

    let parent_hash_matches = torrent_file_info_hash(parent_cache_path)
        .is_some_and(|parent_hash| parent_hash.eq_ignore_ascii_case(hash));
    if parent_hash_matches {
        return parent_cache_path.to_path_buf();
    }

    let legacy_quality_path = parent_cache_path.join(quality);
    let legacy_hash_matches = torrent_file_info_hash(&legacy_quality_path)
        .is_some_and(|legacy_hash| legacy_hash.eq_ignore_ascii_case(hash));
    let legacy_size_matches = allow_legacy_size_match && size_hint
        .is_some_and(|size_hint| legacy_cache_size_matches(&legacy_quality_path, size_hint));
    if legacy_quality_path.exists() && (legacy_hash_matches || legacy_size_matches) {
        legacy_quality_path
    } else {
        hash_path
    }
}

fn legacy_cache_size_bytes(cache_dir: &std::path::Path) -> Option<u64> {
    saved_torrent_file_progress_totals(cache_dir)
        .map(|(_, total)| total)
        .or_else(|| {
            let disk_size = get_payload_disk_space(cache_dir);
            (disk_size > 0).then_some(disk_size)
        })
}

fn legacy_cache_size_matches(cache_dir: &std::path::Path, size_hint: u64) -> bool {
    if size_hint == 0 {
        return false;
    }
    let Some(legacy_size) = legacy_cache_size_bytes(cache_dir) else {
        return false;
    };
    let diff = legacy_size.abs_diff(size_hint);
    let tolerance = (size_hint / 12).max(64 * 1024 * 1024);
    diff <= tolerance
}

fn legacy_cache_size_best_matches_option(
    cache_dir: &std::path::Path,
    current_hash: &str,
    current_quality: &str,
    current_size_hint: Option<u64>,
    options: &[TorrentOption],
) -> bool {
    let Some(current_size_hint) = current_size_hint else {
        return false;
    };
    let Some(legacy_size) = legacy_cache_size_bytes(cache_dir) else {
        return false;
    };
    let current_diff = legacy_size.abs_diff(current_size_hint);
    let tolerance = (current_size_hint / 12).max(64 * 1024 * 1024);
    if current_diff > tolerance {
        return false;
    }

    for opt in options {
        if !opt.quality.eq_ignore_ascii_case(current_quality)
            || opt.hash.eq_ignore_ascii_case(current_hash)
        {
            continue;
        }
        if let Some(other_size_hint) = parse_size_to_bytes(&opt.size) {
            let other_diff = legacy_size.abs_diff(other_size_hint);
            if other_diff < current_diff {
                return false;
            }
            if other_diff == current_diff && opt.hash.as_str() < current_hash {
                return false;
            }
        }
    }

    true
}

fn sibling_option_cache_subdirs(
    option_markers: &[(String, String, String)],
    current_hash: &str,
) -> Vec<String> {
    let mut excluded = Vec::new();
    for (hash, quality, option_subdir) in option_markers {
        if hash.eq_ignore_ascii_case(current_hash) {
            continue;
        }
        excluded.push(quality.clone());
        excluded.push(option_subdir.clone());
    }
    excluded.sort();
    excluded.dedup();
    excluded
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
        return augment_magnet_trackers(url);
    }

    if let Some(h) = get_infohash(url) {
        make_magnet_link(&h, "Movie")
    } else {
        url.to_string()
    }
}

fn torrent_source_for_launch(url: &str, local_torrent_path: &str) -> String {
    let trimmed = url.trim();
    if trimmed.starts_with("magnet:") {
        return augment_magnet_trackers(trimmed);
    }
    if get_infohash(trimmed).is_some() {
        return get_magnet_uri(trimmed);
    }
    if std::path::Path::new(local_torrent_path).exists() {
        return local_torrent_path.to_string();
    }
    trimmed.to_string()
}

fn launch_source_from_url_or_hash(url: &str, hash: &str, title: &str, local_torrent_path: &str) -> String {
    let source = torrent_source_for_launch(url, local_torrent_path);
    if !source.trim().is_empty() {
        return source;
    }
    let clean_hash: String = hash
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_uppercase();
    if clean_hash.len() == 40 {
        make_magnet_link(&clean_hash, title)
    } else {
        source
    }
}

fn launch_magnet_for_display(url: &str, hash: &str, title: &str) -> String {
    let trimmed = url.trim();
    if trimmed.starts_with("magnet:") {
        return augment_magnet_trackers(trimmed);
    }
    if let Some(info_hash) = get_infohash(trimmed) {
        return make_magnet_link(&info_hash, title);
    }
    let clean_hash: String = hash
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_uppercase();
    if clean_hash.len() == 40 {
        make_magnet_link(&clean_hash, title)
    } else {
        String::new()
    }
}

fn display_source_url(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        "(not stored; generated from info hash)".to_string()
    } else {
        trimmed.to_string()
    }
}

fn should_show_source_url(url: &str, launch_magnet: &str) -> bool {
    let trimmed = url.trim();
    !trimmed.is_empty()
        && !trimmed.starts_with("magnet:")
        && trimmed != launch_magnet.trim()
}

// Helper to locate the largest media file inside a directory recursively
fn find_media_file(dir: &std::path::Path) -> Option<PathBuf> {
    find_media_file_excluding(dir, &[])
}

fn find_media_file_excluding(dir: &std::path::Path, excluded_direct_children: &[String]) -> Option<PathBuf> {
    let mut largest_file = None;
    let mut max_size = 0;
    let mut best_progress_score = 0;

    fn visit_dirs(
        root: &std::path::Path,
        dir: &std::path::Path,
        excluded_direct_children: &[String],
        largest: &mut Option<PathBuf>,
        max_sz: &mut u64,
        best_progress_score: &mut u64,
    ) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if is_under_excluded_direct_child(root, &path, excluded_direct_children) {
                    continue;
                }
                if path.is_dir() {
                    visit_dirs(
                        root,
                        &path,
                        excluded_direct_children,
                        largest,
                        max_sz,
                        best_progress_score,
                    );
                } else if path.is_file() {
                    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                        let ext_lower = ext.to_lowercase();
                        if ["mp4", "mkv", "avi", "m4v", "mov", "flv", "webm"]
                            .contains(&ext_lower.as_str())
                        {
                            if let Ok(meta) = path.metadata() {
                                let media_size = meta.len();
                                let progress_score = media_progress_score(root, &path);
                                if media_size > *max_sz
                                    || (media_size == *max_sz
                                        && progress_score > *best_progress_score)
                                {
                                    *max_sz = media_size;
                                    *best_progress_score = progress_score;
                                    *largest = Some(path.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    visit_dirs(
        dir,
        dir,
        excluded_direct_children,
        &mut largest_file,
        &mut max_size,
        &mut best_progress_score,
    );
    largest_file
}

fn find_largest_payload_file_excluding(
    dir: &std::path::Path,
    excluded_direct_children: &[String],
) -> Option<PathBuf> {
    let mut largest_file = None;
    let mut largest_size = 0;

    fn visit(
        root: &std::path::Path,
        dir: &std::path::Path,
        excluded_direct_children: &[String],
        largest_file: &mut Option<PathBuf>,
        largest_size: &mut u64,
    ) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if is_under_excluded_direct_child(root, &path, excluded_direct_children) {
                continue;
            }
            if path.is_dir() {
                visit(
                    root,
                    &path,
                    excluded_direct_children,
                    largest_file,
                    largest_size,
                );
            } else if path.is_file() && !is_control_artifact_path(&path) {
                let size = path.metadata().map(|metadata| metadata.len()).unwrap_or(0);
                if size > *largest_size {
                    *largest_size = size;
                    *largest_file = Some(path);
                }
            }
        }
    }

    visit(
        dir,
        dir,
        excluded_direct_children,
        &mut largest_file,
        &mut largest_size,
    );
    largest_file
}

fn media_progress_score(root: &std::path::Path, media_path: &std::path::Path) -> u64 {
    let mut current = media_path.parent();
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    while let Some(dir) = current {
        let progress_path = progress_file_path(dir);
        if progress_path.exists() {
            let score = progress_file_quality_score(&progress_path);
            if score > 0 {
                return score;
            }
        }
        let canonical_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        if canonical_dir == root {
            break;
        }
        current = dir.parent();
    }
    0
}

fn is_under_excluded_direct_child(
    root: &std::path::Path,
    path: &std::path::Path,
    excluded_direct_children: &[String],
) -> bool {
    if excluded_direct_children.is_empty() {
        return false;
    }
    path.strip_prefix(root)
        .ok()
        .and_then(|relative| relative.components().next())
        .and_then(|component| match component {
            std::path::Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .is_some_and(|name| excluded_direct_children.iter().any(|excluded| excluded == name))
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
    reset_deleted_torrent_progress(cache_dir);
    if let Some(parent) = media_path.parent() {
        remove_empty_dirs_up_to(parent.to_path_buf(), cache_dir);
    }
}

fn reset_deleted_torrent_progress(cache_dir: &std::path::Path) {
    let file_progress_path = cache_dir.join(".torrent_file_progress.json");
    let mut total_bytes = None;
    if let Ok(content) = fs::read_to_string(&file_progress_path) {
        if let Ok(mut entries) = serde_json::from_str::<Vec<serde_json::Value>>(&content) {
            let mut total = 0_u64;
            for entry in &mut entries {
                if let Some(object) = entry.as_object_mut() {
                    total = total.saturating_add(
                        object
                            .get("total")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0),
                    );
                    object.insert("downloaded".to_string(), serde_json::json!(0));
                }
            }
            if let Ok(reset_content) = serde_json::to_string(&entries) {
                let _ = fs::write(&file_progress_path, reset_content);
                total_bytes = Some(total);
            }
        }
    }

    let progress_path = progress_file_path(cache_dir);
    let mut progress = fs::read_to_string(&progress_path)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    progress.insert("downloaded_bytes".to_string(), serde_json::json!(0));
    progress.insert("ranges".to_string(), serde_json::json!([]));
    progress.remove("playable_prefix_ratio");
    progress.remove("contiguous_prefix_bytes");
    progress.remove("player_safe_prefix_bytes");
    if let Some(total) = total_bytes {
        progress.insert("total_bytes".to_string(), serde_json::json!(total));
    }
    if let Ok(content) = serde_json::to_string(&serde_json::Value::Object(progress)) {
        let _ = fs::write(progress_path, content);
    }
}

fn status_speed_is_rate(speed: &str) -> bool {
    let trimmed = speed.trim().to_ascii_lowercase();
    trimmed.ends_with("b/s")
        || trimmed.ends_with("kb/s")
        || trimmed.ends_with("mb/s")
        || trimmed.ends_with("gb/s")
        || trimmed.ends_with("kib/s")
        || trimmed.ends_with("mib/s")
        || trimmed.ends_with("gib/s")
}

fn format_live_torrent_status(
    downloaded: &str,
    expected_total: &str,
    speed: &str,
    peers: &str,
    detail: &str,
    mode: &str,
) -> String {
    let progress = if downloaded.contains('/') || expected_total.trim().is_empty() {
        downloaded.trim().to_string()
    } else {
        format!("{} / {}", downloaded.trim(), expected_total.trim())
    };
    let peers = peers.trim();
    let detail = detail.trim();
    let mode = mode.trim();
    let extra = [(!mode.is_empty()).then_some(mode), (!detail.is_empty()).then_some(detail)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let extra_suffix = if extra.is_empty() {
        String::new()
    } else {
        format!(" · {}", extra.join(" · "))
    };

    if status_speed_is_rate(speed) {
        if peers.is_empty() {
            format!("{} (⬇ {}{})", progress, speed.trim(), extra_suffix)
        } else {
            format!(
                "{} (⬇ {} · live peers {}{})",
                progress,
                speed.trim(),
                peers,
                extra_suffix
            )
        }
    } else if peers.is_empty() {
        format!("{} ({}{})", progress, speed.trim(), extra_suffix)
    } else {
        format!(
            "{} ({} · live peers {}{})",
            progress,
            speed.trim(),
            peers,
            extra_suffix
        )
    }
}

fn preferred_trackers() -> &'static [&'static str] {
    &[
        "udp://tracker.opentrackr.org:1337/announce",
        "udp://open.demonii.com:1337/announce",
        "udp://open.stealth.si:80/announce",
        "udp://tracker.torrent.eu.org:451/announce",
        "udp://explodie.org:6969/announce",
        "udp://exodus.desync.com:6969/announce",
        "udp://p4p.arenabg.com:1337/announce",
        "udp://tracker.cyberia.is:6969/announce",
        "udp://ipv4.tracker.harry.lu:80/announce",
        "udp://tracker2.dler.org:80/announce",
        "udp://movies.zsw.ca:6969/announce",
        "udp://tracker.theoks.net:6969/announce",
        "http://tracker.opentrackr.org:1337/announce",
        "https://tracker1.520.jp:443/announce",
    ]
}

fn augment_magnet_trackers(magnet: &str) -> String {
    if !magnet.trim_start().starts_with("magnet:") {
        return magnet.to_string();
    }
    let mut out = magnet.trim().to_string();
    for tracker in preferred_trackers() {
        let encoded = percent_encode(tracker);
        let needle = format!("tr={}", encoded);
        if !out.contains(&needle) {
            out.push_str("&tr=");
            out.push_str(&encoded);
        }
    }
    out
}

fn libtorrent_worker_path() -> PathBuf {
    app_resource_path("tools/libtorrent_worker.py")
}

fn build_libtorrent_worker_command(
    source: &str,
    save_path: &str,
    display_name: &str,
    sequential: bool,
    sequential_start_mib: Option<u32>,
) -> Command {
    let worker_path = libtorrent_worker_path();
    let app_base = app_base_dir();
    let venv_python = app_base.join(".venv").join("bin").join("python");
    let home_uv = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".local").join("bin").join("uv"));

    let mut cmd = if venv_python.exists() {
        let mut cmd = Command::new(venv_python);
        cmd.arg(worker_path);
        cmd
    } else if let Some(home_uv) = home_uv.filter(|path| path.exists()) {
        let mut cmd = Command::new(home_uv);
        cmd.env("UV_CACHE_DIR", "/data/.cache/uv");
        cmd.args(["run", "python"]);
        cmd.arg(worker_path);
        cmd
    } else {
        let mut cmd = Command::new("uv");
        cmd.env("UV_CACHE_DIR", "/data/.cache/uv");
        cmd.args(["run", "python"]);
        cmd.arg(worker_path);
        cmd
    };
    cmd.current_dir(&app_base);

    cmd.args([
        "--source",
        source,
        "--save-path",
        save_path,
        "--display-name",
        display_name,
    ]);
    if sequential {
        cmd.arg("--sequential");
    }
    if let Some(start_mib) = sequential_start_mib.filter(|start_mib| *start_mib > 0) {
        cmd.args(["--sequential-start-mib", &start_mib.to_string()]);
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd
}

fn kill_managed_torrent_processes() {
    let _ = Command::new("pkill")
        .args(["-9", "-i", "-f", "webtorrent"])
        .status();
    let _ = Command::new("pkill")
        .args(["-9", "-i", "-f", "transmission-cli"])
        .status();
    let _ = Command::new("pkill")
        .args(["-9", "-i", "-f", "libtorrent_worker.py"])
        .status();
    let _ = Command::new("pkill")
        .args(["-9", "-i", "-f", "torrent_stream_proxy.py"])
        .status();
}

fn delete_dir_with_retries(dir_path: &std::path::Path) -> std::io::Result<()> {
    let mut last_err = None;
    for _ in 0..10 {
        match fs::remove_dir_all(dir_path) {
            Ok(()) => return Ok(()),
            Err(_err) if !dir_path.exists() => return Ok(()),
            Err(err) => {
                last_err = Some(err);
                thread::sleep(Duration::from_millis(150));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("failed to delete directory")))
}

fn mark_torrent_complete(
    status_map: &Arc<Mutex<TorrentStatusMap>>,
    status_key: &str,
    progress_dir: &std::path::Path,
    total_hint: Option<u64>,
    ctx: &egui::Context,
) {
    if let Some(total) = total_hint {
        write_torrent_progress(
            progress_dir,
            &format!("{}/{}", format_size(total), format_size(total)),
        );
    }
    let mut map = status_map.lock().unwrap();
    if let Some(s) = map.get_mut(status_key) {
        s.active = false;
        s.speed = "Complete".to_string();
        if let Some(total) = total_hint {
            s.downloaded = format!("{}/{}", format_size(total), format_size(total));
        }
        s.peers.clear();
        s.detail.clear();
    }
    ctx.request_repaint();
}

fn normalize_worker_downloaded_text(
    progress_dir: &std::path::Path,
    downloaded: &str,
) -> String {
    normalized_torrent_progress_text(progress_dir, downloaded)
        .unwrap_or_else(|| downloaded.to_string())
}

fn parse_libtorrent_worker_event(
    line: &str,
    progress_dir: &std::path::Path,
) -> Option<TorrentClientStatusUpdate> {
    let event = serde_json::from_str::<LibtorrentWorkerEvent>(line).ok()?;
    if event.event != "status" {
        return None;
    }
    let downloaded = event
        .downloaded
        .as_ref()
        .map(|value| normalize_worker_downloaded_text(progress_dir, value));
    Some(TorrentClientStatusUpdate {
        speed: event.speed,
        downloaded,
        peers: event.peers,
        complete: event.complete.unwrap_or(false),
    })
}

fn spawn_torrent_client_output_reader<R>(
    reader: R,
    log_prefix: &'static str,
    status_map: Arc<Mutex<TorrentStatusMap>>,
    status_key: String,
    progress_dir: PathBuf,
    total_hint: Option<u64>,
    ctx: egui::Context,
)
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let reader = std::io::BufReader::new(reader);
        for line_res in reader.lines().flatten() {
            let line = line_res.trim_end().to_string();
            if !line.trim().is_empty() {
                println!("[{}] {}", log_prefix, line);
            }

            let worker_event = serde_json::from_str::<LibtorrentWorkerEvent>(&line).ok();
            if let Some(event) = worker_event {
                match event.event.as_str() {
                    "status" => {
                        if let Some(ref downloaded) = event.downloaded {
                            write_torrent_progress(&progress_dir, downloaded);
                        }
                        let mut map = status_map.lock().unwrap();
                        if let Some(s) = map.get_mut(&status_key) {
                            if let Some(speed) = event.speed {
                                s.speed = speed;
                            }
                            if let Some(downloaded) = event.downloaded {
                                s.downloaded =
                                    normalize_worker_downloaded_text(&progress_dir, &downloaded);
                            }
                            if let Some(peers) = event.peers {
                                s.peers = peers;
                            }
                            s.detail = event.detail.unwrap_or_default();
                            if event.complete.unwrap_or(false) {
                                s.active = false;
                                s.speed = "Complete".to_string();
                                s.peers.clear();
                                s.detail.clear();
                            }
                        }
                        if event.complete.unwrap_or(false) {
                            mark_torrent_complete(
                                &status_map,
                                &status_key,
                                &progress_dir,
                                total_hint,
                                &ctx,
                            );
                        }
                        ctx.request_repaint();
                    }
                    "metadata_saved" => {
                        if let Some(path) = event.path {
                            let _ = fs::metadata(path);
                        }
                        let mut map = status_map.lock().unwrap();
                        if let Some(s) = map.get_mut(&status_key) {
                            if matches!(
                                s.speed.trim(),
                                "" | "Connecting..." | "Fetching metadata" | "Waiting for peers..."
                            ) {
                                s.speed = "Metadata ready".to_string();
                            }
                        }
                        ctx.request_repaint();
                    }
                    "log" => {
                        let level = event.level.unwrap_or_default().to_ascii_lowercase();
                        let message = event.message.unwrap_or_default();
                        let mut map = status_map.lock().unwrap();
                        if let Some(s) = map.get_mut(&status_key) {
                            if level == "error" {
                                s.active = false;
                                s.speed = "Error".to_string();
                                s.peers = message;
                                s.detail.clear();
                            } else if !status_speed_is_rate(s.speed.trim()) && !message.is_empty() {
                                s.detail = message;
                            }
                        }
                        ctx.request_repaint();
                    }
                    _ => {}
                }
                continue;
            }

            if let Some(update) = parse_libtorrent_worker_event(&line, &progress_dir) {
                if let Some(ref downloaded) = update.downloaded {
                    write_torrent_progress(&progress_dir, downloaded);
                }
                let mut map = status_map.lock().unwrap();
                if let Some(s) = map.get_mut(&status_key) {
                    if let Some(speed) = update.speed {
                        s.speed = speed;
                    }
                    if let Some(downloaded) = update.downloaded {
                        s.downloaded = normalized_torrent_progress_text(&progress_dir, &downloaded)
                            .unwrap_or(downloaded);
                    }
                    if let Some(peers) = update.peers {
                        s.peers = peers;
                    }
                    if update.complete || status_speed_is_rate(s.speed.trim()) {
                        s.detail.clear();
                    }
                    if update.complete {
                        s.active = false;
                        s.speed = "Complete".to_string();
                    }
                }
                    if update.complete {
                        mark_torrent_complete(
                            &status_map,
                            &status_key,
                            &progress_dir,
                            total_hint,
                            &ctx,
                    );
                }
                ctx.request_repaint();
            }
        }
    });
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

fn title_looks_episodic(raw_name: &str) -> bool {
    let lower = raw_name.to_lowercase();
    let bytes = lower.as_bytes();
    let spaced = lower
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for window in bytes.windows(6) {
        if window[0] == b's'
            && window[1].is_ascii_digit()
            && window[2].is_ascii_digit()
            && window[3] == b'e'
            && window[4].is_ascii_digit()
            && window[5].is_ascii_digit()
        {
            return true;
        }
    }
    for window in bytes.windows(8) {
        if window[0] == b's'
            && window[1].is_ascii_digit()
            && window[2].is_ascii_digit()
            && (window[3] == b' ' || window[3] == b'.' || window[3] == b'-' || window[3] == b'_')
            && window[4] == b'e'
            && window[5] == b'p'
            && window[6].is_ascii_digit()
            && window[7].is_ascii_digit()
        {
            return true;
        }
    }
    for (idx, window) in bytes.windows(4).enumerate() {
        let prev_ok = idx == 0 || !bytes[idx - 1].is_ascii_digit();
        let next_idx = idx + 4;
        let next_ok = next_idx >= bytes.len() || !bytes[next_idx].is_ascii_digit();
        if prev_ok
            && next_ok
            && window[0].is_ascii_digit()
            && window[1] == b'x'
            && window[2].is_ascii_digit()
            && window[3].is_ascii_digit()
        {
            return true;
        }
    }
    [
        "season 1",
        "season 2",
        "season 3",
        "season 4",
        "season 5",
        "episode 1",
        "episode 2",
        "episode 3",
        "episode 4",
        "episode 5",
        "episode 6",
        "episode 7",
        "episode 8",
        "episode 9",
        "episode 10",
        "ep 01",
        "ep 02",
        "ep 03",
        "ep 04",
        "ep 05",
        "ep 06",
        "ep 07",
        "ep 08",
        "ep 09",
        "ep 10",
        "ep01",
        "ep02",
        "ep03",
        "ep04",
        "ep05",
        "ep06",
        "ep07",
        "ep08",
        "ep09",
        "ep10",
        " complete season",
        " series ",
        " tv pack",
        " webrip proper s",
        " hdtv s",
    ]
    .iter()
    .any(|needle| spaced.contains(needle) || lower.contains(needle))
}

fn title_has_specific_episode_marker(raw_name: &str) -> bool {
    let lower = raw_name.to_lowercase();
    let bytes = lower.as_bytes();
    let spaced = lower
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    for window in bytes.windows(6) {
        if window[0] == b's'
            && window[1].is_ascii_digit()
            && window[2].is_ascii_digit()
            && window[3] == b'e'
            && window[4].is_ascii_digit()
            && window[5].is_ascii_digit()
        {
            return true;
        }
    }
    for window in bytes.windows(8) {
        if window[0] == b's'
            && window[1].is_ascii_digit()
            && window[2].is_ascii_digit()
            && (window[3] == b' ' || window[3] == b'.' || window[3] == b'-' || window[3] == b'_')
            && window[4] == b'e'
            && window[5] == b'p'
            && window[6].is_ascii_digit()
            && window[7].is_ascii_digit()
        {
            return true;
        }
    }
    for window in bytes.windows(4) {
        if window[0].is_ascii_digit()
            && window[1] == b'x'
            && window[2].is_ascii_digit()
            && window[3].is_ascii_digit()
        {
            return true;
        }
    }

    [
        "episode 1",
        "episode 2",
        "episode 3",
        "episode 4",
        "episode 5",
        "episode 6",
        "episode 7",
        "episode 8",
        "episode 9",
        "episode 10",
        "ep 01",
        "ep 02",
        "ep 03",
        "ep 04",
        "ep 05",
        "ep 06",
        "ep 07",
        "ep 08",
        "ep 09",
        "ep 10",
        "ep01",
        "ep02",
        "ep03",
        "ep04",
        "ep05",
        "ep06",
        "ep07",
        "ep08",
        "ep09",
        "ep10",
    ]
    .iter()
    .any(|needle| spaced.contains(needle))
}

fn title_looks_clearly_non_video(raw_name: &str) -> bool {
    let lower = format!(" {} ", raw_name.to_lowercase());
    let strong_other_markers = [
        " flac ",
        " mp3 ",
        " discography ",
        " ebook ",
        " pdf ",
        " epub ",
        " audiobook ",
        " apk ",
        " android ",
        " windows ",
        " linux ",
        " macos ",
        " game ",
        " igggames",
        " setup ",
        " iso ",
        " soundtrack ",
        " album ",
    ];
    let video_markers = [
        " 2160p ",
        " 1080p ",
        " 720p ",
        " webrip ",
        " web-dl ",
        " bluray ",
        " brrip ",
        " hdrip ",
        " hdtv ",
        " dvdrip ",
        " x264 ",
        " x265 ",
        " h264 ",
        " h265 ",
        " hevc ",
        " mkv ",
        " mp4 ",
        " avi ",
    ];
    strong_other_markers.iter().any(|needle| lower.contains(needle))
        && !video_markers.iter().any(|needle| lower.contains(needle))
}

fn title_has_strong_video_release_markers(raw_name: &str) -> bool {
    let lower = raw_name.to_lowercase();
    let spaced = lower
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    [
        "2160p",
        "1080p",
        "720p",
        "480p",
        "webrip",
        "web dl",
        "hdrip",
        "hdts",
        "hdtv",
        "bluray",
        "brrip",
        "dvdrip",
        "cam",
        "telesync",
        "x264",
        "x265",
        "h264",
        "h265",
        "hevc",
        "aac",
        "ddp5 1",
        "dd5 1",
        "mkv",
        "mp4",
        "avi",
    ]
    .iter()
    .any(|needle| spaced.contains(needle) || lower.contains(needle))
}

fn title_has_day_month_year_date_marker(raw_name: &str) -> bool {
    let tokens: Vec<&str> = raw_name
        .split(|c: char| !c.is_ascii_digit())
        .filter(|token| !token.is_empty())
        .collect();
    tokens.windows(3).any(|window| {
        window[0].len() == 2
            && window[1].len() == 2
            && window[2].len() == 4
            && window[0].parse::<u8>().is_ok_and(|day| (1..=31).contains(&day))
            && window[1].parse::<u8>().is_ok_and(|month| (1..=12).contains(&month))
            && window[2]
                .parse::<u16>()
                .is_ok_and(|year| (1900..=2099).contains(&year))
    })
}

fn merge_media_kind(a: MediaKind, b: MediaKind) -> MediaKind {
    fn rank(kind: MediaKind) -> u8 {
        match kind {
            MediaKind::Movie => 5,
            MediaKind::Episodic => 4,
            MediaKind::Video => 3,
            MediaKind::Other => 2,
            MediaKind::Unclassified => 1,
        }
    }
    if rank(b) > rank(a) { b } else { a }
}

fn classify_search_media_kind(
    title: &str,
    runtime: Option<u32>,
    genres: Option<&[String]>,
) -> MediaKind {
    if title_looks_episodic(title) {
        return MediaKind::Episodic;
    }
    if title_looks_clearly_non_video(title) {
        return MediaKind::Other;
    }
    if let Some(runtime) = runtime {
        if runtime >= 40 {
            return MediaKind::Movie;
        }
        if runtime > 0 && runtime < 40 {
            return MediaKind::Episodic;
        }
    }
    if genres.is_some() {
        return MediaKind::Movie;
    }
    if title_has_strong_video_release_markers(title) {
        if year_from_title(title).is_some() && !title_has_day_month_year_date_marker(title) {
            return MediaKind::Movie;
        }
        return MediaKind::Video;
    }
    if title_has_day_month_year_date_marker(title) {
        return MediaKind::Other;
    }
    MediaKind::Unclassified
}

fn torrent_options_have_video_quality_markers(torrents: &[YtsTorrent]) -> bool {
    torrents.iter().any(|torrent| {
        matches!(
            torrent.quality.as_str(),
            "2160p" | "1080p" | "720p" | "480p"
        )
    })
}

fn classify_search_media_kind_with_torrents(
    title: &str,
    runtime: Option<u32>,
    genres: Option<&[String]>,
    torrents: &[YtsTorrent],
) -> MediaKind {
    let base = classify_search_media_kind(title, runtime, genres);
    if base != MediaKind::Unclassified {
        return base;
    }
    if year_from_title(title).is_some() && torrent_options_have_video_quality_markers(torrents) {
        return MediaKind::Movie;
    }
    if torrent_options_have_video_quality_markers(torrents) {
        return MediaKind::Video;
    }
    MediaKind::Unclassified
}

fn effective_search_result_media_kind(movie: &YtsMovie) -> MediaKind {
    if movie.media_kind != MediaKind::Unclassified {
        return movie.media_kind;
    }
    let raw_title = movie
        .title_long
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or(&movie.title);
    let primary = classify_search_media_kind_with_torrents(
        raw_title,
        movie.runtime,
        movie.genres.as_deref(),
        &movie.torrents,
    );
    if primary != MediaKind::Unclassified {
        return primary;
    }
    let fallback = classify_search_media_kind_with_torrents(
        &movie.title,
        movie.runtime,
        movie.genres.as_deref(),
        &movie.torrents,
    );
    if fallback != MediaKind::Unclassified {
        return fallback;
    }
    if title_has_strong_video_release_markers(raw_title)
        || title_has_strong_video_release_markers(&movie.title)
    {
        return MediaKind::Video;
    }
    MediaKind::Unclassified
}

fn classify_cached_media_kind(
    cache_dir: &std::path::Path,
    metadata: Option<&MovieMetadata>,
    local_media: Option<&PathBuf>,
) -> MediaKind {
    let raw_title = metadata
        .and_then(|meta| meta.film_title.clone())
        .or_else(|| metadata.map(|meta| meta.title.clone()))
        .or_else(|| torrent_info_name(cache_dir))
        .or_else(|| {
            local_media.and_then(|path| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(ToOwned::to_owned)
            })
        })
        .unwrap_or_default();

    let hinted = metadata
        .map(|meta| meta.media_kind)
        .unwrap_or(MediaKind::Unclassified);
    let inferred = if local_media.is_some() {
        if title_looks_episodic(&raw_title) {
            MediaKind::Episodic
        } else {
            MediaKind::Movie
        }
    } else if title_looks_episodic(&raw_title) {
        MediaKind::Episodic
    } else if title_looks_clearly_non_video(&raw_title) {
        MediaKind::Other
    } else {
        MediaKind::Unclassified
    };

    merge_media_kind(hinted, inferred)
}

fn media_kind_label(kind: MediaKind, title: &str) -> &'static str {
    match kind {
        MediaKind::Movie => "Movie",
        MediaKind::Episodic => {
            if title_has_specific_episode_marker(title) {
                "Episode"
            } else {
                "Series"
            }
        }
        MediaKind::Video => "Video",
        MediaKind::Other => "Other",
        MediaKind::Unclassified => "Unclassified",
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
                let mut needs_metadata_save = false;

                // Lazy-load video duration and cache it in metadata.json
                let local_media = find_media_file(&path);
                if let Some(ref media_path) = local_media {
                    let duration = metadata.as_ref().and_then(|m| m.duration.clone());
                    if duration.is_none() {
                        if let Some(dur_str) = get_video_duration(media_path) {
                            if let Some(ref mut meta) = metadata {
                                meta.duration = Some(dur_str);
                                needs_metadata_save = true;
                            } else {
                                let (film_title, year, source_label) = cache_movie_identity(&path, None);
                                let display_title = match year {
                                    Some(y) => format!("{} ({})", film_title, y),
                                    None => film_title.clone(),
                                };
                                metadata = Some(MovieMetadata {
                                    title: display_title,
                                    url: "".to_string(),
                                    source_url: String::new(),
                                    film_title: Some(film_title),
                                    year,
                                    source_label: Some(source_label),
                                    media_kind: MediaKind::Movie,
                                    duration: Some(dur_str),
                                    seeds: None,
                                    peers: None,
                                    torrent_options: None,
                                });
                                needs_metadata_save = true;
                            }
                        }
                    }
                }

                let inferred_kind =
                    classify_cached_media_kind(&path, metadata.as_ref(), local_media.as_ref());
                if let Some(ref mut meta) = metadata {
                    if meta.media_kind != inferred_kind {
                        meta.media_kind = inferred_kind;
                        needs_metadata_save = true;
                    }
                } else if inferred_kind != MediaKind::Unclassified {
                    let (film_title, year, source_label) = cache_movie_identity(&path, None);
                    let display_title = match year {
                        Some(y) => format!("{} ({})", film_title, y),
                        None => film_title.clone(),
                    };
                    metadata = Some(MovieMetadata {
                        title: display_title,
                        url: "".to_string(),
                        source_url: String::new(),
                        film_title: Some(film_title),
                        year,
                        source_label: Some(source_label),
                        media_kind: inferred_kind,
                        duration: None,
                        seeds: None,
                        peers: None,
                        torrent_options: None,
                    });
                    needs_metadata_save = true;
                }

                if needs_metadata_save {
                    if let Some(ref meta) = metadata {
                        if let Ok(content) = serde_json::to_string_pretty(meta) {
                            let _ = fs::write(&metadata_path, content);
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
                    media_kind: inferred_kind,
                });
            }
        }
    }

    let mut groups: Vec<MovieGroup> = Vec::new();
    for movie in movies {
        if let Some(group) = groups.iter_mut().find(|group| group.key == movie.film_key) {
            group.total_size_bytes += movie.total_size_bytes;
            group.total_logical_size_bytes += movie.logical_size_bytes;
            group.media_kind = merge_media_kind(group.media_kind, movie.media_kind);
            group.torrents.push(movie);
        } else {
            groups.push(MovieGroup {
                key: movie.film_key.clone(),
                title: movie.film_title.clone(),
                year: movie.year,
                total_size_bytes: movie.total_size_bytes,
                total_logical_size_bytes: movie.logical_size_bytes,
                media_kind: movie.media_kind,
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

fn merge_torrent_options(
    existing: Option<Vec<TorrentOption>>,
    incoming: Vec<TorrentOption>,
) -> Vec<TorrentOption> {
    let mut merged = existing.unwrap_or_default();
    for opt in incoming {
        if let Some(existing_opt) = merged
            .iter_mut()
            .find(|existing_opt| existing_opt.hash.eq_ignore_ascii_case(&opt.hash))
        {
            existing_opt.quality = opt.quality;
            existing_opt.size = opt.size;
            existing_opt.url = opt.url;
            if !opt.source_url.is_empty() {
                existing_opt.source_url = opt.source_url;
            }
            existing_opt.seeds = opt.seeds;
            existing_opt.peers = opt.peers;
        } else {
            merged.push(opt);
        }
    }
    merged
}

fn find_existing_cache_dir_for_movie(
    film_title: &str,
    year: Option<u16>,
    incoming_hashes: &[String],
) -> Option<String> {
    let cache_dir = get_cache_dir();
    if !cache_dir.exists() {
        return None;
    }

    let target_key = normalize_film_key(film_title, year);
    for entry in fs::read_dir(cache_dir).ok()?.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let dir_name = entry.file_name().to_string_lossy().into_owned();
        let metadata_path = path.join("metadata.json");
        let metadata: Option<MovieMetadata> = if metadata_path.exists() {
            fs::read_to_string(&metadata_path)
                .ok()
                .and_then(|content| serde_json::from_str(&content).ok())
        } else {
            None
        };

        let (existing_title, existing_year, _) = cache_movie_identity(&path, metadata.as_ref());
        let existing_key = normalize_film_key(&existing_title, existing_year);
        if existing_key != target_key {
            continue;
        }
        let dir_hash = cache_root_hash(&path, metadata.as_ref());
        let dir_matches = dir_hash
            .as_ref()
            .is_some_and(|hash| incoming_hashes.iter().any(|incoming| incoming == hash));
        let option_matches = metadata
            .as_ref()
            .and_then(|meta| meta.torrent_options.as_ref())
            .is_some_and(|options| {
                options.iter().any(|opt| {
                    let hash = opt.hash.to_uppercase();
                    incoming_hashes.iter().any(|incoming| incoming == &hash)
                })
            });

        if dir_matches || option_matches {
            return Some(dir_name);
        }
        return Some(dir_name);
    }

    None
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

fn is_control_artifact_path(path: &std::path::Path) -> bool {
    if path
        .components()
        .any(|component| {
            matches!(
                component.as_os_str().to_str(),
                Some(".transmission") | Some(".hls_stream")
            )
        })
    {
        return true;
    }
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("movie.torrent")
            | Some(".torrent_progress.json")
            | Some(".torrent_file_progress.json")
            | Some(".stream_state.json")
            | Some(".player_safe_probe.log")
            | Some("torrent.fastresume")
            | Some("metadata.json")
            | Some(".transmission-finished")
    )
}

fn get_folder_disk_space_filtered(
    dir: &std::path::Path,
    include_path: &impl Fn(&std::path::Path) -> bool,
) -> u64 {
    let mut total = 0;
    fn visit(
        dir: &std::path::Path,
        total: &mut u64,
        include_path: &impl Fn(&std::path::Path) -> bool,
    ) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    visit(&path, total, include_path);
                } else if path.is_file() && include_path(&path) {
                    if let Ok(meta) = path.metadata() {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::MetadataExt;
                            *total += meta.blocks().saturating_mul(512);
                        }
                        #[cfg(not(unix))]
                        {
                            *total += meta.len();
                        }
                    }
                }
            }
        }
    }
    visit(dir, &mut total, include_path);
    total
}

fn get_folder_disk_space(dir: &std::path::Path) -> u64 {
    get_folder_disk_space_filtered(dir, &|_| true)
}

fn get_payload_disk_space(dir: &std::path::Path) -> u64 {
    get_folder_disk_space_filtered(dir, &|path| !is_control_artifact_path(path))
}

fn get_payload_disk_space_excluding(
    dir: &std::path::Path,
    excluded_direct_children: &[String],
) -> u64 {
    get_folder_disk_space_filtered(dir, &|path| {
        !is_control_artifact_path(path)
            && !is_under_excluded_direct_child(dir, path, excluded_direct_children)
    })
}

fn get_control_disk_space_excluding(
    dir: &std::path::Path,
    excluded_direct_children: &[String],
) -> u64 {
    get_folder_disk_space_filtered(dir, &|path| {
        is_control_artifact_path(path)
            && !is_under_excluded_direct_child(dir, path, excluded_direct_children)
    })
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

fn normalize_torrent_progress_update(
    dest_dir: &std::path::Path,
    downloaded: &str,
) -> Option<(u64, Option<u64>)> {
    if let Some((downloaded_bytes, total_bytes)) = saved_torrent_file_progress_totals(dest_dir) {
        return Some((downloaded_bytes, Some(total_bytes)));
    }
    let mut size_parts = downloaded.splitn(2, '/');
    let downloaded_part = size_parts.next().unwrap_or(downloaded);
    let total_part = size_parts.next();

    let parsed_downloaded = parse_size_to_bytes(downloaded_part)?;
    let existing = get_torrent_downloaded_and_total(dest_dir);
    let disk_used = get_payload_disk_space(dest_dir);
    let total_bytes = existing
        .map(|(_, total)| total)
        .or_else(|| total_part.and_then(parse_size_to_bytes))
        .or_else(|| find_media_file(dest_dir).and_then(|path| path.metadata().ok().map(|m| m.len())));
    let clamped_downloaded = existing
        .map(|(downloaded, _)| downloaded)
        .unwrap_or(0)
        .max(parsed_downloaded)
        .max(disk_used);

    Some((clamped_downloaded.min(total_bytes.unwrap_or(clamped_downloaded)), total_bytes))
}

fn normalized_torrent_progress_text(
    dest_dir: &std::path::Path,
    downloaded: &str,
) -> Option<String> {
    let (downloaded_bytes, total_bytes) = normalize_torrent_progress_update(dest_dir, downloaded)?;
    Some(match total_bytes {
        Some(total_bytes) => format!("{}/{}", format_size(downloaded_bytes), format_size(total_bytes)),
        None => format_size(downloaded_bytes),
    })
}

fn write_torrent_progress(dest_dir: &std::path::Path, downloaded: &str) {
    if let Some((downloaded_bytes, total_bytes)) =
        normalize_torrent_progress_update(dest_dir, downloaded)
    {
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
    len: u64,
}

#[derive(Clone)]
struct TorrentFileProgressEntry {
    display_path: String,
    downloaded: u64,
    total: u64,
}

#[derive(Deserialize)]
struct SavedTorrentFileProgressEntry {
    path: String,
    downloaded: u64,
    total: u64,
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
            len: length,
        }]);
    }

    let files = bdict_get(info, b"files").and_then(bvalue_list)?;
    let mut entries = Vec::new();

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
            len: length,
        });
    }

    Some(entries)
}

fn torrent_file_allocated_bytes(path: &std::path::Path) -> u64 {
    let meta = match path.metadata() {
        Ok(meta) => meta,
        Err(_) => return 0,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.blocks().saturating_mul(512)
    }
    #[cfg(not(unix))]
    {
        meta.len()
    }
}

fn saved_torrent_file_progress_entries(
    cache_dir: &std::path::Path,
) -> Option<Vec<TorrentFileProgressEntry>> {
    let content = fs::read_to_string(cache_dir.join(".torrent_file_progress.json")).ok()?;
    let mut entries: Vec<TorrentFileProgressEntry> = serde_json::from_str::<
        Vec<SavedTorrentFileProgressEntry>,
    >(&content)
    .ok()?
    .into_iter()
    .map(|entry| TorrentFileProgressEntry {
        display_path: entry.path,
        downloaded: entry.downloaded.min(entry.total),
        total: entry.total,
    })
    .collect();
    entries.sort_by(|a, b| {
        b.total
            .cmp(&a.total)
            .then_with(|| a.display_path.cmp(&b.display_path))
    });
    Some(entries)
}

fn saved_torrent_file_progress_totals(cache_dir: &std::path::Path) -> Option<(u64, u64)> {
    let entries = saved_torrent_file_progress_entries(cache_dir)?;
    let downloaded = entries.iter().map(|entry| entry.downloaded).sum();
    let total = entries.iter().map(|entry| entry.total).sum();
    Some((downloaded, total))
}

fn torrent_file_progress_entries(cache_dir: &std::path::Path) -> Option<Vec<TorrentFileProgressEntry>> {
    if let Some(entries) = saved_torrent_file_progress_entries(cache_dir) {
        return Some(entries);
    }
    let torrent_bytes = fs::read(cache_dir.join("movie.torrent")).ok()?;
    let root = parse_bencode(&torrent_bytes).and_then(|value| match value {
        BValue::Dict(dict) => Some(dict),
        _ => None,
    })?;
    let info = bdict_get(&root, b"info").and_then(bvalue_dict)?;
    let files = torrent_files_from_info(cache_dir, info)?;
    let mut entries = Vec::new();

    for entry in files {
        let downloaded = torrent_file_allocated_bytes(&entry.path).min(entry.len);
        let display_path = entry
            .path
            .strip_prefix(cache_dir)
            .unwrap_or(&entry.path)
            .to_string_lossy()
            .into_owned();
        entries.push(TorrentFileProgressEntry {
            display_path,
            downloaded,
            total: entry.len,
        });
    }

    entries.sort_by(|a, b| {
        b.total
            .cmp(&a.total)
            .then_with(|| a.display_path.cmp(&b.display_path))
    });

    Some(entries)
}

fn render_torrent_file_progress_dropdown(ui: &mut egui::Ui, cache_dir: &std::path::Path) {
    let Some(entries) = torrent_file_progress_entries(cache_dir) else {
        return;
    };
    if entries.is_empty() {
        return;
    }

    egui::CollapsingHeader::new("Files in Torrent")
        .id_salt(cache_dir.display().to_string())
        .show(ui, |ui| {
            for entry in entries {
                let pct = if entry.total > 0 {
                    (entry.downloaded as f64 / entry.total as f64) * 100.0
                } else {
                    0.0
                };
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new(&entry.display_path).monospace());
                    ui.label(
                        egui::RichText::new(format!(
                            "{} / {} ({:.2}%)",
                            format_size(entry.downloaded),
                            format_size(entry.total),
                            pct
                        ))
                        .weak(),
                    );
                });
            }
        });
}

fn mpv_script_path() -> Option<String> {
    let path = app_resource_path("scripts/osc.lua");
    path.to_str().map(|s| s.to_string())
}

fn progress_file_path(dest_dir: &std::path::Path) -> PathBuf {
    if dest_dir.is_absolute() {
        dest_dir.join(".torrent_progress.json")
    } else {
        app_base_dir().join(dest_dir).join(".torrent_progress.json")
    }
}

fn progress_file_quality_score(progress_file: &std::path::Path) -> u64 {
    fs::read_to_string(progress_file)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .map(|json| {
            let contiguous = json
                .get("contiguous_prefix_bytes")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let downloaded = json
                .get("downloaded_bytes")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let ranges_bonus = json
                .get("ranges")
                .and_then(|v| v.as_array())
                .map(|ranges| if ranges.is_empty() { 0 } else { 1 })
                .unwrap_or(0);
            contiguous
                .saturating_mul(4)
                .saturating_add(downloaded)
                .saturating_add(ranges_bonus)
        })
        .unwrap_or(0)
}

fn dir_contains_matching_media_file(
    dir: &std::path::Path,
    target_name: &std::ffi::OsStr,
    target_size: u64,
) -> bool {
    fn visit(dir: &std::path::Path, target_name: &std::ffi::OsStr, target_size: u64) -> bool {
        let Ok(entries) = fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if visit(&path, target_name, target_size) {
                    return true;
                }
                continue;
            }
            if !path.is_file() || path.file_name() != Some(target_name) {
                continue;
            }
            if path.metadata().map(|meta| meta.len()).ok() == Some(target_size) {
                return true;
            }
        }
        false
    }

    visit(dir, target_name, target_size)
}

fn resolve_playback_progress_file(
    cache_dir: &std::path::Path,
    media_path: &std::path::Path,
) -> PathBuf {
    let direct_progress = progress_file_path(cache_dir);
    let media_parent_progress = media_path
        .parent()
        .map(progress_file_path)
        .filter(|path| path.exists());
    if let Some(path) = media_parent_progress {
        return path;
    }

    let target_name = match media_path.file_name() {
        Some(name) => name,
        None => return direct_progress,
    };
    let target_size = match fs::metadata(media_path) {
        Ok(meta) => meta.len(),
        Err(_) => return direct_progress,
    };

    let mut best_path = if direct_progress.exists() {
        Some(direct_progress.clone())
    } else {
        None
    };
    let mut best_score = best_path
        .as_ref()
        .map(|path| progress_file_quality_score(path))
        .unwrap_or(0);

    if let Ok(entries) = fs::read_dir(cache_dir) {
        for entry in entries.flatten() {
            let child_path = entry.path();
            if !child_path.is_dir() {
                continue;
            }
            let child_progress = progress_file_path(&child_path);
            if !child_progress.exists() {
                continue;
            }
            if !dir_contains_matching_media_file(&child_path, target_name, target_size) {
                continue;
            }
            let child_score = progress_file_quality_score(&child_progress);
            if child_score > best_score {
                best_score = child_score;
                best_path = Some(child_progress);
            }
        }
    }

    best_path.unwrap_or(direct_progress)
}

fn python_tool_command(script_relative_path: &str) -> Command {
    let app_base = app_base_dir();
    let venv_python = app_base.join(".venv").join("bin").join("python");
    let home_uv = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".local").join("bin").join("uv"));

    if venv_python.exists() {
        let mut cmd = Command::new(venv_python);
        cmd.arg(app_base.join(script_relative_path));
        cmd
    } else if let Some(home_uv) = home_uv.filter(|path| path.exists()) {
        let mut cmd = Command::new(home_uv);
        cmd.env("UV_CACHE_DIR", "/data/.cache/uv");
        cmd.args(["run", "python"]);
        cmd.arg(app_base.join(script_relative_path));
        cmd
    } else {
        let mut cmd = Command::new("uv");
        cmd.env("UV_CACHE_DIR", "/data/.cache/uv");
        cmd.args(["run", "python"]);
        cmd.arg(app_base.join(script_relative_path));
        cmd
    }
}

fn wait_for_local_http_proxy(port: u16, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

fn ensure_complete_progress_snapshot_for_existing_media(
    media_path: &std::path::Path,
    progress_file: &std::path::Path,
) {
    if progress_file.exists() {
        return;
    }
    let total_bytes = match fs::metadata(media_path) {
        Ok(metadata) => metadata.len(),
        Err(_) => return,
    };
    if total_bytes == 0 {
        return;
    }
    if let Some(parent) = progress_file.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let progress_json = serde_json::json!({
        "downloaded_bytes": total_bytes,
        "total_bytes": total_bytes,
        "contiguous_prefix_bytes": total_bytes,
        "piece_verified_bytes": total_bytes,
        "startup_window_bytes": 0,
        "startup_window_downloaded_bytes": 0,
        "startup_window_complete": true,
        "complete": true,
        "range_source": "legacy_local_file_fallback",
        "ranges": [[0, total_bytes]],
    });
    if let Ok(content) = serde_json::to_string(&progress_json) {
        let _ = fs::write(progress_file, content);
    }
}

fn direct_mpv_args(progress_file: &std::path::Path) -> Vec<String> {
    let mut args = vec![
        "--load-scripts=no".to_string(),
        "--osc=no".to_string(),
        "--ytdl=no".to_string(),
        "--force-window=immediate".to_string(),
        "--no-resume-playback".to_string(),
        format!(
            "--script-opts=osc-layout=bottombar,osc-seekbarstyle=bar,osc-seekrangestyle=none,osc-torrent_progress_file={}",
            progress_file.to_string_lossy()
        ),
    ];

    if let Some(script) = mpv_script_path() {
        args.push(format!("--script={}", script));
    }

    args.push("--slang=en,eng,english".to_string());

    args
}

fn proxy_stream_url(proxy_port: u16, media_path: &std::path::Path) -> String {
    let route = media_path
        .extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
        .map(|ext| format!("contiguous-stream.{}", ext.to_ascii_lowercase()))
        .unwrap_or_else(|| "contiguous-stream".to_string());
    format!("http://127.0.0.1:{}/{}", proxy_port, route)
}

fn launch_mpv(
    spawned_children: &Arc<Mutex<Vec<std::process::Child>>>,
    media_path: &std::path::Path,
    progress_file: &std::path::Path,
) -> Result<(), String> {
    ensure_complete_progress_snapshot_for_existing_media(media_path, progress_file);
    let proxy_port = get_free_port().ok_or_else(|| "failed to allocate local proxy port".to_string())?;
    let mut proxy_cmd = python_tool_command("tools/torrent_stream_proxy.py");
    proxy_cmd
        .arg("--media-path")
        .arg(media_path)
        .arg("--progress-file")
        .arg(progress_file)
        .arg("--port")
        .arg(proxy_port.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut proxy_child = proxy_cmd.spawn().map_err(|err| err.to_string())?;
    if !wait_for_local_http_proxy(proxy_port, Duration::from_secs(2)) {
        let _ = proxy_child.kill();
        let _ = proxy_child.wait();
        return Err(format!(
            "local stream proxy did not start on 127.0.0.1:{}",
            proxy_port
        ));
    }

    let mut cmd = Command::new("mpv");
    cmd.args(direct_mpv_args(progress_file));
    cmd.arg(proxy_stream_url(proxy_port, media_path));
    match cmd.spawn() {
        Ok(mut child) => {
            thread::sleep(Duration::from_millis(500));
            match child.try_wait() {
                Ok(Some(status)) => {
                    let _ = proxy_child.kill();
                    let _ = proxy_child.wait();
                    Err(format!("mpv exited immediately with status {}", status))
                }
                Ok(None) => {
                    let mut children = spawned_children.lock().unwrap();
                    children.push(proxy_child);
                    children.push(child);
                    Ok(())
                }
                Err(err) => {
                    let _ = proxy_child.kill();
                    let _ = proxy_child.wait();
                    Err(format!("failed to poll mpv startup: {}", err))
                }
            }
        }
        Err(err) => {
            let _ = proxy_child.kill();
            let _ = proxy_child.wait();
            Err(err.to_string())
        }
    }
}

fn default_found_count() -> u32 {
    1
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct YtsTorrent {
    url: String,
    #[serde(default)]
    source_url: String,
    hash: String,
    quality: String,
    size: String,
    seeds: Option<u32>,
    peers: Option<u32>,
    #[serde(default = "default_found_count")]
    found_count: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct YtsMovie {
    title_long: Option<String>,
    title: String,
    year: Option<u16>,
    rating: Option<f32>,
    genres: Option<Vec<String>>,
    runtime: Option<u32>,
    #[serde(default, alias = "date_uploaded")]
    uploaded_at: Option<String>,
    #[serde(default)]
    media_kind: MediaKind,
    torrents: Vec<YtsTorrent>,
}

#[derive(Deserialize, Debug)]
struct MirrorSearchReportEntry {
    domain: String,
    effective_domain: String,
    reason: String,
}

#[derive(Deserialize, Debug)]
struct MirrorSearchReport {
    #[serde(default)]
    api_successful: Vec<MirrorSearchReportEntry>,
    #[serde(default)]
    successful: Vec<MirrorSearchReportEntry>,
    #[serde(default)]
    failed: Vec<MirrorSearchReportEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SearchSource {
    Yify,
    X1337,
    SolidTorrents,
    ExtTo,
}

#[derive(Clone, Default)]
struct SearchStateSnapshot {
    query: String,
    results: Vec<YtsMovie>,
    page: usize,
    total_pages: Option<usize>,
    status: String,
}

impl SearchSource {
    fn source_label(self) -> &'static str {
        match self {
            SearchSource::Yify => "YIFY",
            SearchSource::X1337 => "1337x",
            SearchSource::SolidTorrents => "SolidTorrents",
            SearchSource::ExtTo => "ext",
        }
    }

    fn search_button_label(self) -> &'static str {
        match self {
            SearchSource::Yify => "🔍 Search YIFY Database",
            SearchSource::X1337 => "🔍 Search 1337x Database",
            SearchSource::SolidTorrents => "🔍 Search SolidTorrents API",
            SearchSource::ExtTo => "🔍 Search ext Mirrors",
        }
    }

    fn heading(self) -> &'static str {
        match self {
            SearchSource::Yify => "🔍 Search YIFY / YTS Database",
            SearchSource::X1337 => "🔍 Search 1337x Database",
            SearchSource::SolidTorrents => "🔍 Search SolidTorrents API",
            SearchSource::ExtTo => "🔍 Search ext Mirrors",
        }
    }

    fn description(self) -> &'static str {
        match self {
            SearchSource::Yify => "Search online for torrents across verified YIFY/YTS mirrors, or select a cached library on the left.",
            SearchSource::X1337 => "Search online for torrents across verified 1337x mirrors, or select a cached library on the left.",
            SearchSource::SolidTorrents => "Search online for torrents via SolidTorrents API, querying the global DHT network with no page blocks.",
            SearchSource::ExtTo => "Search online for torrents across verified ext mirrors, or select a cached library on the left.",
        }
    }

    fn api_mirror_file(self) -> Option<&'static str> {
        match self {
            SearchSource::Yify => Some("yify_api_mirrors.txt"),
            SearchSource::X1337 => None,
            SearchSource::SolidTorrents => Some("solidtorrents_api_mirrors.txt"),
            SearchSource::ExtTo => Some("ext_api_mirrors.txt"),
        }
    }

    fn html_mirror_file(self) -> &'static str {
        match self {
            SearchSource::Yify => "yify_mirrors.txt",
            SearchSource::X1337 => "1337x_mirrors.txt",
            SearchSource::SolidTorrents => "solidtorrents_mirrors.txt",
            SearchSource::ExtTo => "ext_mirrors.txt",
        }
    }

    fn report_file(self) -> &'static str {
        match self {
            SearchSource::Yify => "yify_search_report.json",
            SearchSource::X1337 => "1337x_search_report.json",
            SearchSource::SolidTorrents => "solidtorrents_search_report.json",
            SearchSource::ExtTo => "ext_search_report.json",
        }
    }

    fn supports_pagination(self) -> bool {
        matches!(self, SearchSource::Yify | SearchSource::SolidTorrents)
    }

    fn page_size(self) -> usize {
        match self {
            SearchSource::Yify => 50,
            SearchSource::SolidTorrents => 100,
            SearchSource::X1337 | SearchSource::ExtTo => 100,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppTab {
    Library,
    Search,
    Settings,
}

fn push_unique_mirror(mirrors: &mut Vec<String>, line: &str) {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return;
    }
    if !mirrors.iter().any(|existing| existing == line) {
        mirrors.push(line.to_string());
    }
}

fn load_search_mirrors(source: SearchSource) -> Vec<String> {
    let mut mirrors = Vec::new();
    if let Some(path) = source.api_mirror_file() {
        if let Ok(content) = fs::read_to_string(app_resource_path(path)) {
            for line in content.lines() {
                push_unique_mirror(&mut mirrors, line);
            }
        }
    }

    if let Ok(content) = fs::read_to_string(app_resource_path(source.html_mirror_file())) {
        for line in content.lines() {
            push_unique_mirror(&mut mirrors, line);
        }
    }
    if mirrors.len() > 1 {
        let mut other_mirrors = mirrors.split_off(1);
        other_mirrors.sort_by_key(|m| !m.starts_with("https://"));
        mirrors.extend(other_mirrors);
    }
    mirrors
}

fn load_diagnostic_recheck_mirrors(source: SearchSource) -> Vec<(String, MirrorStatusSource)> {
    let mut mirrors = Vec::new();
    if let Some(path) = source.api_mirror_file() {
        if let Ok(content) = fs::read_to_string(app_resource_path(path)) {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if !mirrors.iter().any(|(existing, _)| existing == line) {
                    mirrors.push((line.to_string(), MirrorStatusSource::LiveRecheckApi));
                }
            }
        }
    }
    if let Ok(content) = fs::read_to_string(app_resource_path(source.html_mirror_file())) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if !mirrors.iter().any(|(existing, _)| existing == line) {
                mirrors.push((line.to_string(), MirrorStatusSource::LiveRecheckHtml));
            }
        }
    }
    mirrors
}

fn load_report_only_diagnostic_statuses(
    source: SearchSource,
    rechecked_mirrors: &[(String, MirrorStatusSource)],
) -> Vec<MirrorStatus> {
    let report_path = app_resource_path(source.report_file());
    let content = match fs::read_to_string(report_path) {
        Ok(content) => content,
        Err(_) => return Vec::new(),
    };

    let report: MirrorSearchReport = match serde_json::from_str(&content) {
        Ok(report) => report,
        Err(_) => return Vec::new(),
    };

    let should_skip = |url: &str| rechecked_mirrors.iter().any(|(mirror, _)| mirror == url);
    let mut statuses = Vec::new();

    for entry in report.api_successful {
        let url = if entry.effective_domain.trim().is_empty() {
            entry.domain.trim().to_string()
        } else {
            entry.effective_domain.trim().to_string()
        };
        if url.is_empty() || should_skip(&url) {
            continue;
        }
        statuses.push(MirrorStatus {
            url,
            status: "API Searchable (Report)".to_string(),
            detail: entry.reason,
            source: MirrorStatusSource::CachedReportApi,
        });
    }

    for entry in report.successful {
        let url = if entry.effective_domain.trim().is_empty() {
            entry.domain.trim().to_string()
        } else {
            entry.effective_domain.trim().to_string()
        };
        if url.is_empty() || should_skip(&url) {
            continue;
        }
        statuses.push(MirrorStatus {
            url,
            status: "Searchable (Report)".to_string(),
            detail: entry.reason,
            source: MirrorStatusSource::CachedReportSearch,
        });
    }

    for entry in report.failed {
        let url = if entry.effective_domain.trim().is_empty() {
            entry.domain.trim().to_string()
        } else {
            entry.effective_domain.trim().to_string()
        };
        if url.is_empty() {
            continue;
        }
        statuses.push(MirrorStatus {
            url,
            status: "Search Verification Failed".to_string(),
            detail: entry.reason,
            source: MirrorStatusSource::CachedReportSearch,
        });
    }

    statuses
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

fn scrape_yify_api_page(
    agent: &ureq::Agent,
    mirror: &str,
    query: &str,
    page: usize,
) -> (Vec<YtsMovie>, Option<usize>) {
    let encoded = percent_encode(query);
    let page = page.max(1);
    let limit = SearchSource::Yify.page_size();
    let url = format!(
        "{}/api/v2/list_movies.json?query_term={}&limit={}&page={}",
        mirror.trim_end_matches('/'),
        encoded,
        limit,
        page
    );
    let Ok(mut resp) = agent
        .get(&url)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
        )
        .call()
    else {
        return (Vec::new(), None);
    };
    let Ok(body) = resp.body_mut().read_to_string() else {
        return (Vec::new(), None);
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) else {
        return (Vec::new(), None);
    };
    if val["status"] != "ok" {
        return (Vec::new(), None);
    }

    let mut parsed_movies = Vec::new();
    if let Some(movies_arr) = val["data"]["movies"].as_array() {
        for m_val in movies_arr {
            if let Ok(mut movie) = serde_json::from_value::<YtsMovie>(m_val.clone()) {
                movie.uploaded_at = movie
                    .uploaded_at
                    .as_deref()
                    .and_then(compact_uploaded_date);
                parsed_movies.push(movie);
            }
        }
    }
    let total_pages = val["data"]["movie_count"]
        .as_u64()
        .map(|count| ((count as usize).saturating_add(limit - 1)) / limit)
        .map(|pages| pages.max(1));
    (parsed_movies, total_pages)
}

fn scrape_yts_html_fallback(
    agent: &ureq::Agent,
    mirror: &str,
    query: &str,
    page: usize,
) -> Vec<YtsMovie> {
    let mut movies = Vec::new();
    let encoded = percent_encode(query);
    let page = page.max(1);
    let url = if page == 1 {
        format!("{}/?keyword={}", mirror, encoded)
    } else {
        format!("{}/?keyword={}&page={}", mirror, encoded, page)
    };
    
    let res = agent.get(&url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .call();
    if let Ok(mut resp) = res {
        if let Ok(body) = resp.body_mut().read_to_string() {
            let mut parts: Vec<&str> = body.split("class=\"browse-movie-wrap").collect();
            if parts.len() > 1 {
                parts.remove(0); // remove prefix
                for part in parts {
                    // Extract details page URL
                    let details_url = if let Some(p1) = part.find("href=\"") {
                        let rest = &part[p1 + 6..];
                        if let Some(p2) = rest.find("\"") {
                            let u = rest[..p2].to_string();
                            if u.starts_with('/') {
                                Some(format!("{}{}", mirror, u))
                            } else {
                                Some(u)
                            }
                        } else { None }
                    } else { None };

                    // Extract title using robust class="browse-movie-title" check
                    let title = if let Some(p1) = part.find("class=\"browse-movie-title\"") {
                        if let Some(p_tag_end) = part[p1..].find('>') {
                            let text_start = p1 + p_tag_end + 1;
                            if let Some(p_text_end) = part[text_start..].find('<') {
                                Some(part[text_start..text_start + p_text_end].trim().to_string())
                            } else { None }
                        } else { None }
                    } else if let Some(p1) = part.find("title=\"") {
                        // Fallback but exclude generic attributes
                        let rest = &part[p1 + 7..];
                        if let Some(p2) = rest.find("\"") {
                            let val = rest[..p2].to_string();
                            if val != "Download" && val != "View details" && !val.is_empty() {
                                Some(val)
                            } else { None }
                        } else { None }
                    } else { None };
 
                    // Extract year
                    let year = if let Some(p1) = part.find("class=\"browse-movie-year\">") {
                        let rest = &part[p1 + 26..];
                        if let Some(p2) = rest.find("<") {
                            rest[..p2].trim().parse::<u16>().ok()
                        } else { None }
                    } else { None };
 
                    // Extract rating using robust clean text check
                    let rating = if let Some(p1) = part.find("class=\"rating\"") {
                        if let Some(p_tag_end) = part[p1..].find('>') {
                            let text = &part[p1 + p_tag_end + 1..];
                            let mut clean_text = String::new();
                            let mut in_tag = false;
                            for c in text.chars() {
                                if clean_text.contains("/ 10") {
                                    break;
                                }
                                if c == '<' {
                                    in_tag = true;
                                } else if c == '>' {
                                    in_tag = false;
                                } else if !in_tag {
                                    clean_text.push(c);
                                }
                            }
                            if let Some(slash_pos) = clean_text.find("/ 10") {
                                clean_text[..slash_pos].trim().parse::<f32>().ok()
                            } else { None }
                        } else { None }
                    } else { None };
 
                    if let (Some(url), Some(title_str)) = (details_url, title) {
                        let query_lower = query.to_lowercase();
                        let title_lower = title_str.to_lowercase();
                        let is_match = query_lower.split_whitespace().any(|word| {
                            if word.len() <= 2 && query_lower.len() > 3 {
                                false
                            } else {
                                title_lower.contains(word)
                            }
                        });
                        if !is_match {
                            continue;
                        }
 
                        let clean_title = if let Some(pos) = title_str.rfind(" (") {
                            title_str[..pos].to_string()
                        } else {
                            title_str.clone()
                        };
 
                        // Fetch the details page to resolve torrent qualities and hashes
                        let mut torrents = Vec::new();
                        let det_res = agent.get(&url)
                            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
                            .call();
                        if let Ok(mut det_resp) = det_res {
                            if let Ok(det_body) = det_resp.body_mut().read_to_string() {
                                if det_body.contains("class=\"modal-torrent\"") {
                                    let det_parts: Vec<&str> = det_body.split("class=\"modal-torrent\"").collect();
                                    for det_part in det_parts.iter().skip(1) {
                                        let quality = if let Some(q1) = det_part.find("id=\"modal-quality-") {
                                            let rest = &det_part[q1 + 18..];
                                            if let Some(q2) = rest.find("\"") {
                                                rest[..q2].to_string()
                                            } else { "720p".to_string() }
                                        } else { "720p".to_string() };
 
                                        let size = if let Some(s1) = det_part.find("class=\"quality-size\">") {
                                            let rest = &det_part[s1 + 21..];
                                            if let Some(s2) = rest.find("<") {
                                                rest[..s2].trim().to_string()
                                            } else { "Unknown".to_string() }
                                        } else { "Unknown".to_string() };
 
                                        // Extract info hash from magnet link or download link
                                        let hash = if let Some(m1) = det_part.find("magnet:?xt=urn:btih:") {
                                            let rest = &det_part[m1 + 20..];
                                            if let Some(m2) = rest.find("&") {
                                                rest[..m2].to_uppercase()
                                            } else {
                                                if let Some(m2) = rest.find("\"") {
                                                    rest[..m2].to_uppercase()
                                                } else {
                                                    String::new()
                                                }
                                            }
                                        } else if let Some(d1) = det_part.find("/torrent/download/") {
                                            let rest = &det_part[d1 + 18..];
                                            if let Some(d2) = rest.find("\"") {
                                                rest[..d2].to_uppercase()
                                            } else {
                                                String::new()
                                            }
                                        } else { String::new() };
 
                                        let clean_hash: String = hash.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
 
                                        if clean_hash.len() == 40 {
                                            torrents.push(YtsTorrent {
                                                url: format!("magnet:?xt=urn:btih:{}&dn={}", clean_hash, percent_encode(&clean_title)),
                                                source_url: url.clone(),
                                                hash: clean_hash,
                                                quality,
                                                size,
                                                seeds: Some(0),
                                                peers: Some(0),
                                                found_count: 1,
                                            });
                                        }
                                    }
                                } else {
                                    // Try AJAX JSON hits API fallback for unified proxy templates
                                    let yr_str = year.map(|y| y.to_string()).unwrap_or_default();
                                    let name_enc = percent_encode(&clean_title);
                                     // Parse base URL from details_url (e.g. "https://en.yts.lu")
                                     let details_base = if let Some(p) = url.find("://") {
                                         let rest = &url[p + 3..];
                                         if let Some(slash) = rest.find('/') {
                                             format!("{}://{}", &url[..p], &rest[..slash])
                                         } else {
                                             url.clone()
                                         }
                                     } else {
                                         mirror.to_string()
                                     };
                                     let api_url = format!("{}/?api=torrents&mode=movie&name={}&year={}&quality=all", details_base, name_enc, yr_str);
                                    
                                     let api_res = agent.get(&api_url)
                                         .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
                                         .header("X-Requested-With", "XMLHttpRequest")
                                         .call();
                                    
                                    if let Ok(mut api_resp) = api_res {
                                        if let Ok(api_body) = api_resp.body_mut().read_to_string() {
                                            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&api_body) {
                                                if let Some(hits) = val["hits"].as_array() {
                                                    for hit in hits {
                                                        let title_str = hit["title"].as_str().unwrap_or("");
                                                        let magnet = hit["magnetUrl"].as_str().unwrap_or("");
                                                        let hash = hit["hash"].as_str().unwrap_or("");
                                                        let seeds = hit["seeds"].as_u64().unwrap_or(0) as u32;
                                                        let peers = hit["peers"].as_u64().unwrap_or(0) as u32;
                                                        let bytes = hit["bytes"].as_u64().unwrap_or(0);
 
                                                        let clean_hash: String = hash.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
                                                        if clean_hash.len() == 40 && !magnet.is_empty() {
                                                            let quality = if title_str.contains("2160p") || title_str.contains("4K") {
                                                                "2160p".to_string()
                                                            } else if title_str.contains("1080p") {
                                                                "1080p".to_string()
                                                            } else if title_str.contains("720p") {
                                                                "720p".to_string()
                                                            } else {
                                                                "1080p".to_string()
                                                            };
 
                                                            let size = if bytes > 0 {
                                                                format_size(bytes)
                                                            } else {
                                                                "Unknown".to_string()
                                                            };
 
                                                            torrents.push(YtsTorrent {
                                                                url: magnet.to_string(),
                                                                source_url: api_url.clone(),
                                                                hash: clean_hash,
                                                                quality,
                                                                size,
                                                                seeds: Some(seeds),
                                                                peers: Some(peers),
                                                                found_count: 1,
                                                            });
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        if !torrents.is_empty() {
                            let media_kind =
                                classify_search_media_kind_with_torrents(
                                    &title_str,
                                    None,
                                    None,
                                    &torrents,
                                );
                            movies.push(YtsMovie {
                                title_long: Some(title_str),
                                title: clean_title,
                                year,
                                rating,
                                genres: None,
                                runtime: None,
                                uploaded_at: None,
                                media_kind,
                                torrents,
                            });
                        }
                    }
                }
            }
        }
    }
    movies
}

fn strip_html_tags(text: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&nbsp;", " ")
        .trim()
        .to_string()
}

fn quality_from_title(title: &str) -> String {
    let lower = title.to_lowercase();
    if lower.contains("2160p") || lower.contains("4k") {
        "2160p".to_string()
    } else if lower.contains("1080p") {
        "1080p".to_string()
    } else if lower.contains("720p") {
        "720p".to_string()
    } else if lower.contains("480p") {
        "480p".to_string()
    } else {
        "Unknown".to_string()
    }
}

fn year_from_title(title: &str) -> Option<u16> {
    for token in title.split(|c: char| !c.is_ascii_digit()) {
        if token.len() == 4 {
            if let Ok(year) = token.parse::<u16>() {
                if (1900..=2100).contains(&year) {
                    return Some(year);
                }
            }
        }
    }
    None
}

fn normalize_release_title(title: &str) -> String {
    let mut out = strip_html_tags(title)
        .replace('.', " ")
        .replace('_', " ");
    for marker in [" 2160p", " 1080p", " 720p", " 480p", " x264", " x265", " h264", " h265", " web-dl", " webrip", " bluray"] {
        if let Some(pos) = out.to_lowercase().find(marker) {
            out = out[..pos].trim().to_string();
            break;
        }
    }
    out.trim_matches(|c: char| c == '-' || c.is_whitespace())
        .to_string()
}

fn query_matches_title(query: &str, title: &str) -> bool {
    let title_lower = title.to_lowercase();
    query.to_lowercase()
        .split_whitespace()
        .filter(|word| word.len() > 1)
        .all(|word| title_lower.contains(word))
}

fn solidtorrents_size_string(value: &serde_json::Value) -> String {
    if let Some(bytes) = value.as_u64() {
        return format_size(bytes);
    }
    if let Some(size) = value.as_str() {
        return size.to_string();
    }
    "Unknown".to_string()
}

fn compact_uploaded_date(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.len() >= 10 {
        Some(trimmed[..10].to_string())
    } else if !trimmed.is_empty() {
        Some(trimmed.to_string())
    } else {
        None
    }
}

fn month_number_from_name(value: &str) -> Option<u32> {
    match value.trim_matches('.').to_ascii_lowercase().as_str() {
        "jan" | "january" => Some(1),
        "feb" | "february" => Some(2),
        "mar" | "march" => Some(3),
        "apr" | "april" => Some(4),
        "may" => Some(5),
        "jun" | "june" => Some(6),
        "jul" | "july" => Some(7),
        "aug" | "august" => Some(8),
        "sep" | "sept" | "september" => Some(9),
        "oct" | "october" => Some(10),
        "nov" | "november" => Some(11),
        "dec" | "december" => Some(12),
        _ => None,
    }
}

fn normalize_1337x_uploaded_date(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if matches!(lower.as_str(), "today" | "y-day" | "yesterday") {
        return Some(trimmed.to_string());
    }
    if trimmed.len() >= 10
        && trimmed.as_bytes().get(4) == Some(&b'-')
        && trimmed.as_bytes().get(7) == Some(&b'-')
    {
        return Some(trimmed[..10].to_string());
    }

    let cleaned = trimmed.replace(',', " ");
    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    if tokens.len() < 3 {
        return None;
    }

    let month = month_number_from_name(tokens[0])?;
    let day_digits: String = tokens[1]
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect();
    let year_digits: String = tokens[2]
        .trim_start_matches('\'')
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect();

    let day = day_digits.parse::<u32>().ok()?;
    let year = match year_digits.len() {
        4 => year_digits.parse::<u32>().ok()?,
        2 => 2000 + year_digits.parse::<u32>().ok()?,
        _ => return Some(trimmed.to_string()),
    };

    Some(format!("{year:04}-{month:02}-{day:02}"))
}

fn extract_1337x_uploaded_date(row: &str) -> Option<String> {
    extract_text_from_row_class(row, "coll-4")
        .or_else(|| extract_text_from_row_class(row, "coll-5"))
        .or_else(|| extract_text_from_row_class(row, "time"))
        .and_then(|text| normalize_1337x_uploaded_date(&text))
        .or_else(|| {
            row.split("<td")
                .skip(1)
                .filter_map(|cell| {
                    let cell_end = cell.find("</td>")?;
                    let start = cell.find('>')? + 1;
                    let text = strip_html_tags(&cell[start..cell_end]);
                    normalize_1337x_uploaded_date(text.trim())
                })
                .next()
        })
}

fn scrape_solidtorrents_api_page(
    agent: &ureq::Agent,
    mirror: &str,
    query: &str,
    page: usize,
    sort_by_date: bool,
) -> (Vec<YtsMovie>, Option<usize>) {
    let encoded = percent_encode(query);
    let page = page.max(1);
    let per_page = SearchSource::SolidTorrents.page_size();
    let mut url = format!(
        "{}/api/v1/search?q={}&limit={}&page={}",
        mirror.trim_end_matches('/'),
        encoded,
        per_page,
        page
    );
    if sort_by_date {
        url.push_str("&sort=date");
    }
    let res = agent
        .get(&url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .call();

    let Ok(mut resp) = res else {
        return (Vec::new(), None);
    };
    let Ok(body) = resp.body_mut().read_to_string() else {
        return (Vec::new(), None);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) else {
        return (Vec::new(), None);
    };
    if value["success"] != true {
        return (Vec::new(), None);
    }

    let mut movies = Vec::new();
    if let Some(results) = value["results"].as_array() {
        for item in results {
            let title = item["title"].as_str().unwrap_or("").trim();
            let info_hash = item["infohash"]
                .as_str()
                .or_else(|| item["infoHash"].as_str())
                .unwrap_or("")
                .trim();
            let clean_hash: String = info_hash
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_uppercase();
            if title.is_empty() || clean_hash.len() != 40 || !query_matches_title(query, title) {
                continue;
            }

            let display_title = normalize_release_title(title);
            let movie_title = if display_title.is_empty() {
                title.to_string()
            } else {
                display_title
            };
            let year = year_from_title(title);
            let seeds = item["seeders"]
                .as_u64()
                .or_else(|| item["seeds"].as_u64())
                .map(|v| v as u32);
            let peers = item["leechers"]
                .as_u64()
                .or_else(|| item["peers"].as_u64())
                .map(|v| v as u32);
            let size = solidtorrents_size_string(&item["size"]);
            let uploaded_at = item["createdAt"]
                .as_str()
                .and_then(compact_uploaded_date);

            let movie_torrents = vec![YtsTorrent {
                url: make_magnet_link(&clean_hash, title),
                source_url: url.clone(),
                hash: clean_hash,
                quality: quality_from_title(title),
                size,
                seeds,
                peers,
                found_count: 1,
            }];
            let media_kind =
                classify_search_media_kind_with_torrents(title, None, None, &movie_torrents);
            movies.push(YtsMovie {
                title_long: Some(title.to_string()),
                title: movie_title,
                year,
                rating: None,
                genres: None,
                runtime: None,
                uploaded_at,
                media_kind,
                torrents: movie_torrents,
            });
        }
    }
    let total_pages = value["totalPages"]
        .as_u64()
        .map(|v| v as usize)
        .or_else(|| {
            let total = value["total"].as_u64()? as usize;
            let per_page = value["perPage"]
                .as_u64()
                .map(|v| v as usize)
                .unwrap_or(per_page)
                .max(1);
            Some((total.saturating_add(per_page - 1)) / per_page)
        })
        .map(|pages| pages.max(1));
    (movies, total_pages)
}

fn extract_first_href_with_markers(section: &str, markers: &[&str]) -> Option<String> {
    for quote in ['"', '\''] {
        let needle = format!("href={}", quote);
        for fragment in section.split(&needle).skip(1) {
            let end = fragment.find(quote)?;
            let href = &fragment[..end];
            if markers.iter().any(|marker| href.to_lowercase().contains(marker)) {
                return Some(href.to_string());
            }
        }
    }
    None
}

fn extract_first_size_like_text(section: &str) -> Option<String> {
    let text = strip_html_tags(section);
    let tokens: Vec<&str> = text.split_whitespace().collect();
    for idx in 0..tokens.len() {
        let token = tokens[idx].trim_matches(|c: char| c == ',' || c == ')' || c == '(');
        if matches!(token.to_ascii_uppercase().as_str(), "KB" | "MB" | "GB" | "TB") && idx > 0 {
            let previous = tokens[idx - 1].trim_matches(|c: char| c == ',' || c == ')' || c == '(');
            if previous.chars().any(|c| c.is_ascii_digit()) {
                return Some(format!("{} {}", previous, token));
            }
        }
        let upper = token.to_ascii_uppercase();
        if upper.ends_with("KB") || upper.ends_with("MB") || upper.ends_with("GB") || upper.ends_with("TB") {
            return Some(token.to_string());
        }
    }
    None
}

fn extract_magnet_and_hash_from_html(html: &str) -> Option<(String, String)> {
    let magnet_pos = html.find("magnet:?xt=urn:btih:")?;
    let rest = &html[magnet_pos..];
    let end = rest.find('"').or_else(|| rest.find('\'')).unwrap_or(rest.len());
    let magnet = rest[..end].replace("&amp;", "&");
    let hash_start = "magnet:?xt=urn:btih:".len();
    if rest.len() <= hash_start {
        return None;
    }
    let hash_rest = &rest[hash_start..];
    let hash_end = hash_rest
        .find('&')
        .or_else(|| hash_rest.find('"'))
        .or_else(|| hash_rest.find('\''))
        .unwrap_or(hash_rest.len());
    let clean_hash = hash_rest[..hash_end]
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_uppercase();
    if clean_hash.len() == 40 {
        Some((magnet, clean_hash))
    } else {
        None
    }
}

fn scrape_ext_api(agent: &ureq::Agent, mirror: &str, query: &str) -> Vec<YtsMovie> {
    let encoded = percent_encode(query);
    let candidate_urls = [
        format!("{}/api/v1/search?q={}&limit=100", mirror.trim_end_matches('/'), encoded),
        format!("{}/api/search?q={}&limit=100", mirror.trim_end_matches('/'), encoded),
        format!("{}/api/torrents/search?q={}&limit=100", mirror.trim_end_matches('/'), encoded),
    ];

    for url in candidate_urls {
        let Ok(mut resp) = agent
            .get(&url)
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
            .call()
        else {
            continue;
        };
        let Ok(body) = resp.body_mut().read_to_string() else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) else {
            continue;
        };

        let results = value["results"]
            .as_array()
            .or_else(|| value["items"].as_array())
            .or_else(|| value["data"]["results"].as_array())
            .or_else(|| value["data"]["items"].as_array());

        let Some(results) = results else {
            continue;
        };

        let mut movies = Vec::new();
        for item in results {
            let title = item["title"]
                .as_str()
                .or_else(|| item["name"].as_str())
                .or_else(|| item["filename"].as_str())
                .unwrap_or("")
                .trim();
            let info_hash = item["infohash"]
                .as_str()
                .or_else(|| item["infoHash"].as_str())
                .or_else(|| item["hash"].as_str())
                .unwrap_or("")
                .trim();
            let clean_hash: String = info_hash
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_uppercase();
            if title.is_empty() || clean_hash.len() != 40 || !query_matches_title(query, title) {
                continue;
            }
            let display_title = normalize_release_title(title);
            let movie_title = if display_title.is_empty() {
                title.to_string()
            } else {
                display_title
            };
            let seeds = item["seeders"]
                .as_u64()
                .or_else(|| item["seeds"].as_u64())
                .map(|v| v as u32);
            let peers = item["leechers"]
                .as_u64()
                .or_else(|| item["peers"].as_u64())
                .map(|v| v as u32);
            let size = solidtorrents_size_string(&item["size"]);

            let movie_torrents = vec![YtsTorrent {
                url: make_magnet_link(&clean_hash, title),
                source_url: url.clone(),
                hash: clean_hash,
                quality: quality_from_title(title),
                size,
                seeds,
                peers,
                found_count: 1,
            }];
            let media_kind =
                classify_search_media_kind_with_torrents(title, None, None, &movie_torrents);
            movies.push(YtsMovie {
                title_long: Some(title.to_string()),
                title: movie_title,
                year: year_from_title(title),
                rating: None,
                genres: None,
                runtime: None,
                uploaded_at: None,
                media_kind,
                torrents: movie_torrents,
            });
        }
        if !movies.is_empty() {
            return movies;
        }
    }

    Vec::new()
}

fn scrape_ext_html_fallback(agent: &ureq::Agent, mirror: &str, query: &str) -> Vec<YtsMovie> {
    let encoded = percent_encode(query);
    let url = format!("{}/browse/?q={}&with_adult=1", mirror.trim_end_matches('/'), encoded);
    let Ok(mut resp) = agent
        .get(&url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .call()
    else {
        return Vec::new();
    };
    let Ok(body) = resp.body_mut().read_to_string() else {
        return Vec::new();
    };
    let body_lower = body.to_lowercase();
    if body_lower.contains("cf-mitigated") || body_lower.contains("just a moment") {
        return Vec::new();
    }

    let mut movies = Vec::new();
    for row in body.split("<tr").skip(1) {
        let detail_href = extract_first_href_with_markers(
            row,
            &["/torrent/", "/view/", "/download/", "/browse/", "/detail/"],
        );
        let Some(detail_href) = detail_href else {
            continue;
        };
        let row_text = strip_html_tags(row);
        if row_text.is_empty() || !query_matches_title(query, &row_text) {
            continue;
        }

        let title = normalize_release_title(&row_text);
        if title.is_empty() {
            continue;
        }

        let detail_url = if detail_href.starts_with("http") {
            detail_href.clone()
        } else {
            format!("{}{}", mirror.trim_end_matches('/'), detail_href)
        };

        let seeds = extract_table_number(row, "seed")
            .or_else(|| extract_table_number(row, "seeds"))
            .or_else(|| extract_table_number(row, "coll-2"));
        let peers = extract_table_number(row, "leech")
            .or_else(|| extract_table_number(row, "leechers"))
            .or_else(|| extract_table_number(row, "coll-3"));
        let size = extract_first_size_like_text(row).unwrap_or_else(|| "Unknown".to_string());

        let mut magnet_and_hash = extract_magnet_and_hash_from_html(row);
        if magnet_and_hash.is_none() {
            if let Ok(mut detail_resp) = agent
                .get(&detail_url)
                .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
                .call()
            {
                if let Ok(detail_body) = detail_resp.body_mut().read_to_string() {
                    magnet_and_hash = extract_magnet_and_hash_from_html(&detail_body);
                }
            }
        }

        let Some((magnet, clean_hash)) = magnet_and_hash else {
            continue;
        };

        let movie_torrents = vec![YtsTorrent {
            url: magnet,
            source_url: detail_url,
            hash: clean_hash,
            quality: quality_from_title(&row_text),
            size,
            seeds,
            peers,
            found_count: 1,
        }];
        let media_kind = classify_search_media_kind_with_torrents(
            &row_text,
            None,
            None,
            &movie_torrents,
        );
        movies.push(YtsMovie {
            title_long: Some(row_text.clone()),
            title,
            year: year_from_title(&row_text),
            rating: None,
            genres: None,
            runtime: None,
            uploaded_at: None,
            media_kind,
            torrents: movie_torrents,
        });
    }
    movies
}

fn scrape_1337x_html_fallback(agent: &ureq::Agent, mirror: &str, query: &str) -> Vec<YtsMovie> {
    let encoded = percent_encode(query);
    let url = format!("{}/search/{}/1/", mirror.trim_end_matches('/'), encoded);
    let res = agent
        .get(&url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .call();

    let Ok(mut resp) = res else {
        return Vec::new();
    };
    let Ok(body) = resp.body_mut().read_to_string() else {
        return Vec::new();
    };

    let mut movies = Vec::new();
    for row in body.split("<tr").skip(1) {
        if !row.contains("/torrent/") {
            continue;
        }

        let Some(href_pos) = row.find("/torrent/") else {
            continue;
        };
        let href_rest = &row[href_pos..];
        let Some(href_end) = href_rest.find('"') else {
            continue;
        };
        let detail_url = format!("{}{}", mirror.trim_end_matches('/'), &href_rest[..href_end]);

        let title = if let Some(title_attr) = row.find("title=\"") {
            let rest = &row[title_attr + 7..];
            rest.find('"')
                .map(|end| strip_html_tags(&rest[..end]))
                .unwrap_or_default()
        } else if let Some(anchor_start) = row[href_pos..].find('>') {
            let rest = &row[href_pos + anchor_start + 1..];
            rest.find('<')
                .map(|end| strip_html_tags(&rest[..end]))
                .unwrap_or_default()
        } else {
            String::new()
        };
        if title.is_empty() || !query_matches_title(query, &title) {
            continue;
        }

        let seeds = extract_table_number(row, "coll-2");
        let peers = extract_table_number(row, "coll-3");
        let size = extract_size_from_row(row).unwrap_or_else(|| "Unknown".to_string());
        let uploaded_at = extract_1337x_uploaded_date(row);

        let mut magnet = String::new();
        let mut clean_hash = String::new();
        if let Ok(mut detail_resp) = agent
            .get(&detail_url)
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
            .call()
        {
            if let Ok(detail_body) = detail_resp.body_mut().read_to_string() {
                if let Some(magnet_pos) = detail_body.find("magnet:?xt=urn:btih:") {
                    let rest = &detail_body[magnet_pos..];
                    if let Some(end) = rest.find('"') {
                        magnet = rest[..end].replace("&amp;", "&");
                    }
                    let hash_start = "magnet:?xt=urn:btih:".len();
                    if rest.len() > hash_start {
                        let hash_rest = &rest[hash_start..];
                        let hash_end = hash_rest.find('&').or_else(|| hash_rest.find('"')).unwrap_or(hash_rest.len());
                        clean_hash = hash_rest[..hash_end]
                            .chars()
                            .filter(|c| c.is_ascii_alphanumeric())
                            .collect::<String>()
                            .to_uppercase();
                    }
                }
            }
        }
        if magnet.is_empty() || clean_hash.len() != 40 {
            continue;
        }

        let torrent = YtsTorrent {
            url: magnet,
            source_url: detail_url,
            hash: clean_hash,
            quality: quality_from_title(&title),
            size,
            seeds,
            peers,
            found_count: 1,
        };
        let movie_torrents = vec![torrent.clone()];
        let media_kind = classify_search_media_kind_with_torrents(
            &title,
            None,
            None,
            &movie_torrents,
        );
        movies.push(YtsMovie {
            title_long: Some(title.clone()),
            title: normalize_release_title(&title),
            year: year_from_title(&title),
            rating: None,
            genres: None,
            runtime: None,
            uploaded_at,
            media_kind,
            torrents: movie_torrents,
        });
    }
    movies
}

fn extract_table_number(row: &str, class_name: &str) -> Option<u32> {
    let needle = format!("class=\"{}\"", class_name);
    let pos = row.find(&needle)?;
    let rest = &row[pos..];
    let start = rest.find('>')? + 1;
    let end = rest[start..].find('<')?;
    strip_html_tags(&rest[start..start + end]).trim().parse::<u32>().ok()
}

fn extract_size_from_row(row: &str) -> Option<String> {
    let pos = row.find("coll-4").or_else(|| row.find("size"))?;
    let rest = &row[pos..];
    let start = rest.find('>')? + 1;
    let end = rest[start..].find('<')?;
    let text = strip_html_tags(&rest[start..start + end]);
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn extract_text_from_row_class(row: &str, class_name: &str) -> Option<String> {
    let needle = format!("class=\"{}\"", class_name);
    let pos = row.find(&needle)?;
    let rest = &row[pos..];
    let start = rest.find('>')? + 1;
    let end = rest[start..].find('<')?;
    let text = strip_html_tags(&rest[start..start + end]);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn merge_search_results(results: &mut Vec<YtsMovie>, new_movies: Vec<YtsMovie>) {
    for movie in new_movies {
        if let Some(existing_movie) = results
            .iter_mut()
            .find(|m| m.title == movie.title && m.year == movie.year)
        {
            existing_movie.media_kind =
                merge_media_kind(existing_movie.media_kind, movie.media_kind);
            if existing_movie.uploaded_at.is_none() {
                existing_movie.uploaded_at = movie.uploaded_at.clone();
            }
            for t in movie.torrents {
                if let Some(existing_t) = existing_movie
                    .torrents
                    .iter_mut()
                    .find(|ext_t| ext_t.hash.to_uppercase() == t.hash.to_uppercase())
                {
                    existing_t.found_count += 1;
                    if existing_t.seeds.is_none() {
                        existing_t.seeds = t.seeds;
                    }
                    if existing_t.peers.is_none() {
                        existing_t.peers = t.peers;
                    }
                } else {
                    let mut new_t = t.clone();
                    new_t.found_count = 1;
                    existing_movie.torrents.push(new_t);
                }
            }
        } else {
            let mut new_movie = movie.clone();
            for t in &mut new_movie.torrents {
                t.found_count = 1;
            }
            results.push(new_movie);
        }
    }
}

fn make_magnet_link(info_hash: &str, title: &str) -> String {
    let mut tracker_args = String::new();
    for t in preferred_trackers() {
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

fn get_live_downloaded_and_total_excluding(
    cache_dir: &std::path::Path,
    total_hint: Option<u64>,
    excluded_direct_children: &[String],
) -> Option<(u64, u64)> {
    if let Some((downloaded, total)) = saved_torrent_file_progress_totals(cache_dir) {
        return Some((downloaded, total));
    }
    let disk_used = get_payload_disk_space_excluding(cache_dir, excluded_direct_children);
    if let Some((downloaded, total)) = get_torrent_downloaded_and_total(cache_dir) {
        return Some((downloaded.max(disk_used).min(total), total));
    }
    total_hint.map(|total| (disk_used.min(total), total))
}

fn ensure_torrent_progress_snapshot(
    cache_dir: &std::path::Path,
    total_hint: Option<u64>,
) -> (u64, Option<u64>) {
    if let Some((downloaded, total)) = saved_torrent_file_progress_totals(cache_dir) {
        return (downloaded, Some(total));
    }
    let disk_used = get_payload_disk_space(cache_dir);
    let existing = get_torrent_downloaded_and_total(cache_dir);
    let downloaded = existing
        .map(|(downloaded, _)| downloaded)
        .unwrap_or(disk_used)
        .max(disk_used);
    let total = existing.map(|(_, total)| total).or(total_hint);

    if let Some(total) = total {
        let progress_path = progress_file_path(cache_dir);
        let mut progress_json = fs::read_to_string(&progress_path)
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        progress_json.insert(
            "downloaded_bytes".to_string(),
            serde_json::json!(downloaded.min(total)),
        );
        progress_json.insert("total_bytes".to_string(), serde_json::json!(total));
        if let Ok(content) = serde_json::to_string(&serde_json::Value::Object(progress_json)) {
            let _ = fs::write(progress_path, content);
        }
    }

    (downloaded, total)
}

fn initialize_torrent_runtime_state(
    status_map: &Arc<Mutex<TorrentStatusMap>>,
    status_key: &str,
    cache_dir: &std::path::Path,
    total_hint: Option<u64>,
    sequential_mode: bool,
) {
    let (downloaded, _) = ensure_torrent_progress_snapshot(cache_dir, total_hint);
    let mut map = status_map.lock().unwrap();
    map.insert(
        status_key.to_string(),
        PerTorrentStatus {
            active: true,
            speed: "Connecting...".to_string(),
            downloaded: format_size(downloaded),
            peers: "connecting".to_string(),
            detail: String::new(),
            mode: if sequential_mode {
                "sequential".to_string()
            } else {
                "normal".to_string()
            },
        },
    );
}

fn movie_runtime_label(metadata: Option<&MovieMetadata>) -> Option<&str> {
    metadata
        .and_then(|meta| meta.duration.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn group_sidebar_progress_text(group: &MovieGroup) -> String {
    let mut entries = Vec::new();

    for torrent in &group.torrents {
        let cache_dir_path = get_cache_dir().join(&torrent.dir_name);
        let torrent_options = torrent
            .metadata
            .as_ref()
            .and_then(|meta| meta.torrent_options.as_ref())
            .cloned();
        let option_cache_markers: Vec<(String, String, String)> = torrent_options
            .as_ref()
            .map(|options| {
                options
                    .iter()
                    .map(|opt| {
                        let hash = opt.hash.to_uppercase();
                        (
                            hash.clone(),
                            opt.quality.clone(),
                            torrent_option_cache_subdir(&hash),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        if torrent_options.as_ref().is_none_or(|options| options.is_empty())
            && let Some((downloaded, total)) = get_torrent_downloaded_and_total(&cache_dir_path)
        {
            let live_downloaded = downloaded
                .max(get_payload_disk_space(&cache_dir_path))
                .min(total);
            let pct = if total > 0 {
                (live_downloaded as f64 / total as f64) * 100.0
            } else {
                0.0
            };
            entries.push(format!(
                "{} / {} ({:.2}%)",
                format_size(live_downloaded),
                format_size(total),
                pct
            ));
        }

        if let Some(ref options) = torrent_options {
            for opt in options {
                    let hash = opt.hash.to_uppercase();
                    let size_hint = parse_size_to_bytes(&opt.size);
                    let allow_legacy_size_match = legacy_cache_size_best_matches_option(
                        &cache_dir_path.join(&opt.quality),
                        &hash,
                        &opt.quality,
                        size_hint,
                        options,
                    );
                    let option_path = torrent_option_cache_path(
                        &cache_dir_path,
                        &opt.quality,
                        &hash,
                        size_hint,
                        allow_legacy_size_match,
                    );
                    let uses_root_cache = option_path == cache_dir_path;
                    let excluded_subdirs = if uses_root_cache {
                        sibling_option_cache_subdirs(&option_cache_markers, &hash)
                    } else {
                        Vec::new()
                    };
                    let has_option_state = option_path.join(".torrent_progress.json").exists()
                        || option_path.join(".torrent_file_progress.json").exists()
                        || option_path.join("movie.torrent").exists()
                        || get_payload_disk_space_excluding(&option_path, &excluded_subdirs) > 0;
                    if !has_option_state {
                        continue;
                    }
                    if let Some((downloaded, total)) = get_live_downloaded_and_total_excluding(
                        &option_path,
                        parse_size_to_bytes(&opt.size),
                        &excluded_subdirs,
                    )
                    {
                        let pct = if total > 0 {
                            (downloaded as f64 / total as f64) * 100.0
                        } else {
                            0.0
                        };
                        entries.push(format!(
                            "{} / {} ({:.2}%)",
                            format_size(downloaded),
                            format_size(total),
                            pct
                        ));
                    }
                }
        }
    }

    entries.sort();
    entries.dedup();
    if !entries.is_empty() {
        return entries.join(" ");
    }

    let live_disk_used: u64 = group
        .torrents
        .iter()
        .map(|torrent| {
            let cache_dir_path = get_cache_dir().join(&torrent.dir_name);
            if torrent
                .metadata
                .as_ref()
                .and_then(|meta| meta.torrent_options.as_ref())
                .is_some_and(|options| !options.is_empty())
            {
                0
            } else {
                get_folder_disk_space(&cache_dir_path)
            }
        })
        .sum();
    format!("0 B ({})", format_size(live_disk_used))
}

fn has_local_cache_artifacts(cache_dir: &std::path::Path) -> bool {
    cache_dir.exists() && get_folder_disk_space(cache_dir) > 0
}

/// Returns a sorted, deduplicated list of language names found in cached subtitle files.
/// Empty vec means no subtitle files are present locally.
fn cached_subtitle_languages(cache_dir: &std::path::Path) -> Vec<String> {
    let mut langs: Vec<String> = Vec::new();

    fn collect(dir: &std::path::Path, langs: &mut Vec<String>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                        if matches!(ext.to_lowercase().as_str(), "srt" | "vtt") {
                            let stem = path
                                .file_stem()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string();
                            langs.push(guess_language_from_name(&stem));
                        }
                    }
                } else if path.is_dir() {
                    collect(&path, langs);
                }
            }
        }
    }
    collect(cache_dir, &mut langs);
    langs.sort();
    langs.dedup();
    langs
}

/// Returns a sorted, deduplicated list of language names found inside the .torrent file list.
/// Returns None if no .torrent file exists yet.
fn torrent_subtitle_languages(cache_dir: &std::path::Path) -> Option<Vec<String>> {
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

    let mut langs: Vec<String> = Vec::new();

    if let Some(files) = bdict_get(info, b"files").and_then(bvalue_list) {
        for f in files {
            if let BValue::Dict(f_dict) = f {
                if let Some(BValue::List(path_list)) = bdict_get(&f_dict, b"path") {
                    for p in path_list {
                        if let BValue::Bytes(p_bytes) = p {
                            if let Ok(p_str) = std::str::from_utf8(p_bytes) {
                                let p_lower = p_str.to_lowercase();
                                if p_lower.ends_with(".srt") || p_lower.ends_with(".vtt") {
                                    let stem = std::path::Path::new(p_str)
                                        .file_stem()
                                        .unwrap_or_default()
                                        .to_string_lossy()
                                        .to_string();
                                    langs.push(guess_language_from_name(&stem));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    langs.sort();
    langs.dedup();
    Some(langs)
}

/// Best-effort language name extraction from a subtitle filename stem.
/// e.g. "2_English" → "English", "SDH.eng.HI" → "English (HI)", "Latin American.spa" → "Spanish (Latin America)"
fn guess_language_from_name(stem: &str) -> String {
    let s = stem.to_lowercase();

    // Strip leading numeric prefix like "2_"
    let s = s.trim_start_matches(|c: char| c.is_ascii_digit() || c == '_');

    let hi = s.contains(".hi") || s.ends_with("hi") || s.contains("_hi") || s.contains("sdh");
    let forced = s.contains("forced");

    let base = if s.contains("english") || s.starts_with("eng") || s == "en" {
        "English"
    } else if s.contains("spanish") || s.contains("spa") || s.contains("esp") {
        if s.contains("latin") { "Spanish (Latin America)" } else { "Spanish" }
    } else if s.contains("french") || s.contains("fre") || s.contains("fra") {
        "French"
    } else if s.contains("german") || s.contains("ger") || s.contains("deu") {
        "German"
    } else if s.contains("portuguese") || s.contains("por") || s.contains("pt") {
        "Portuguese"
    } else if s.contains("italian") || s.contains("ita") {
        "Italian"
    } else if s.contains("dutch") || s.contains("nld") || s.contains("nl") {
        "Dutch"
    } else if s.contains("russian") || s.contains("rus") {
        "Russian"
    } else if s.contains("chinese") || s.contains("chi") || s.contains("zho") {
        "Chinese"
    } else if s.contains("japanese") || s.contains("jpn") {
        "Japanese"
    } else if s.contains("korean") || s.contains("kor") {
        "Korean"
    } else if s.contains("arabic") || s.contains("ara") {
        "Arabic"
    } else if s.contains("turkish") || s.contains("tur") {
        "Turkish"
    } else if s.contains("polish") || s.contains("pol") {
        "Polish"
    } else if s.contains("swedish") || s.contains("swe") {
        "Swedish"
    } else if s.contains("norwegian") || s.contains("nor") {
        "Norwegian"
    } else if s.contains("danish") || s.contains("dan") {
        "Danish"
    } else if s.contains("finnish") || s.contains("fin") {
        "Finnish"
    } else if s.contains("hindi") || s.contains("hin") {
        "Hindi"
    } else {
        // Fall back to the raw stem, capitalised
        return stem
            .split(|c: char| !c.is_alphanumeric())
            .filter(|p| !p.is_empty())
            .map(|p| {
                let mut ch = p.chars();
                match ch.next() {
                    None => String::new(),
                    Some(f) => f.to_uppercase().collect::<String>() + ch.as_str(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
    };

    let mut label = base.to_string();
    if forced { label.push_str(" (Forced)"); }
    if hi     { label.push_str(" (SDH)"); }
    label
}

fn format_subtitle_summary(langs: &[String]) -> String {
    if langs.is_empty() {
        return "None".to_string();
    }
    let has_english = langs.iter().any(|l| {
        let l_lower = l.to_lowercase();
        l_lower.contains("english") || l_lower.contains("eng") || l_lower == "en"
    });
    if has_english {
        let others = langs.len() - 1;
        if others > 0 {
            format!("English, +{} Others", others)
        } else {
            "English".to_string()
        }
    } else {
        let others = langs.len();
        if others > 0 {
            format!("No English, +{} Others", others)
        } else {
            "No English".to_string()
        }
    }
}

fn check_single_mirror_status(agent: &ureq::Agent, mirror: &str, search_source: SearchSource) -> MirrorStatus {
    let encoded = percent_encode("Apex");
    if search_source != SearchSource::X1337 {
        let url_json = match search_source {
            SearchSource::Yify => format!(
                "{}/api/v2/list_movies.json?query_term={}&limit=1",
                mirror.trim_end_matches('/'),
                encoded
            ),
            SearchSource::SolidTorrents => format!(
                "{}/api/v1/search?q={}&limit=1",
                mirror.trim_end_matches('/'),
                encoded
            ),
            SearchSource::ExtTo => format!(
                "{}/api/v1/search?q={}&limit=1",
                mirror.trim_end_matches('/'),
                encoded
            ),
            SearchSource::X1337 => String::new(),
        };
        let res_json = agent.get(&url_json).call();

        if let Ok(mut resp) = res_json {
            if let Ok(body) = resp.body_mut().read_to_string() {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) {
                    let api_ok = match search_source {
                        SearchSource::Yify => val["status"] == "ok" && val["data"]["movies"].as_array().is_some(),
                        SearchSource::SolidTorrents => val["success"] == true,
                        SearchSource::ExtTo => {
                            val["success"] == true
                                || val["results"].as_array().is_some()
                                || val["items"].as_array().is_some()
                                || val["data"]["results"].as_array().is_some()
                        }
                        SearchSource::X1337 => false,
                    };
                    if api_ok {
                        let detail = match search_source {
                            SearchSource::Yify => "Responds to list_movies API requests",
                            SearchSource::SolidTorrents => "Responds to SolidTorrents search API requests",
                            SearchSource::ExtTo => "Responds to ext search API requests",
                            SearchSource::X1337 => "Responds to API requests",
                        };
                        return MirrorStatus {
                            url: mirror.to_string(),
                            status: "Working (API)".to_string(),
                            detail: detail.to_string(),
                            source: MirrorStatusSource::LiveRecheckApi,
                        };
                    }
                }
            }
        }
    }

    let url_html = match search_source {
        SearchSource::Yify => format!("{}/?keyword={}", mirror.trim_end_matches('/'), encoded),
        SearchSource::X1337 => format!("{}/search/{}/1/", mirror.trim_end_matches('/'), encoded),
        SearchSource::SolidTorrents => format!("{}/search?q={}", mirror.trim_end_matches('/'), encoded),
        SearchSource::ExtTo => format!("{}/browse/?q={}&with_adult=1", mirror.trim_end_matches('/'), encoded),
    };
    let res_html = agent.get(&url_html).call();

    match res_html {
        Ok(mut resp) => {
            if let Ok(body) = resp.body_mut().read_to_string() {
                let body_lower = body.to_lowercase();
                
                // Check if page contains typical redirect/spam/parking keywords
                if body_lower.contains("primewire") 
                    || body_lower.contains("sflix") 
                    || body_lower.contains("expireddomains")
                    || body_lower.contains("domain sale")
                {
                    return MirrorStatus {
                        url: mirror.to_string(),
                        status: "Redirected".to_string(),
                        detail: "Redirected to spam/parking page".to_string(),
                        source: MirrorStatusSource::LiveRecheckHtml,
                    };
                }

                if body.contains("challenges.cloudflare.com") 
                    || body_lower.contains("just a moment") 
                    || body_lower.contains("cf-cookie") 
                {
                    return MirrorStatus {
                        url: mirror.to_string(),
                        status: "Cloudflare Block".to_string(),
                        detail: "Forces JavaScript browser verification".to_string(),
                        source: MirrorStatusSource::LiveRecheckHtml,
                    };
                }

                if body_lower.contains("view full site")
                    || body_lower.contains("continue to site")
                    || body_lower.contains("go to site")
                    || body_lower.contains("open full site")
                    || body_lower.contains("visit full site")
                {
                    return MirrorStatus {
                        url: mirror.to_string(),
                        status: "Redirected".to_string(),
                        detail: "Loads an intermediate gateway page instead of the actual YTS site".to_string(),
                        source: MirrorStatusSource::LiveRecheckHtml,
                    };
                }

                let has_search_results = body.contains("class=\"browse-movie-wrap")
                    || body.contains("class='browse-movie-wrap")
                    || body.contains("browse-movie-wrap");
                let has_yts_shell = body.contains("browse-movie-link")
                    || body.contains("browse-movie-title")
                    || body.contains("browse-movie-year")
                    || body.contains("movie-info")
                    || body.contains("yts.mx")
                    || body.contains("yts.lt")
                    || body.contains("yts.rs")
                    || body.contains("yify subtitles");

                let has_1337x_shell = body_lower.contains("/torrent/")
                    || body_lower.contains("search torrents")
                    || body_lower.contains("1337x");
                let has_solidtorrents_shell = body_lower.contains("solidtorrents")
                    || body_lower.contains("/api/v1/search")
                    || body_lower.contains("search torrents");
                let has_ext_shell = body_lower.contains("extratorrent")
                    || body_lower.contains("ext.to")
                    || body_lower.contains("/browse/?q=")
                    || body_lower.contains("with_adult=1");

                let search_ui_works = match search_source {
                    SearchSource::Yify => has_search_results || has_yts_shell,
                    SearchSource::X1337 => has_1337x_shell,
                    SearchSource::SolidTorrents => has_solidtorrents_shell,
                    SearchSource::ExtTo => has_ext_shell,
                };

                if search_ui_works {
                    return MirrorStatus {
                        url: mirror.to_string(),
                        status: "Working (HTML)".to_string(),
                        detail: "Search UI works (successfully scraped via HTML fallback)".to_string(),
                        source: MirrorStatusSource::LiveRecheckHtml,
                    };
                }

                return MirrorStatus {
                    url: mirror.to_string(),
                    status: "Broken/Empty".to_string(),
                    detail: format!("Loads but doesn't contain {} search elements", search_source.source_label()),
                    source: MirrorStatusSource::LiveRecheckHtml,
                };
            }
        }
        Err(e) => {
            let err_msg = e.to_string();
            let err_msg_lower = err_msg.to_lowercase();
            
            let (status, detail) = if err_msg_lower.contains("403") || err_msg_lower.contains("503") {
                ("Cloudflare Block".to_string(), format!("Access Challenged ({})", err_msg))
            } else if err_msg_lower.contains("dns") || err_msg_lower.contains("resolve") {
                ("DNS Failure".to_string(), "Failed to resolve domain name".to_string())
            } else if err_msg_lower.contains("ssl") || err_msg_lower.contains("cert") {
                ("SSL Error".to_string(), "SSL/TLS handshake failure".to_string())
            } else if err_msg_lower.contains("timeout") {
                ("Timeout".to_string(), "Request timed out".to_string())
            } else if err_msg_lower.contains("refused") {
                ("Connection Refused".to_string(), "Server refused connection".to_string())
            } else {
                ("Connection Error".to_string(), err_msg)
            };

            return MirrorStatus {
                url: mirror.to_string(),
                status,
                detail,
                source: MirrorStatusSource::LiveRecheckHtml,
            };
        }
    }

    MirrorStatus {
        url: mirror.to_string(),
        status: "Unknown Failure".to_string(),
        detail: "No response received".to_string(),
        source: MirrorStatusSource::LiveRecheckHtml,
    }
}

fn run_mirror_diagnostics(
    search_source: SearchSource,
    mirrors: Vec<(String, MirrorStatusSource)>,
    cached_report_statuses: Vec<MirrorStatus>,
    statuses: Arc<Mutex<Vec<MirrorStatus>>>,
    is_checking: Arc<Mutex<bool>>,
    ctx: egui::Context,
) {
    *is_checking.lock().unwrap() = true;
    let recheck_offset = cached_report_statuses.len();
    {
        let mut list = statuses.lock().unwrap();
        *list = cached_report_statuses;
        for (m, source) in &mirrors {
            list.push(MirrorStatus {
                url: m.clone(),
                status: "Checking...".to_string(),
                detail: String::new(),
                source: source.clone(),
            });
        }
    }
    ctx.request_repaint();

    let threads_count = mirrors.len();
    let completed_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    for (idx, (m, source)) in mirrors.into_iter().enumerate() {
        let statuses_clone = statuses.clone();
        let is_checking_clone = is_checking.clone();
        let completed_clone = completed_count.clone();
        let ctx_clone = ctx.clone();
        let search_source = search_source;

        thread::spawn(move || {
            let config = ureq::Agent::config_builder()
                .timeout_global(Some(std::time::Duration::from_secs(5)))
                .build();
            let agent: ureq::Agent = config.into();

            let res = check_single_mirror_status(&agent, &m, search_source);

            {
                let mut list = statuses_clone.lock().unwrap();
                let target_idx = recheck_offset + idx;
                if target_idx < list.len() {
                    list[target_idx] = MirrorStatus {
                        source,
                        ..res
                    };
                }
            }
            ctx_clone.request_repaint();

            let prev = completed_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if prev + 1 == threads_count {
                *is_checking_clone.lock().unwrap() = false;
                ctx_clone.request_repaint();
            }
        });
    }
}

fn refresh_cached_seeds_and_peers(
    cache_dir: PathBuf,
    ctx: egui::Context,
    status_sink: Arc<Mutex<Option<String>>>,
) {
    thread::spawn(move || {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(5)))
            .build();
        let agent: ureq::Agent = config.into();
        let mut files_updated = 0u32;

        if let Ok(entries) = fs::read_dir(&cache_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let metadata_path = path.join("metadata.json");
                    if metadata_path.exists() {
                        if let Ok(content) = fs::read_to_string(&metadata_path) {
                            if let Ok(mut meta) = serde_json::from_str::<MovieMetadata>(&content) {
                                if let Some(ref mut options) = meta.torrent_options {
                                    let clean_title = meta.film_title.clone().unwrap_or_else(|| meta.title.clone());
                                    let encoded = percent_encode(&clean_title);
                                    let url = format!("https://movies-api.accel.li/api/v2/list_movies.json?query_term={}", encoded);
                                    if let Ok(mut resp) = agent.get(&url).call() {
                                        if let Ok(body) = resp.body_mut().read_to_string() {
                                            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) {
                                                if let Some(movies_arr) = val["data"]["movies"].as_array() {
                                                    if let Some(matching_movie) = movies_arr.iter().find(|m| {
                                                        let m_title = m["title"].as_str().unwrap_or("");
                                                        let m_year = m["year"].as_u64().map(|y| y as u16);
                                                        m_title.to_lowercase() == clean_title.to_lowercase() && m_year == meta.year
                                                    }) {
                                                        if let Some(torrents_arr) = matching_movie["torrents"].as_array() {
                                                            let mut updated = false;
                                                            for opt in options.iter_mut() {
                                                                if let Some(t_val) = torrents_arr.iter().find(|t| {
                                                                    let t_hash = t["hash"].as_str().unwrap_or("").to_uppercase();
                                                                    t_hash == opt.hash.to_uppercase()
                                                                }) {
                                                                    let s = t_val["seeds"].as_u64().map(|v| v as u32);
                                                                    let p = t_val["peers"].as_u64().map(|v| v as u32);
                                                                    if s != opt.seeds || p != opt.peers {
                                                                        opt.seeds = s;
                                                                        opt.peers = p;
                                                                        updated = true;
                                                                    }
                                                                }
                                                            }
                                                            if updated {
                                                                if let Ok(new_content) = serde_json::to_string_pretty(&meta) {
                                                                    let _ = fs::write(&metadata_path, new_content);
                                                                    files_updated += 1;
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        let message = if files_updated > 0 {
            format!("Refresh complete; updated seeds and leechers for {} cached entr{}.", files_updated, if files_updated == 1 { "y" } else { "ies" })
        } else {
            "Refresh complete; no cached seed or leecher changes found.".to_string()
        };
        *status_sink.lock().unwrap() = Some(message);
        ctx.request_repaint();
    });
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MirrorStatusSource {
    CachedReportSearch,
    CachedReportApi,
    LiveRecheckHtml,
    LiveRecheckApi,
}

#[derive(Clone, Debug)]
struct MirrorStatus {
    url: String,
    status: String,
    detail: String,
    source: MirrorStatusSource,
}

struct AppState {
    active_tab: AppTab,
    movies: Vec<MovieGroup>,
    selected_idx: Option<usize>,
    ui_scale: f32,
    new_movie_url: String,
    library_filter: String,
    status_message: String,

    torrent_status: Arc<Mutex<TorrentStatusMap>>,
    spawned_children: Arc<Mutex<Vec<std::process::Child>>>,

    search_query: String,
    search_results: Arc<Mutex<Vec<YtsMovie>>>,
    search_page: usize,
    search_total_pages: Arc<Mutex<Option<usize>>>,
    is_searching: Arc<Mutex<bool>>,
    search_status: Arc<Mutex<String>>,
    search_cancelled: Arc<Mutex<bool>>,
    search_state_cache: HashMap<SearchSource, SearchStateSnapshot>,
    solidtorrents_sort_by_date: bool,
    search_source: SearchSource,
    search_manual_mode: bool,

    show_mirror_checker: bool,
    mirror_checker_source: SearchSource,
    mirror_statuses: Arc<Mutex<Vec<MirrorStatus>>>,
    is_checking_mirrors: Arc<Mutex<bool>>,
    background_status_message: Arc<Mutex<Option<String>>>,
    pending_delete_file: Option<(PathBuf, PathBuf)>,
    pending_delete_dir: Option<PathBuf>,
}

impl AppState {
    fn new(ctx: egui::Context) -> Self {
        let movies = scan_caches();
        let selected_idx = if movies.is_empty() { None } else { Some(0) };
        let ui_scale = ctx.pixels_per_point();

        let torrent_status = Arc::new(Mutex::new(TorrentStatusMap::new()));
        let spawned_children = Arc::new(Mutex::new(Vec::new()));
        let repaint_ctx = ctx.clone();

        // Spawn background polling thread to repaint the UI periodically (every 1 second) to capture real-time file size increases
        thread::spawn(move || {
            loop {
                repaint_ctx.request_repaint();
                thread::sleep(std::time::Duration::from_secs(1));
            }
        });

        Self {
            active_tab: AppTab::Library,
            movies,
            selected_idx,
            ui_scale,
            new_movie_url: String::new(),
            library_filter: String::new(),
            status_message: "Ready".to_string(),
            torrent_status,
            spawned_children,
            search_query: String::new(),
            search_results: Arc::new(Mutex::new(Vec::new())),
            search_page: 1,
            search_total_pages: Arc::new(Mutex::new(None)),
            is_searching: Arc::new(Mutex::new(false)),
            search_status: Arc::new(Mutex::new(String::new())),
            search_cancelled: Arc::new(Mutex::new(false)),
            search_state_cache: HashMap::new(),
            solidtorrents_sort_by_date: false,
            search_source: SearchSource::Yify,
            search_manual_mode: false,
            show_mirror_checker: false,
            mirror_checker_source: SearchSource::Yify,
            mirror_statuses: Arc::new(Mutex::new(Vec::new())),
            is_checking_mirrors: Arc::new(Mutex::new(false)),
            background_status_message: Arc::new(Mutex::new(None)),
            pending_delete_file: None,
            pending_delete_dir: None,
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

    fn save_current_search_state(&mut self) {
        let snapshot = SearchStateSnapshot {
            query: self.search_query.clone(),
            results: self.search_results.lock().unwrap().clone(),
            page: self.search_page,
            total_pages: *self.search_total_pages.lock().unwrap(),
            status: self.search_status.lock().unwrap().clone(),
        };
        self.search_state_cache.insert(self.search_source, snapshot);
    }

    fn load_search_state(&mut self, source: SearchSource) {
        if let Some(snapshot) = self.search_state_cache.get(&source).cloned() {
            self.search_query = snapshot.query;
            *self.search_results.lock().unwrap() = snapshot.results;
            self.search_page = snapshot.page.max(1);
            *self.search_total_pages.lock().unwrap() = snapshot.total_pages;
            *self.search_status.lock().unwrap() = snapshot.status;
        } else {
            self.search_query.clear();
            self.search_results.lock().unwrap().clear();
            self.search_page = 1;
            *self.search_total_pages.lock().unwrap() = None;
            self.search_status.lock().unwrap().clear();
        }
    }

    fn start_search_page(&mut self, ctx: &egui::Context, page: usize) {
        let query = self.search_query.trim().to_string();
        if query.is_empty() {
            return;
        }

        let page = page.max(1);
        self.search_page = page;
        let search_source = self.search_source;
        let solidtorrents_sort_by_date = self.solidtorrents_sort_by_date;
        let results_clone = self.search_results.clone();
        let total_pages_clone = self.search_total_pages.clone();
        let is_searching_clone = self.is_searching.clone();
        let status_clone = self.search_status.clone();
        let search_cancelled_clone = self.search_cancelled.clone();
        let ctx_clone = ctx.clone();

        *self.is_searching.lock().unwrap() = true;
        *self.search_cancelled.lock().unwrap() = false;
        *self.search_total_pages.lock().unwrap() = None;
        *self.search_status.lock().unwrap() = match search_source {
            SearchSource::Yify => format!("Stage 1: Searching YIFY JSON APIs (page {})...", page),
            SearchSource::X1337 => "Searching 1337x HTML mirrors...".to_string(),
            SearchSource::SolidTorrents => {
                format!("Searching SolidTorrents APIs (page {})...", page)
            }
            SearchSource::ExtTo => "Searching ext mirrors...".to_string(),
        };
        results_clone.lock().unwrap().clear();
        self.save_current_search_state();

        thread::spawn(move || {
            let mirrors = load_search_mirrors(search_source);
            if mirrors.is_empty() {
                *status_clone.lock().unwrap() =
                    format!("Error: {} mirror list not found!", search_source.source_label());
                *is_searching_clone.lock().unwrap() = false;
                ctx_clone.request_repaint();
                return;
            }

            let total_mirrors = mirrors.len();
            let active_threads = Arc::new(Mutex::new(total_mirrors));
            let html_mirrors_to_query = Arc::new(Mutex::new(Vec::new()));

            for mirror in &mirrors {
                if *search_cancelled_clone.lock().unwrap() {
                    break;
                }
                let query = query.clone();
                let results_clone = results_clone.clone();
                let total_pages_clone = total_pages_clone.clone();
                let status_clone = status_clone.clone();
                let ctx_clone = ctx_clone.clone();
                let active_threads = active_threads.clone();
                let html_mirrors_to_query = html_mirrors_to_query.clone();
                let search_cancelled_clone = search_cancelled_clone.clone();
                let mirror = mirror.clone();

                thread::spawn(move || {
                    if *search_cancelled_clone.lock().unwrap() {
                        let mut active = active_threads.lock().unwrap();
                        *active -= 1;
                        return;
                    }

                    let config = ureq::Agent::config_builder()
                        .timeout_global(Some(std::time::Duration::from_secs(4)))
                        .build();
                    let agent: ureq::Agent = config.into();

                    let (parsed_movies, is_json_success, page_count_hint) = match search_source {
                        SearchSource::Yify => {
                            let (movies, total_pages) =
                                scrape_yify_api_page(&agent, &mirror, &query, page);
                            (movies, true, total_pages)
                        }
                        SearchSource::SolidTorrents => {
                            let (movies, total_pages) =
                                scrape_solidtorrents_api_page(
                                    &agent,
                                    &mirror,
                                    &query,
                                    page,
                                    solidtorrents_sort_by_date,
                                );
                            (movies, true, total_pages)
                        }
                        SearchSource::ExtTo => {
                            let movies = scrape_ext_api(&agent, &mirror, &query);
                            let success = !movies.is_empty();
                            (movies, success, None)
                        }
                        SearchSource::X1337 => {
                            html_mirrors_to_query.lock().unwrap().push(mirror);
                            let mut active = active_threads.lock().unwrap();
                            *active -= 1;
                            ctx_clone.request_repaint();
                            return;
                        }
                    };

                    if let Some(total_pages) = page_count_hint {
                        let mut known_pages = total_pages_clone.lock().unwrap();
                        *known_pages = Some(known_pages.unwrap_or(1).max(total_pages));
                    }

                    if is_json_success {
                        if !parsed_movies.is_empty() && !*search_cancelled_clone.lock().unwrap() {
                            let mut results = results_clone.lock().unwrap();
                            merge_search_results(&mut results, parsed_movies);
                            let total_torrents: usize =
                                results.iter().map(|m| m.torrents.len()).sum();
                            let mut status = status_clone.lock().unwrap();
                            *status = format!(
                                "Stage 1: Updating JSON results for page {} (found {} torrent options)...",
                                page, total_torrents
                            );
                            ctx_clone.request_repaint();
                        }
                    } else {
                        html_mirrors_to_query.lock().unwrap().push(mirror);
                    }

                    let mut active = active_threads.lock().unwrap();
                    *active -= 1;
                    ctx_clone.request_repaint();
                });
            }

            loop {
                if *search_cancelled_clone.lock().unwrap() {
                    break;
                }
                let active = *active_threads.lock().unwrap();
                if active == 0 {
                    break;
                }
                thread::sleep(std::time::Duration::from_millis(50));
            }

            if *search_cancelled_clone.lock().unwrap() {
                *is_searching_clone.lock().unwrap() = false;
                *status_clone.lock().unwrap() = "Search stopped by user.".to_string();
                ctx_clone.request_repaint();
                return;
            }

            let html_mirrors = {
                let list = html_mirrors_to_query.lock().unwrap();
                list.clone()
            };

            if !html_mirrors.is_empty() {
                let total_html = html_mirrors.len();
                *status_clone.lock().unwrap() = format!(
                    "Stage 2: Searching {} HTML fallbacks for page {}...",
                    total_html, page
                );
                ctx_clone.request_repaint();

                let active_html_threads = Arc::new(Mutex::new(total_html));

                for mirror in html_mirrors {
                    if *search_cancelled_clone.lock().unwrap() {
                        break;
                    }
                    let query = query.clone();
                    let results_clone = results_clone.clone();
                    let status_clone = status_clone.clone();
                    let ctx_clone = ctx_clone.clone();
                    let active_html_threads = active_html_threads.clone();
                    let search_cancelled_clone = search_cancelled_clone.clone();
                    let mirror = mirror.clone();

                    thread::spawn(move || {
                        if *search_cancelled_clone.lock().unwrap() {
                            let mut active = active_html_threads.lock().unwrap();
                            *active -= 1;
                            return;
                        }

                        let config = ureq::Agent::config_builder()
                            .timeout_global(Some(std::time::Duration::from_secs(5)))
                            .build();
                        let agent: ureq::Agent = config.into();

                        let parsed_movies = match search_source {
                            SearchSource::Yify => {
                                scrape_yts_html_fallback(&agent, &mirror, &query, page)
                            }
                            SearchSource::X1337 => {
                                scrape_1337x_html_fallback(&agent, &mirror, &query)
                            }
                            SearchSource::SolidTorrents => {
                                scrape_solidtorrents_api_page(
                                    &agent,
                                    &mirror,
                                    &query,
                                    page,
                                    solidtorrents_sort_by_date,
                                )
                                .0
                            }
                            SearchSource::ExtTo => scrape_ext_html_fallback(&agent, &mirror, &query),
                        };

                        if !parsed_movies.is_empty() && !*search_cancelled_clone.lock().unwrap() {
                            let mut results = results_clone.lock().unwrap();
                            merge_search_results(&mut results, parsed_movies);
                            let total_torrents: usize =
                                results.iter().map(|m| m.torrents.len()).sum();
                            let mut status = status_clone.lock().unwrap();
                            *status = format!(
                                "Stage 2: Updating HTML results for page {} (found {} torrent options)...",
                                page, total_torrents
                            );
                            ctx_clone.request_repaint();
                        }

                        let mut active = active_html_threads.lock().unwrap();
                        *active -= 1;
                        ctx_clone.request_repaint();
                    });
                }

                loop {
                    if *search_cancelled_clone.lock().unwrap() {
                        break;
                    }
                    let active = *active_html_threads.lock().unwrap();
                    if active == 0 {
                        break;
                    }
                    thread::sleep(std::time::Duration::from_millis(50));
                }
            }

            *is_searching_clone.lock().unwrap() = false;
            let mut status = status_clone.lock().unwrap();
            if *search_cancelled_clone.lock().unwrap() {
                *status = "Search stopped by user.".to_string();
            } else {
                let final_results = results_clone.lock().unwrap().len();
                let total_pages = *total_pages_clone.lock().unwrap();
                if final_results == 0 {
                    *status = format!("No results found on page {} across any mirrors.", page);
                } else if let Some(total_pages) = total_pages {
                    *status = format!(
                        "Search completed. Found {} matches on page {} of {}.",
                        final_results, page, total_pages
                    );
                } else {
                    *status = format!(
                        "Search completed. Found {} matches on page {}.",
                        final_results, page
                    );
                }
            }
            ctx_clone.request_repaint();
        });
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
        // Force SIGKILL to clear any orphaned torrent client helper processes
        kill_managed_torrent_processes();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(message) = self.background_status_message.lock().unwrap().take() {
            self.status_message = message;
        }

        if self.status_message.starts_with("Stopping") {
            let has_active_torrents = {
                let map = self.torrent_status.lock().unwrap();
                map.values().any(|status| status.active)
            };
            if !has_active_torrents {
                self.status_message = "Torrent stopped.".to_string();
            }
        }

        if (ctx.pixels_per_point() - self.ui_scale).abs() > f32::EPSILON {
            ctx.set_pixels_per_point(self.ui_scale);
        }

        // Set custom visual aesthetics
        let mut style = (*ctx.style()).clone();
        style.visuals.dark_mode = true;
        style.visuals.override_text_color = Some(egui::Color32::from_rgb(220, 220, 225));
        style.visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(24, 24, 28);
        style.visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(36, 36, 42);
        style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(48, 48, 56);
        style.visuals.widgets.active.bg_fill = egui::Color32::from_rgb(60, 60, 70);
        ctx.set_style(style);

        if let Some((cache_path, media_path)) = self.pending_delete_file.clone() {
            egui::Window::new("Confirm Delete File")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label("Delete this local media file from disk?");
                    ui.label(egui::RichText::new(media_path.display().to_string()).weak());
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.pending_delete_file = None;
                        }
                        if ui
                            .add(
                                egui::Button::new("Delete File")
                                    .fill(egui::Color32::from_rgb(180, 40, 40)),
                            )
                            .clicked()
                        {
                            delete_local_media_file(&cache_path, &media_path);
                            self.pending_delete_file = None;
                            self.refresh();
                            self.status_message = "Deleted local media file.".to_string();
                        }
                    });
                });
        }

        if let Some(dir_path) = self.pending_delete_dir.clone() {
            egui::Window::new("Confirm Delete Film")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label("Delete this film library item and its cached files?");
                    ui.label(egui::RichText::new(dir_path.display().to_string()).weak());
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            self.pending_delete_dir = None;
                        }
                        if ui
                            .add(
                                egui::Button::new("Delete Film")
                                    .fill(egui::Color32::from_rgb(180, 40, 40)),
                            )
                            .clicked()
                        {
                            {
                                let mut children = self.spawned_children.lock().unwrap();
                                for child in children.iter_mut() {
                                    let _ = child.kill();
                                    let _ = child.wait();
                                }
                                children.clear();
                            }
                            kill_managed_torrent_processes();
                            self.torrent_status.lock().unwrap().clear();
                            let delete_result = if dir_path.exists() {
                                delete_dir_with_retries(&dir_path)
                            } else {
                                Ok(())
                            };
                            if delete_result.is_ok() && !dir_path.exists() {
                                self.pending_delete_dir = None;
                                self.selected_idx = None;
                                self.refresh();
                                self.status_message = "Deleted film library item.".to_string();
                            } else {
                                self.status_message = format!(
                                    "Failed to delete film library item on first attempt: {}",
                                    delete_result
                                        .err()
                                        .map(|err| err.to_string())
                                        .unwrap_or_else(|| "directory still exists".to_string())
                                );
                            }
                        }
                    });
                });
        }

        // Header Panel
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
	                    if ui.selectable_label(self.active_tab == AppTab::Library, "📂 Library").clicked() {
	                        self.active_tab = AppTab::Library;
	                        if self.selected_idx.is_none() && !self.movies.is_empty() {
	                            self.selected_idx = Some(0);
	                        }
	                        self.show_mirror_checker = false;
	                    }
                        if ui.selectable_label(self.active_tab == AppTab::Search, "🔍 Search").clicked() {
                            self.active_tab = AppTab::Search;
                            self.selected_idx = None;
                            self.show_mirror_checker = false;
                            self.status_message = "Opened search dashboard".to_string();
                        }
                        if ui.selectable_label(self.active_tab == AppTab::Settings, "⚙ Settings").clicked() {
                            self.active_tab = AppTab::Settings;
                            self.show_mirror_checker = false;
                            self.status_message = "Opened settings".to_string();
                        }
	                });
                ui.add_space(8.0);
            });
        });

        // Bottom Status / Controller Panel
        egui::TopBottomPanel::bottom("bottom_panel").show(ctx, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
		                ui.label(egui::RichText::new("Status:").weak());
	                ui.label(egui::RichText::new(&self.status_message).italics().color(egui::Color32::from_rgb(100, 200, 255)));
	                ui.separator();
                    let active_summary = {
                        let map = self.torrent_status.lock().unwrap();
                        let active_entries: Vec<_> = map.values().filter(|s| s.active).collect();
                        if active_entries.is_empty() {
                            None
                        } else {
                            Some(format!(
                                "⬇ {} active torrent{} — {}",
                                active_entries.len(),
                                if active_entries.len() == 1 { "" } else { "s" },
                                active_entries.iter().map(|s| s.speed.as_str()).collect::<Vec<_>>().join(" + ")
                            ))
                        }
                    };
	                if let Some(active_summary) = active_summary {
	                    ui.label(
	                        egui::RichText::new(active_summary)
	                        .color(egui::Color32::from_rgb(0, 220, 100))
	                        .strong(),
	                    );
	                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("⏹ Stop All Torrents")
                                    .color(egui::Color32::WHITE)
                                    .strong(),
                            )
                            .fill(egui::Color32::from_rgb(180, 40, 40)),
                        )
                        .clicked()
                    {
                        {
                            let mut children = self.spawned_children.lock().unwrap();
                            for child in children.iter_mut() {
                                let _ = child.kill();
                            }
                            children.clear();
                        }
                        kill_managed_torrent_processes();
                        let mut map = self.torrent_status.lock().unwrap();
                        for s in map.values_mut() {
                            s.active = false;
                        }
	                        self.status_message = "All torrents stopped.".to_string();
	                    }
	                }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("🔄 Refresh").clicked() {
                            self.refresh();
                            refresh_cached_seeds_and_peers(
                                get_cache_dir(),
                                ctx.clone(),
                                self.background_status_message.clone(),
                            );
                            self.status_message = "Cache refreshed; updating seeds and leechers in background...".to_string();
                        }
                    });
	            });
            ui.add_space(8.0);
        });

        // Left Sidebar: Movie Selection List
        if self.active_tab == AppTab::Library {
	            egui::SidePanel::left("left_panel")
	                .resizable(true)
	                .default_width(260.0)
	                .show(ctx, |ui| {
	                ui.add_space(8.0);
	                ui.heading("📂 Library");
                    ui.add_space(6.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.library_filter)
                            .hint_text("Search library...")
                            .desired_width(f32::INFINITY),
                    );
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
                        let filter = self.library_filter.trim().to_lowercase();
	        for (idx, group) in self.movies.iter().enumerate() {
	                            let is_selected = self.selected_idx == Some(idx);
	                            let title = match group.year {
	                                Some(year) => format!("{} ({year})", group.title),
	                                None => group.title.clone(),
	                            };
                            if !filter.is_empty() {
                                let matches_title = title.to_lowercase().contains(&filter);
                                let matches_torrent = group.torrents.iter().any(|torrent| {
                                    torrent.dir_name.to_lowercase().contains(&filter)
                                        || torrent.source_label.to_lowercase().contains(&filter)
                                        || torrent.metadata.as_ref().is_some_and(|meta| {
                                            meta.title.to_lowercase().contains(&filter)
                                                || meta.url.to_lowercase().contains(&filter)
                                                || meta.torrent_options.as_ref().is_some_and(|options| {
                                                    options.iter().any(|opt| {
                                                        opt.quality.to_lowercase().contains(&filter)
                                                            || opt.hash.to_lowercase().contains(&filter)
                                                            || opt.url.to_lowercase().contains(&filter)
                                                    })
                                                })
                                        })
                                });
                                if !matches_title && !matches_torrent {
                                    continue;
                                }
                            }

                             // Check if any torrent in this group is actively downloading
                             let is_downloading = {
                                 let map = self.torrent_status.lock().unwrap();
                                 group.torrents.iter().any(|t| {
                                     if map.get(&t.dir_name).map(|s| s.active).unwrap_or(false) {
                                         return true;
                                     }
                                     if let Some(ref meta) = t.metadata {
                                         if let Some(ref options) = meta.torrent_options {
                                             for opt in options {
                                                 if map.get(&opt.hash.to_uppercase()).map(|s| s.active).unwrap_or(false) {
                                                     return true;
                                                 }
                                             }
                                         }
                                     }
                                     false
                                 })
                             };
 
                             // Find the highest download percentage and check if complete
	                             let mut max_pct = 0.0;
		                             let mut is_complete = false;
		                             for t in &group.torrents {
		                                 let cache_dir_path = get_cache_dir().join(&t.dir_name);
                                     let option_cache_markers: Vec<(String, String, String)> = t
                                         .metadata
                                         .as_ref()
                                         .and_then(|meta| meta.torrent_options.as_ref())
                                         .map(|options| {
                                             options
                                                 .iter()
                                                 .map(|opt| {
                                                     let hash = opt.hash.to_uppercase();
                                                     (
                                                         hash.clone(),
                                                         opt.quality.clone(),
                                                         torrent_option_cache_subdir(&hash),
                                                     )
                                                 })
                                                 .collect()
                                         })
                                         .unwrap_or_default();
		                                 if let Some(ref meta) = t.metadata {
		                                     if let Some(ref options) = meta.torrent_options {
		                                         for opt in options {
                                                 let hash = opt.hash.to_uppercase();
                                                 let size_hint = parse_size_to_bytes(&opt.size);
                                                 let allow_legacy_size_match =
                                                     legacy_cache_size_best_matches_option(
                                                         &cache_dir_path.join(&opt.quality),
                                                         &hash,
                                                         &opt.quality,
                                                         size_hint,
                                                         options,
                                                     );
	                                                 let option_path = torrent_option_cache_path(
	                                                     &cache_dir_path,
	                                                     &opt.quality,
	                                                     &hash,
	                                                     size_hint,
                                                     allow_legacy_size_match,
	                                                 );
                                                 let uses_root_cache = option_path == cache_dir_path;
                                                 let excluded_subdirs = if uses_root_cache {
                                                     sibling_option_cache_subdirs(
                                                         &option_cache_markers,
                                                         &hash,
                                                     )
                                                 } else {
                                                     Vec::new()
                                                 };
	                                             if let Some((dl, tot)) = get_live_downloaded_and_total_excluding(
                                                     &option_path,
                                                     parse_size_to_bytes(&opt.size),
                                                     &excluded_subdirs,
                                                 ) {
	                                                 if tot > 0 {
	                                                     let pct = (dl as f64 / tot as f64) * 100.0;
	                                                     if pct > max_pct {
                                                         max_pct = pct;
                                                     }
                                                     if dl >= tot {
                                                         is_complete = true;
                                                     }
                                                 }
                                             }
		                                         }
		                                     }
		                                 }
		                                 if t
                                         .metadata
                                         .as_ref()
                                         .and_then(|meta| meta.torrent_options.as_ref())
                                         .is_none_or(|options| options.is_empty())
                                         && let Some((dl, tot)) = get_live_downloaded_and_total_excluding(
                                             &cache_dir_path,
                                             Some(t.logical_size_bytes),
                                             &[],
                                         )
                                     {
		                                     if tot > 0 {
		                                         let pct = (dl as f64 / tot as f64) * 100.0;
                                         if pct > max_pct {
                                             max_pct = pct;
                                         }
                                         if dl >= tot {
                                             is_complete = true;
                                         }
                                     }
                                 }
                             }
 
                             let prefix = if is_downloading {
                                 "⬇"
                             } else if is_complete {
                                 "✅"
                             } else {
                                 "🎞"
                             };
 
                             let suffix = if is_complete {
                                 " [Complete]".to_string()
                             } else {
                                 String::new()
                             };
                             let progress_text = group_sidebar_progress_text(group);

                             let item_text = format!(
                                 "{}{}\n{}",
                                 title,
                                 suffix,
                                 progress_text
                             );

                             ui.horizontal(|ui| {
                                 let prefix_color = if is_downloading {
                                     egui::Color32::from_rgb(0, 220, 100)
                                 } else if is_complete {
                                     egui::Color32::from_rgb(120, 220, 120)
                                 } else {
                                     ui.visuals().text_color()
                                 };
                                 ui.label(
                                     egui::RichText::new(prefix)
                                         .color(prefix_color)
                                         .strong(),
                                 );

                                 if ui
                                     .selectable_label(is_selected, egui::RichText::new(item_text))
                                     .clicked()
	                                 {
	                                     self.selected_idx = Some(idx);
	                                     self.active_tab = AppTab::Library;
	                                     self.status_message = format!("Selected: {}", title);
	                                 }
		                             });
		                             ui.add_space(4.0);
	                        }
	                    }
	                });
	            });
        }

        // Central Panel
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(8.0);
            if self.active_tab == AppTab::Library {
                if let Some(idx) = self.selected_idx {
                if idx < self.movies.len() {
                    let group = self.movies[idx].clone();
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            self.central_panel_rendering(ui, ctx, group);
                        });
                }
                }
            } else if self.active_tab == AppTab::Search {
                ui.vertical(|ui| {
                    if self.show_mirror_checker {
                        ui.horizontal(|ui| {
                            ui.heading(egui::RichText::new(format!("🌐 {} Mirror Status Checker", self.mirror_checker_source.source_label())).font(egui::FontId::proportional(20.0)).strong());
                            if ui.button("🔙 Back to Search").clicked() {
                                self.show_mirror_checker = false;
                            }
                        });
                        ui.add_space(8.0);
                        ui.label("Diagnose which proxy mirrors are currently responding, blocked by Cloudflare, or offline.");
                        ui.add_space(8.0);

                        if !*self.is_checking_mirrors.lock().unwrap()
                            && self.mirror_statuses.lock().unwrap().is_empty()
                        {
                            let rechecked_mirrors = load_diagnostic_recheck_mirrors(self.mirror_checker_source);
                            *self.mirror_statuses.lock().unwrap() =
                                load_report_only_diagnostic_statuses(self.mirror_checker_source, &rechecked_mirrors);
                        }

                        let is_checking = *self.is_checking_mirrors.lock().unwrap();
                        if is_checking {
                            ui.horizontal(|ui| {
                                ui.add(egui::widgets::Spinner::new());
                                let list = self.mirror_statuses.lock().unwrap();
                                let total = list.len();
                                let done = list.iter().filter(|s| s.status != "Checking...").count();
                                ui.label(format!("Scanning mirrors in parallel ({} / {} completed)...", done, total));
                            });
                        } else {
                            if ui.button("🔄 Run Diagnostics").clicked() {
                                let source = self.mirror_checker_source;
                                let mirrors = load_diagnostic_recheck_mirrors(source);
                                let cached_report_statuses =
                                    load_report_only_diagnostic_statuses(source, &mirrors);
                                let statuses_clone = self.mirror_statuses.clone();
                                let is_checking_clone = self.is_checking_mirrors.clone();
                                let ctx_clone = ctx.clone();
                                thread::spawn(move || {
	                                    run_mirror_diagnostics(
                                            source,
	                                        mirrors,
	                                        cached_report_statuses,
                                        statuses_clone,
                                        is_checking_clone,
                                        ctx_clone,
                                    );
                                });
                            }
                        }

                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(8.0);

	                        let list = self.mirror_statuses.lock().unwrap().clone();
	                        if list.is_empty() {
	                            ui.label(egui::RichText::new(format!(
                                    "No diagnostic report loaded yet. Run the verifier for {}, then reopen this view or click 'Run Diagnostics'.",
                                    self.mirror_checker_source.source_label()
                                )).italics().weak());
	                        } else {
                            let mut report_api = Vec::new();
                            let mut report_searchable = Vec::new();
                            let mut report_failed = Vec::new();
                            let mut rechecked_working_api = Vec::new();
                            let mut rechecked_working_html = Vec::new();
                            let mut rechecked_cf_block = Vec::new();
                            let mut rechecked_offline = Vec::new();
                            let mut rechecked_redirected = Vec::new();
                            let mut rechecked_other = Vec::new();

                            for item in list {
                                match item.source {
                                    MirrorStatusSource::CachedReportApi => report_api.push(item),
                                    MirrorStatusSource::CachedReportSearch => match item.status.as_str() {
                                        "Searchable (Report)" => report_searchable.push(item),
                                        _ => report_failed.push(item),
                                    },
                                    MirrorStatusSource::LiveRecheckApi
                                    | MirrorStatusSource::LiveRecheckHtml => match item.status.as_str() {
                                        "Working (API)" => rechecked_working_api.push(item),
                                        "Working (HTML)" => rechecked_working_html.push(item),
                                        "Cloudflare Block" => rechecked_cf_block.push(item),
                                        "Redirected" => rechecked_redirected.push(item),
                                        "Checking..." => rechecked_other.push(item),
                                        _ => rechecked_offline.push(item),
                                    },
                                }
                            }

                            egui::ScrollArea::vertical().show(ui, |ui| {
                                if !rechecked_working_api.is_empty() {
                                    egui::CollapsingHeader::new(
                                        egui::RichText::new(format!("🟢 Live Rechecked JSON APIs ({})", rechecked_working_api.len()))
                                            .color(egui::Color32::from_rgb(0, 220, 100))
                                            .strong()
                                    )
                                    .default_open(true)
                                    .show(ui, |ui| {
                                        for m in &rechecked_working_api {
                                            ui.horizontal(|ui| {
                                                ui.label(egui::RichText::new(&m.url).strong());
                                                ui.label(egui::RichText::new(&m.detail).weak().italics());
                                            });
                                        }
                                    });
                                }

                                if !rechecked_working_html.is_empty() {
                                    egui::CollapsingHeader::new(
                                        egui::RichText::new(format!("🔵 Live Rechecked HTML Mirrors ({})", rechecked_working_html.len()))
                                            .color(egui::Color32::from_rgb(100, 200, 255))
                                            .strong()
                                    )
                                    .default_open(true)
                                    .show(ui, |ui| {
                                        for m in &rechecked_working_html {
                                            ui.horizontal(|ui| {
                                                ui.label(egui::RichText::new(&m.url).strong());
                                                ui.label(egui::RichText::new(&m.detail).weak().italics());
                                            });
                                        }
                                    });
                                }

                                if !rechecked_cf_block.is_empty() {
                                    egui::CollapsingHeader::new(
                                        egui::RichText::new(format!("🟡 Live Rechecked Cloudflare JS Challenge ({})", rechecked_cf_block.len()))
                                            .color(egui::Color32::from_rgb(250, 180, 50))
                                            .strong()
                                    )
                                    .default_open(false)
                                    .show(ui, |ui| {
                                        for m in &rechecked_cf_block {
                                            ui.horizontal(|ui| {
                                                ui.label(egui::RichText::new(&m.url).strong());
                                                ui.label(egui::RichText::new(&m.detail).weak());
                                            });
                                        }
                                    });
                                }

                                if !rechecked_offline.is_empty() {
                                    egui::CollapsingHeader::new(
                                        egui::RichText::new(format!("🔴 Live Rechecked Dead / Offline / Failed ({})", rechecked_offline.len()))
                                            .color(egui::Color32::from_rgb(250, 80, 80))
                                            .strong()
                                    )
                                    .default_open(false)
                                    .show(ui, |ui| {
                                        for m in &rechecked_offline {
                                            ui.horizontal(|ui| {
                                                ui.label(egui::RichText::new(&m.url).strong());
                                                ui.label(egui::RichText::new(format!("{} - {}", m.status, m.detail)).weak());
                                            });
                                        }
                                    });
                                }

                                if !rechecked_redirected.is_empty() {
                                    egui::CollapsingHeader::new(
                                        egui::RichText::new(format!("⚪ Live Rechecked Redirected / Spam / Parked ({})", rechecked_redirected.len()))
                                            .color(egui::Color32::LIGHT_GRAY)
                                            .strong()
                                    )
                                    .default_open(false)
                                    .show(ui, |ui| {
                                        for m in &rechecked_redirected {
                                            ui.horizontal(|ui| {
                                                ui.label(egui::RichText::new(&m.url).strong());
                                                ui.label(egui::RichText::new(&m.detail).weak());
                                            });
                                        }
                                    });
                                }

                                if !report_api.is_empty() {
                                    egui::CollapsingHeader::new(
                                        egui::RichText::new(format!("📄 Search Report API Mirrors Not Rechecked ({})", report_api.len()))
                                            .color(egui::Color32::from_rgb(120, 200, 255))
                                            .strong()
                                    )
                                    .default_open(false)
                                    .show(ui, |ui| {
                                        for m in &report_api {
                                            ui.horizontal(|ui| {
                                                ui.label(egui::RichText::new(&m.url).strong());
                                                ui.label(egui::RichText::new(&m.detail).weak());
                                            });
                                        }
                                    });
                                }

                                if !report_searchable.is_empty() {
                                    egui::CollapsingHeader::new(
                                        egui::RichText::new(format!("📄 Search Report Searchable Mirrors Not Rechecked ({})", report_searchable.len()))
                                            .color(egui::Color32::from_rgb(120, 210, 170))
                                            .strong()
                                    )
                                    .default_open(false)
                                    .show(ui, |ui| {
                                        for m in &report_searchable {
                                            ui.horizontal(|ui| {
                                                ui.label(egui::RichText::new(&m.url).strong());
                                                ui.label(egui::RichText::new(&m.detail).weak());
                                            });
                                        }
                                    });
                                }

                                if !report_failed.is_empty() {
                                    egui::CollapsingHeader::new(
                                        egui::RichText::new(format!("📄 Search Report Failed Mirrors ({})", report_failed.len()))
                                            .color(egui::Color32::from_rgb(210, 120, 120))
                                            .strong()
                                    )
                                    .default_open(false)
                                    .show(ui, |ui| {
                                        for m in &report_failed {
                                            ui.horizontal(|ui| {
                                                ui.label(egui::RichText::new(&m.url).strong());
                                                ui.label(egui::RichText::new(&m.detail).weak());
                                            });
                                        }
                                    });
                                }

                                if !rechecked_other.is_empty() {
                                    egui::CollapsingHeader::new(
                                        egui::RichText::new(format!("⏳ Live Rechecked Other / Checking ({})", rechecked_other.len()))
                                            .color(egui::Color32::GRAY)
                                    )
                                    .default_open(true)
                                    .show(ui, |ui| {
                                        for m in &rechecked_other {
                                            ui.horizontal(|ui| {
                                                ui.label(&m.url);
                                                ui.label(egui::RichText::new(&m.status).weak());
                                            });
                                        }
                                    });
                                }
                            });
                        }
                    } else {
                        // Normal Search View!
	                        ui.heading(egui::RichText::new(self.search_source.heading()).font(egui::FontId::proportional(20.0)).strong());
	                        ui.add_space(8.0);
	                        ui.label(self.search_source.description());
	                        ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                for source in [SearchSource::Yify, SearchSource::X1337, SearchSource::SolidTorrents, SearchSource::ExtTo] {
                                    let selected = self.search_source == source;
                                    if ui.selectable_label(selected, source.source_label()).clicked() {
                                        if self.search_source != source {
                                            self.save_current_search_state();
                                        }
                                        self.search_manual_mode = false;
                                        self.search_source = source;
                                        self.load_search_state(source);
                                        self.mirror_checker_source = source;
                                        self.show_mirror_checker = false;
                                        self.mirror_statuses.lock().unwrap().clear();
                                        self.status_message = format!("Opened {} search dashboard", source.source_label());
                                    }
                                }
                                if ui.selectable_label(self.search_manual_mode, "Manual").clicked() {
                                    self.search_manual_mode = true;
                                    self.show_mirror_checker = false;
                                }
                            });
                            ui.add_space(8.0);

                            if self.search_manual_mode {
                                ui.horizontal(|ui| {
                                    let text_edit = ui.add(
                                        egui::TextEdit::singleline(&mut self.new_movie_url)
                                            .hint_text("Paste magnet link OR torrent download URL...")
                                            .desired_width(520.0),
                                    );
                                    let enter_pressed = text_edit.lost_focus()
                                        && ui.input(|i| i.key_pressed(egui::Key::Enter));

                                    if ui.button("➕ Add Torrent").clicked() || enter_pressed {
                                        let url = self.new_movie_url.trim().to_string();
                                        if !url.is_empty() {
                                            let hash = get_infohash(&url).unwrap_or_else(|| {
                                                let cleaned: String = url
                                                    .chars()
                                                    .map(|c| if c.is_alphanumeric() { c } else { '_' })
                                                    .collect();
                                                if cleaned.len() > 30 {
                                                    cleaned[cleaned.len() - 30..].to_string()
                                                } else {
                                                    cleaned
                                                }
                                            });
                                            let manual_title = get_infohash(&url)
                                                .map(|full_hash| {
                                                    let short_hash = full_hash.chars().take(12).collect::<String>();
                                                    format!("manual-torrent {}", short_hash)
                                                })
                                                .unwrap_or_else(|| hash.clone());
                                            let dir_name =
                                                cache_root_dir_name(&manual_title, None, &hash);
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
                                                "source_url": if url.starts_with("magnet:") { String::new() } else { url.clone() },
                                                "film_title": film_title,
                                                "year": year,
                                                "source_label": source_label,
                                                "media_kind": "unclassified",
                                                "seeds": null,
                                                "peers": null,
                                            });
                                            if let Ok(content) = serde_json::to_string_pretty(&meta_json) {
                                                let _ = fs::write(&meta_path, content);
                                            }

                                            self.refresh();

                                            if let Some(pos) = self.movies.iter().position(|m| m.key == dir_name) {
                                                self.selected_idx = Some(pos);
                                            }

                                            self.new_movie_url.clear();
                                            self.status_message = "Added torrent to Cache Library. Click 'Start Torrent' to download!".to_string();
                                        } else {
                                            self.status_message = "Error: Torrent URL cannot be empty!".to_string();
                                        }
                                    }
                                });
                            } else {
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
                                ui.add_space(4.0);
                                if ui.button(egui::RichText::new("⏹ Stop Search").strong()).clicked() {
                                    *self.search_cancelled.lock().unwrap() = true;
                                    *self.is_searching.lock().unwrap() = false;
                                    *self.search_status.lock().unwrap() = "Search stopped by user.".to_string();
                                }
                            } else {
	                                if ui.button(self.search_source.search_button_label()).clicked() || enter_pressed {
                                        self.start_search_page(ctx, 1);
                                    }
                            }

                            if self.search_source == SearchSource::SolidTorrents {
                                ui.menu_button("Search Options", |ui| {
                                    ui.checkbox(
                                        &mut self.solidtorrents_sort_by_date,
                                        "Sort by uploaded date on the API",
                                    );
                                });
                            }

                            if ui.button("🌐 Mirror Status Checker").clicked() {
                                self.show_mirror_checker = true;
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

                        if self.search_source.supports_pagination() {
                            let total_pages = *self.search_total_pages.lock().unwrap();
                            let can_go_prev = !is_searching && self.search_page > 1;
                            let can_go_next = !is_searching
                                && total_pages.map(|tp| self.search_page < tp).unwrap_or(true)
                                && !self.search_query.trim().is_empty();
                            ui.horizontal(|ui| {
                                ui.label(format!("Page {}", self.search_page));
                                if ui
                                    .add_enabled(can_go_prev, egui::Button::new("◀ Prev"))
                                    .clicked()
                                {
                                    self.start_search_page(ctx, self.search_page.saturating_sub(1));
                                }

                                let total_pages_for_buttons = total_pages.unwrap_or(self.search_page).max(1);
                                let start_page = self.search_page.saturating_sub(2).max(1);
                                let end_page = total_pages_for_buttons.min(start_page + 4);
                                for page_num in start_page..=end_page {
                                    if ui
                                        .selectable_label(self.search_page == page_num, page_num.to_string())
                                        .clicked()
                                        && !is_searching
                                        && page_num != self.search_page
                                    {
                                        self.start_search_page(ctx, page_num);
                                    }
                                }

                                if let Some(total_pages) = total_pages {
                                    if end_page < total_pages {
                                        ui.label("...");
                                        if ui
                                            .selectable_label(
                                                self.search_page == total_pages,
                                                total_pages.to_string(),
                                            )
                                            .clicked()
                                            && !is_searching
                                        {
                                            self.start_search_page(ctx, total_pages);
                                        }
                                    }
                                }

                                if ui
                                    .add_enabled(can_go_next, egui::Button::new("Next ▶"))
                                    .clicked()
                                {
                                    self.start_search_page(ctx, self.search_page + 1);
                                }
                            });
                            ui.add_space(6.0);
                        }
                            }

                        // Render search results scroll area
                        let results = self.search_results.lock().unwrap().clone();
	                        if !results.is_empty() {
	                            egui::ScrollArea::vertical().show(ui, |ui| {
	                                for movie in results {
                                        let effective_media_kind =
                                            effective_search_result_media_kind(&movie);
	                                    let title = match movie.year {
	                                        Some(y) => format!("{} ({})", movie.title, y),
	                                        None => movie.title.clone(),
	                                    };

		                                    ui.group(|ui| {
		                                        ui.vertical(|ui| {
                                                let add_allowed = effective_media_kind != MediaKind::Other;
		                                            ui.horizontal(|ui| {
	                                                ui.label(egui::RichText::new(title).strong().font(egui::FontId::proportional(16.0)));
                                                if let Some(r) = movie.rating {
                                                    ui.label(egui::RichText::new(format!("★ {}/10", r)).color(egui::Color32::from_rgb(255, 200, 0)));
                                                }
	                                                if let Some(rt) = movie.runtime {
	                                                    ui.label(format!("⏱ {} min", rt));
	                                                }
                                                if let Some(genres) = movie.genres.as_ref() {
                                                    if !genres.is_empty() {
                                                        ui.label(
                                                            egui::RichText::new(genres.join(", "))
                                                                .weak(),
                                                        );
                                                    }
                                                }

	                                                ui.separator();

                                                let add_label = match effective_media_kind {
                                                    MediaKind::Other => "⛔ Not Video",
                                                    _ => "➕ Add to Library",
                                                };
                                                let add_btn = ui.add_enabled(
                                                    add_allowed,
                                                    egui::Button::new(
                                                        egui::RichText::new(add_label).strong()
                                                    ),
                                                );
	                                                if add_btn.clicked() {
	                                                    if let Some(first_torrent) = movie.torrents.first() {
                                                            let source_label = self.search_source.source_label().to_string();
	                                                        let first_hash = first_torrent.hash.to_uppercase();
                                                        let incoming_hashes: Vec<String> = movie
                                                            .torrents
                                                            .iter()
                                                            .map(|t| t.hash.to_uppercase())
                                                            .collect();
                                                        let film_key = normalize_film_key(&movie.title, movie.year);
                                                        let desired_dir_name = cache_root_dir_name(
                                                            &movie.title,
                                                            movie.year,
                                                            &first_hash,
                                                        );
                                                        let mut dir_name = find_existing_cache_dir_for_movie(
                                                            &movie.title,
                                                            movie.year,
                                                            &incoming_hashes,
                                                        )
                                                        .unwrap_or_else(|| desired_dir_name.clone());
                                                        let cache_root = get_cache_dir();
                                                        let mut dest_dir = cache_root.join(&dir_name);
                                                        let desired_dest_dir = cache_root.join(&desired_dir_name);
                                                        if dir_name != desired_dir_name
                                                            && dest_dir.exists()
                                                            && !desired_dest_dir.exists()
                                                        {
                                                            if fs::rename(&dest_dir, &desired_dest_dir).is_ok() {
                                                                dir_name = desired_dir_name.clone();
                                                                dest_dir = desired_dest_dir;
                                                            }
                                                        }
                                                        let _ = fs::create_dir_all(&dest_dir);

                                                        let display_title = match movie.year {
                                                            Some(y) => format!("{} ({})", movie.title, y),
                                                            None => movie.title.clone(),
                                                        };

                                                        let t_opts: Vec<TorrentOption> = movie.torrents.iter().map(|t| TorrentOption {
		                                                            quality: t.quality.clone(),
		                                                            size: t.size.clone(),
		                                                            hash: t.hash.to_uppercase(),
		                                                            url: if t.url.is_empty() { make_magnet_link(&t.hash, &movie.title) } else { t.url.clone() },
                                                                    source_url: if t.source_url.is_empty() {
                                                                        if t.url.starts_with("magnet:") {
                                                                            String::new()
                                                                        } else {
                                                                            t.url.clone()
                                                                        }
                                                                    } else {
                                                                        t.source_url.clone()
                                                                    },
		                                                            seeds: t.seeds,
		                                                            peers: t.peers,
		                                                        }).collect();

		                                                        let mut meta_json = MovieMetadata {
		                                                            title: display_title,
			                                                            url: if first_torrent.url.is_empty() { make_magnet_link(&first_torrent.hash, &movie.title) } else { first_torrent.url.clone() },
                                                                    source_url: if first_torrent.source_url.is_empty() {
                                                                        if first_torrent.url.starts_with("magnet:") {
                                                                            String::new()
                                                                        } else {
                                                                            first_torrent.url.clone()
                                                                        }
                                                                    } else {
                                                                        first_torrent.source_url.clone()
                                                                    },
			                                                            film_title: Some(movie.title.clone()),
			                                                            year: movie.year,
			                                                            source_label: Some(source_label),
                                                            media_kind: effective_media_kind,
	                                                            duration: None,
	                                                            seeds: Some(0),
	                                                            peers: Some(0),
	                                                            torrent_options: Some(t_opts),
	                                                        };

                                                        let metadata_path = dest_dir.join("metadata.json");
                                                        if let Ok(existing_content) = fs::read_to_string(&metadata_path) {
                                                            if let Ok(existing_meta) = serde_json::from_str::<MovieMetadata>(&existing_content) {
                                                                meta_json.duration = existing_meta.duration.or(meta_json.duration);
                                                                meta_json.seeds = existing_meta.seeds.or(meta_json.seeds);
                                                                meta_json.peers = existing_meta.peers.or(meta_json.peers);
	                                                                if !existing_meta.url.is_empty() {
	                                                                    meta_json.url = existing_meta.url;
	                                                                }
                                                                    if !existing_meta.source_url.is_empty() {
                                                                        meta_json.source_url = existing_meta.source_url;
                                                                    }
                                                                if existing_meta.film_title.is_some() {
                                                                    meta_json.film_title = existing_meta.film_title;
                                                                }
	                                                                if existing_meta.source_label.is_some() {
	                                                                    meta_json.source_label = existing_meta.source_label;
	                                                                }
                                                                meta_json.media_kind = merge_media_kind(
                                                                    existing_meta.media_kind,
                                                                    meta_json.media_kind,
                                                                );
	                                                                meta_json.torrent_options = Some(merge_torrent_options(
	                                                                    existing_meta.torrent_options,
	                                                                    meta_json.torrent_options.take().unwrap_or_default(),
	                                                                ));
                                                            }
                                                        }

                                                        if let Ok(content) = serde_json::to_string_pretty(&meta_json) {
                                                            let _ = fs::write(&metadata_path, content);
                                                        }

	                                                        self.refresh();
	
	                                                        if let Some(pos) = self.movies.iter().position(|m| {
                                                                m.key == film_key
                                                                    || m.torrents.iter().any(|t| t.dir_name == dir_name)
                                                            }) {
	                                                            self.selected_idx = Some(pos);
	                                                        }
                                                            self.active_tab = AppTab::Library;
	
	                                                        self.status_message = format!("Added {} to Cache Library. Select qualities in sidebar to start downloading!", movie.title);
	                                                    } else {
	                                                        self.status_message = "Error: No stream options found for this film.".to_string();
	                                                    }
	                                                }
                                            });

		                                            ui.horizontal(|ui| {
	                                                let color = match effective_media_kind {
	                                                    MediaKind::Movie => egui::Color32::from_rgb(0, 200, 100),
	                                                    MediaKind::Episodic => egui::Color32::from_rgb(220, 180, 0),
                                                    MediaKind::Video => egui::Color32::from_rgb(100, 180, 255),
	                                                    MediaKind::Other => egui::Color32::from_rgb(220, 90, 90),
	                                                    MediaKind::Unclassified => egui::Color32::GRAY,
	                                                };
	                                                ui.label(egui::RichText::new("Type:").strong());
	                                                ui.label(
	                                                    egui::RichText::new(
                                                            media_kind_label(
                                                                effective_media_kind,
                                                                movie.title_long.as_deref().unwrap_or(&movie.title),
                                                            )
                                                        )
	                                                    .color(color)
	                                                    .strong(),
	                                                );
                                                    if let Some(uploaded_at) = movie.uploaded_at.as_deref() {
                                                        ui.label(
                                                            egui::RichText::new(format!("Uploaded {}", uploaded_at))
                                                                .weak(),
                                                        );
                                                    }
                                                    for torrent in &movie.torrents {
                                                        let btn_text = format!(
                                                            "📥 {} ({}) [found on {} site{}]",
                                                            torrent.quality,
                                                            torrent.size,
                                                            torrent.found_count,
                                                            if torrent.found_count == 1 { "" } else { "s" }
                                                        );
                                                        ui.label(egui::RichText::new(btn_text).weak());
                                                    }
	                                                if effective_media_kind == MediaKind::Other {
	                                                    ui.label(
	                                                        egui::RichText::new(
                                                            "Use Manual add to keep it anyway."
                                                        )
                                                        .weak(),
                                                    );
                                                }
                                            });

	                                            ui.add_space(4.0);
	                                        });
                                    });
                                    ui.add_space(6.0);
                                }
                            });
                        }
                    }
                });
            } else {
                ui.heading(egui::RichText::new("⚙ Settings").font(egui::FontId::proportional(20.0)).strong());
                ui.add_space(8.0);
                ui.label("Application DPI Scale");
                let old_scale = self.ui_scale;
                ui.add(
                    egui::Slider::new(&mut self.ui_scale, 0.75..=3.0)
                        .logarithmic(false)
                        .fixed_decimals(2),
                );
                if (self.ui_scale - old_scale).abs() > f32::EPSILON {
                    ctx.set_pixels_per_point(self.ui_scale);
                }
                if ui.button("Reset to Current System DPI").clicked() {
                    self.ui_scale = ctx.native_pixels_per_point().unwrap_or(1.0);
                    ctx.set_pixels_per_point(self.ui_scale);
                }
            }
        });
    }
}

// Extension trait helper for app rendering to avoid borrow checkers issues
trait PanelRenderHelper {
    fn central_panel_rendering(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        group: MovieGroup,
    );
}

impl PanelRenderHelper for AppState {
    fn central_panel_rendering(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        group: MovieGroup,
    ) {
        let title = match group.year {
            Some(year) => format!("{} ({year})", group.title),
            None => group.title.clone(),
        };
        ui.heading(
            egui::RichText::new(format!("🎞 {}", title))
                .font(egui::FontId::proportional(20.0))
                .strong(),
        );
        ui.label(
            egui::RichText::new(format!("Type: {}", media_kind_label(group.media_kind, &group.title)))
                .color(match group.media_kind {
                    MediaKind::Movie => egui::Color32::from_rgb(0, 200, 100),
                    MediaKind::Episodic => egui::Color32::from_rgb(220, 180, 0),
                    MediaKind::Video => egui::Color32::from_rgb(100, 180, 255),
                    MediaKind::Other => egui::Color32::from_rgb(220, 90, 90),
                    MediaKind::Unclassified => egui::Color32::GRAY,
                })
                .strong(),
        );
        ui.add_space(8.0);
        let primary_torrent = group
            .torrents
            .iter()
            .find(|torrent| {
                torrent
                    .metadata
                    .as_ref()
                    .and_then(|meta| meta.torrent_options.as_ref())
                    .is_some_and(|options| !options.is_empty())
            })
            .or_else(|| group.torrents.first())
            .cloned();
 
        // Quality Option Buttons at the top (from metadata.torrent_options)
        if let Some(torrent) = primary_torrent.clone() {
            let parent_cache_path = get_cache_dir().join(&torrent.dir_name);
            let root_hash = cache_root_hash(&parent_cache_path, torrent.metadata.as_ref());
            let torrent_options = torrent
                .metadata
                .as_ref()
                .and_then(|meta| meta.torrent_options.clone());
            let root_option_cache_markers: Vec<(String, String, String)> = torrent_options
                .as_ref()
                .map(|options| {
                    options
                        .iter()
                        .map(|opt| {
                            let hash = opt.hash.to_uppercase();
                            (
                                hash.clone(),
                                opt.quality.clone(),
                                torrent_option_cache_subdir(&hash),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let root_excluded_subdirs = root_hash
                .as_ref()
                .map(|hash| sibling_option_cache_subdirs(&root_option_cache_markers, hash))
                .unwrap_or_default();
            let root_media = find_media_file_excluding(&parent_cache_path, &root_excluded_subdirs);
            let root_has_torrent = parent_cache_path.join("movie.torrent").exists();
            let root_is_active = {
                let map = self.torrent_status.lock().unwrap();
                map.get(&torrent.dir_name).map(|s| s.active).unwrap_or(false)
            };
            let root_payload_disk =
                get_payload_disk_space_excluding(&parent_cache_path, &root_excluded_subdirs);
            let root_control_disk =
                get_control_disk_space_excluding(&parent_cache_path, &root_excluded_subdirs);
            let root_has_cache_artifacts = root_payload_disk > 0 || root_control_disk > 0;
            let has_torrent_options = torrent_options.as_ref().is_some_and(|options| !options.is_empty());
            let root_has_local_status =
                root_has_torrent || root_media.is_some() || root_is_active || root_has_cache_artifacts;
            if root_has_local_status && !has_torrent_options {
                ui.label(egui::RichText::new("Quality Options Local Status:").strong());
                ui.add_space(6.0);
                ui.group(|ui| {
                    ui.vertical(|ui| {
                        let root_torrent_option = torrent_options.as_ref()
                            .and_then(|opts| {
                                if let Some(ref hash) = root_hash {
                                    opts.iter().find(|opt| opt.hash.eq_ignore_ascii_case(hash))
                                        .or_else(|| opts.first())
                                } else {
                                    opts.first()
                                }
                            });
                        let label = root_torrent_option
                            .map(|opt| format!("Default Stream ({})", opt.quality))
                            .unwrap_or_else(|| "Default Stream".to_string());

                        ui.label(egui::RichText::new(label).strong());
                        if let Some(runtime) = movie_runtime_label(torrent.metadata.as_ref()) {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("Runtime:").strong());
                                ui.label(runtime);
                            });
                        }
                        ui.add_space(4.0);

	                        if let Some(ref hash) = root_hash {
	                            ui.horizontal(|ui| {
	                                ui.label(egui::RichText::new("Info Hash:").strong());
	                                ui.label(hash);
	                            });
		                                let source_url = torrent
                                            .metadata
                                            .as_ref()
                                            .map(|m| {
                                                if m.source_url.is_empty() {
                                                    m.url.clone()
                                                } else {
                                                    m.source_url.clone()
                                                }
                                            })
                                            .unwrap_or_default();
		                                let display_magnet = launch_magnet_for_display(
                                            torrent.metadata.as_ref().map(|m| m.url.as_str()).unwrap_or(""),
                                            hash,
                                            &title,
                                        );
	                                if should_show_source_url(&source_url, &display_magnet) {
	                                ui.horizontal_wrapped(|ui| {
	                                    ui.label(egui::RichText::new("Source URL:").strong());
	                                    ui.monospace(display_source_url(&source_url));
	                                });
                                }
	                                if !display_magnet.is_empty() {
	                                    ui.horizontal_wrapped(|ui| {
	                                        ui.label(egui::RichText::new("Launch Magnet:").strong());
                                        ui.monospace(&display_magnet);
                                    });
                                }
	                        }

                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Subfolder Path:").strong());
                            ui.label(cache_item_path(&torrent.dir_name).display().to_string());
                        });

                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Disk Space Used:").strong());
                                    ui.label(format_size(root_payload_disk));
                                });

                                if root_control_disk > 0 {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new("Torrent Metadata / Control Files:")
                                                .strong(),
                                        );
                                        ui.label(format_size(root_control_disk));
                                    });
                                }

                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Total File Size:").strong());
                            ui.label(format_size(torrent.logical_size_bytes));
                        });

                        if let Some(opt) = root_torrent_option {
                            if let (Some(s), Some(l)) = (opt.seeds, opt.peers) {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("Seeds / Leechers:").strong());
                                    ui.label(format!("{} seeds · {} leechers", s, l));
                                });
                            }
                        }

                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Subtitles:").strong());
                            let cached_langs = cached_subtitle_languages(&parent_cache_path);
                            if !cached_langs.is_empty() {
                                let summary = format_subtitle_summary(&cached_langs);
                                ui.label(
                                    egui::RichText::new(format!("Available (cached): {}", summary))
                                        .color(egui::Color32::from_rgb(0, 200, 100))
                                        .strong(),
                                );
                            } else {
                                match torrent_subtitle_languages(&parent_cache_path) {
                                    Some(langs) if !langs.is_empty() => {
                                        let summary = format_subtitle_summary(&langs);
                                        ui.label(
                                            egui::RichText::new(format!("In torrent: {}", summary))
                                                .color(egui::Color32::from_rgb(200, 160, 0))
                                                .strong(),
                                        );
                                    }
                                    Some(_) => {
                                        ui.label(egui::RichText::new("None in torrent").weak());
                                    }
                                    None => {
                                        ui.label(egui::RichText::new("Unknown (start torrent to check)").weak());
                                    }
                                }
                            }
                        });

                            let per_status = {
                                let map = self.torrent_status.lock().unwrap();
                                map.get(&torrent.dir_name).cloned()
                            };
                            let root_startup_detail = per_status.as_ref().and_then(|s| {
                                if s.active
                                    && !s.detail.trim().is_empty()
                                    && !status_speed_is_rate(s.speed.trim())
                                {
                                    Some(s.detail.clone())
                                } else {
                                    None
                                }
                            });
	                        ui.horizontal(|ui| {
	                            ui.label(egui::RichText::new("Torrented Status:").strong());

		                                if let Some(ref s) = per_status {
	                                    if s.active {
                                            let live_downloaded = normalized_torrent_progress_text(
                                                &parent_cache_path,
                                                &s.downloaded,
                                            )
                                            .unwrap_or_else(|| s.downloaded.clone());
	                                        ui.label(
	                                        egui::RichText::new(format_live_torrent_status(
	                                            &live_downloaded,
	                                            &format_size(torrent.logical_size_bytes),
	                                            &s.speed,
	                                            &s.peers,
	                                            &s.detail,
                                                &s.mode,
			                            ))
	                                        .color(egui::Color32::from_rgb(0, 220, 100))
	                                        .strong(),
	                                    );
			                                } else if let Some((dl, total)) =
			                                    get_live_downloaded_and_total_excluding(
                                                    &parent_cache_path,
                                                    Some(torrent.logical_size_bytes),
                                                    &root_excluded_subdirs,
                                                )
			                                {
	                                    let pct = if total > 0 { (dl as f64 / total as f64) * 100.0 } else { 0.0 };
	                                    ui.label(format!("{} / {} ({:.2}%)", format_size(dl), format_size(total), pct));
	                                } else {
	                                    ui.label(format_size(root_payload_disk));
	                                }
	                            } else if let Some((dl, total)) =
	                                get_live_downloaded_and_total_excluding(
                                        &parent_cache_path,
                                        Some(torrent.logical_size_bytes),
                                        &root_excluded_subdirs,
                                    )
	                            {
	                                let pct = if total > 0 { (dl as f64 / total as f64) * 100.0 } else { 0.0 };
		                                ui.label(format!("{} / {} ({:.2}%)", format_size(dl), format_size(total), pct));
		                            } else {
		                                ui.label(format_size(root_payload_disk));
		                            }
	                        });
                            if let Some(detail) = root_startup_detail {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "Startup diagnostics: {}",
                                        detail
                                    ))
                                    .weak(),
                                );
                            }

                        if let Some(ref path) = root_media {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(egui::RichText::new("Largest Media File:").strong());
                                ui.label(path.file_name().unwrap_or_default().to_string_lossy());
                            });
                        }
                        render_torrent_file_progress_dropdown(ui, &parent_cache_path);

                        ui.add_space(8.0);

                        ui.horizontal(|ui| {
                            let is_this_active = {
                                let map = self.torrent_status.lock().unwrap();
                                map.get(&torrent.dir_name).map(|s| s.active).unwrap_or(false)
                            };

                            if is_this_active {
                                let stop_btn = ui.add_sized(
                                    [160.0, 40.0],
                                    egui::Button::new(
                                        egui::RichText::new("⏹ Stop Torrent")
                                            .font(egui::FontId::proportional(14.0))
                                            .strong(),
                                    )
                                    .fill(egui::Color32::from_rgb(180, 40, 40)),
                                );
                                if stop_btn.clicked() {
                                    let dir_to_stop = torrent.dir_name.clone();
                                    let children_clone = self.spawned_children.clone();
                                    let torrent_status_clone = self.torrent_status.clone();
                                    thread::spawn(move || {
                                        let mut children = children_clone.lock().unwrap();
                                        for child in children.iter_mut() {
                                            let _ = child.kill();
                                        }
                                        children.clear();
                                        kill_managed_torrent_processes();
                                        let mut map = torrent_status_clone.lock().unwrap();
                                        if let Some(s) = map.get_mut(&dir_to_stop) {
                                            s.active = false;
                                        }
                                    });
                                    self.status_message = "Stopping torrent...".to_string();
                                }
                            } else {
                                let mut start_mode: Option<bool> = None;
                                ui.horizontal(|ui| {
                                    let start_normal_btn = ui.add_sized(
                                        [170.0, 40.0],
                                        egui::Button::new(
                                            egui::RichText::new("🧲 Start Normal")
                                                .font(egui::FontId::proportional(14.0))
                                                .strong(),
                                        )
                                        .fill(egui::Color32::from_rgb(0, 130, 90)),
                                    );
                                    if start_normal_btn.clicked() {
                                        start_mode = Some(false);
                                    }

                                    let start_sequential_btn = ui.add_sized(
                                        [190.0, 40.0],
                                        egui::Button::new(
                                            egui::RichText::new("🧲 Start Sequential")
                                                .font(egui::FontId::proportional(14.0))
                                                .strong(),
                                        )
                                        .fill(egui::Color32::from_rgb(0, 160, 80)),
                                    );
                                    if start_sequential_btn.clicked() {
                                        start_mode = Some(true);
                                    }
                                });
                                if let Some(sequential_mode) = start_mode {
	                                    let url_to_play = torrent.metadata.as_ref().map(|m| m.url.clone()).unwrap_or_default();
                                        let root_hash_for_launch =
                                            cache_root_hash(&cache_item_path(&torrent.dir_name), torrent.metadata.as_ref())
                                                .unwrap_or_default();
                                    let dir_to_play = torrent.dir_name.clone();
                                    let children_clone = self.spawned_children.clone();
                                    let torrent_status_clone = self.torrent_status.clone();
                                    let ctx_clone = ctx.clone();
                                    thread::spawn(move || {
	                                        let mut children = children_clone.lock().unwrap();

		                                        let dest_dir = cache_item_path(&dir_to_play);
		                                        let local_torrent_path = dest_dir.join("movie.torrent");
		                                        let torrent_source = launch_source_from_url_or_hash(
	                                                &url_to_play,
	                                                &root_hash_for_launch,
	                                                "Movie",
	                                                &local_torrent_path.display().to_string(),
	                                            );

			                                        let _ = fs::create_dir_all(&dest_dir);

	                                            initialize_torrent_runtime_state(
                                                &torrent_status_clone,
                                                &dir_to_play,
                                                &dest_dir,
                                                Some(torrent.logical_size_bytes),
                                                sequential_mode,
                                            );
                                            ctx_clone.request_repaint();

			                                        let mut cmd = build_libtorrent_worker_command(
	                                                    torrent_source.as_str(),
	                                                    dest_dir.to_string_lossy().as_ref(),
	                                                    "Movie",
	                                                    sequential_mode,
	                                                    sequential_mode.then_some(20),
                                                );

			                                        match cmd.spawn() {
			                                            Ok(mut child) => {
	                                                let _child_pid = child.id();
			                                                if let Some(stderr) = child.stderr.take() {
			                                                    spawn_torrent_client_output_reader(
			                                                        stderr,
			                                                        "LIBTORRENT ERR",
			                                                        torrent_status_clone.clone(),
			                                                        dir_to_play.clone(),
			                                                        dest_dir.clone(),
			                                                        Some(torrent.logical_size_bytes),
			                                                        ctx_clone.clone(),
			                                                    );
			                                                }
			                                                if let Some(stdout) = child.stdout.take() {
			                                                    spawn_torrent_client_output_reader(
			                                                        stdout,
			                                                        "LIBTORRENT",
			                                                        torrent_status_clone.clone(),
			                                                        dir_to_play.clone(),
			                                                        dest_dir.clone(),
			                                                        Some(torrent.logical_size_bytes),
			                                                        ctx_clone.clone(),
			                                                    );
		                                                }
	                                                children.push(child);
	                                            }
	                                            Err(err) => {
	                                                let mut map = torrent_status_clone.lock().unwrap();
	                                                if let Some(s) = map.get_mut(&dir_to_play) {
	                                                    s.active = false;
	                                                    s.speed = "Error".to_string();
	                                                    s.peers = err.to_string();
	                                                }
	                                                ctx_clone.request_repaint();
	                                            }
		                                        }
	                                    });
	                                    self.status_message = if sequential_mode {
                                            "Starting sequential torrent download...".to_string()
                                        } else {
                                            "Starting normal torrent download...".to_string()
                                        };
                                }
                            }

                            if let Some(media_path) = root_media.clone() {
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
                                    let progress_file =
                                        resolve_playback_progress_file(&parent_cache_path, &media_path);
                                    match launch_mpv(
                                        &self.spawned_children,
                                        &media_path,
                                        &progress_file,
                                    ) {
                                        Ok(()) => {
                                            self.status_message =
                                                "Opened local file in mpv.".to_string();
                                        }
                                        Err(err) => {
                                            self.status_message =
                                                format!("Failed to open local file in mpv: {err}");
                                        }
                                    }
                                }

                                let delete_file_btn = ui.add_sized(
                                    [150.0, 40.0],
                                    egui::Button::new("🗑 Delete File")
                                        .fill(egui::Color32::from_rgb(145, 70, 35)),
	                                );
	                                if delete_file_btn.clicked() {
	                                    self.pending_delete_file = Some((parent_cache_path.clone(), media_path.clone()));
	                                }
	                            }
                        });
                    });
                });
                ui.add_space(8.0);
            }

            if let Some(options) = torrent_options.clone() {
                if !options.is_empty() {
                    ui.label(egui::RichText::new("Torrent Options:").strong());
                    ui.add_space(4.0);

                    let option_cache_markers: Vec<(String, String, String)> = options
                        .iter()
                        .map(|opt| {
                            let hash = opt.hash.to_uppercase();
                            (
                                hash.clone(),
                                opt.quality.clone(),
                                torrent_option_cache_subdir(&hash),
                            )
                        })
                        .collect();
                    let options_for_matching = options.clone();

                    for opt in options {
                        let hash = opt.hash.to_uppercase();
                        let size_hint = parse_size_to_bytes(&opt.size);
                        let allow_legacy_size_match = legacy_cache_size_best_matches_option(
                            &parent_cache_path.join(&opt.quality),
                            &hash,
                            &opt.quality,
                            size_hint,
                            &options_for_matching,
                        );
                        let local_cache_path = torrent_option_cache_path(
                            &parent_cache_path,
                            &opt.quality,
                            &hash,
                            size_hint,
                            allow_legacy_size_match,
                        );
                        let uses_root_cache = local_cache_path == parent_cache_path;
                        let excluded_subdirs = if uses_root_cache {
                            sibling_option_cache_subdirs(&option_cache_markers, &hash)
                        } else {
                            Vec::new()
                        };
                        let local_media =
                            find_media_file_excluding(&local_cache_path, &excluded_subdirs);
                        let has_torrent = local_cache_path.join("movie.torrent").exists();
                        let local_payload_disk =
                            get_payload_disk_space_excluding(&local_cache_path, &excluded_subdirs);
                        let local_control_disk =
                            get_control_disk_space_excluding(&local_cache_path, &excluded_subdirs);
                        let has_cache_artifacts = if uses_root_cache {
                            local_payload_disk > 0 || local_control_disk > 0
                        } else {
                            has_local_cache_artifacts(&local_cache_path)
                        };
                        let is_active = {
                            let map = self.torrent_status.lock().unwrap();
                            map.get(&hash).map(|s| s.active).unwrap_or(false)
                        };

                        ui.group(|ui| {
                            ui.vertical(|ui| {
                                ui.label(
                                    egui::RichText::new(format!("Stream Option: {}", opt.quality)).strong(),
                                );
                                if let Some(runtime) = movie_runtime_label(torrent.metadata.as_ref()) {
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new("Runtime:").strong());
                                        ui.label(runtime);
                                    });
                                }
                                ui.add_space(4.0);

                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("Info Hash:").strong());
                                    ui.label(&opt.hash);
                                });

                                let source_url = if opt.source_url.is_empty() {
                                    opt.url.clone()
                                } else {
                                    opt.source_url.clone()
                                };
                                let display_magnet =
                                    launch_magnet_for_display(&opt.url, &opt.hash, &title);
                                if should_show_source_url(&source_url, &display_magnet) {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(egui::RichText::new("Source URL:").strong());
                                        ui.monospace(display_source_url(&source_url));
                                    });
                                }

                                if !display_magnet.is_empty() {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(egui::RichText::new("Launch Magnet:").strong());
                                        ui.monospace(&display_magnet);
                                    });
                                }

                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("Total File Size:").strong());
                                    ui.label(&opt.size);
                                });

                                if let (Some(s), Some(l)) = (opt.seeds, opt.peers) {
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new("Seeds / Leechers:").strong());
                                        ui.label(format!("{} seeds · {} leechers", s, l));
                                    });
                                }

                                if has_torrent || local_media.is_some() || is_active || has_cache_artifacts {
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new("Subfolder Path:").strong());
                                        if uses_root_cache {
                                            ui.label(cache_item_path(&torrent.dir_name).display().to_string());
                                        } else {
                                            ui.label(local_cache_path.display().to_string());
                                        }
                                    });

                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new("Disk Space Used:").strong());
                                        ui.label(format_size(local_payload_disk));
                                    });

                                    if local_control_disk > 0 {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                egui::RichText::new(
                                                    "Torrent Metadata / Control Files:",
                                                )
                                                .strong(),
                                            );
                                            ui.label(format_size(local_control_disk));
                                        });
                                    }

                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new("Subtitles:").strong());
                                        let cached_langs = cached_subtitle_languages(&local_cache_path);
                                        if !cached_langs.is_empty() {
                                            let summary = format_subtitle_summary(&cached_langs);
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "Available (cached): {}",
                                                    summary
                                                ))
                                                .color(egui::Color32::from_rgb(0, 200, 100))
                                                .strong(),
                                            );
                                        } else {
                                            match torrent_subtitle_languages(&local_cache_path) {
                                                Some(langs) if !langs.is_empty() => {
                                                    let summary = format_subtitle_summary(&langs);
                                                    ui.label(
                                                        egui::RichText::new(format!(
                                                            "In torrent: {}",
                                                            summary
                                                        ))
                                                        .color(egui::Color32::from_rgb(200, 160, 0))
                                                        .strong(),
                                                    );
                                                }
                                                Some(_) => {
                                                    ui.label(
                                                        egui::RichText::new("None in torrent").weak(),
                                                    );
                                                }
                                                None => {
                                                    ui.label(
                                                        egui::RichText::new(
                                                            "Unknown (start torrent to check)",
                                                        )
                                                        .weak(),
                                                    );
                                                }
                                            }
                                        }
                                    });

                                    let per_status = {
                                        let map = self.torrent_status.lock().unwrap();
                                        map.get(&hash).cloned()
                                    };
                                    let option_startup_detail = per_status.as_ref().and_then(|s| {
                                        if s.active
                                            && !s.detail.trim().is_empty()
                                            && !status_speed_is_rate(s.speed.trim())
                                        {
                                            Some(s.detail.clone())
                                        } else {
                                            None
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new("Torrented Status:").strong());

		                                        if let Some(ref s) = per_status {
		                                            if s.active {
                                                    let live_downloaded =
                                                        normalized_torrent_progress_text(
                                                            &local_cache_path,
                                                            &s.downloaded,
                                                        )
                                                        .unwrap_or_else(|| s.downloaded.clone());
	                                                ui.label(
		                                                    egui::RichText::new(format_live_torrent_status(
		                                                        &live_downloaded,
		                                                        &opt.size,
		                                                        &s.speed,
		                                                        &s.peers,
	                                                            &s.detail,
                                                                &s.mode,
			                                                    ))
		                                                    .color(egui::Color32::from_rgb(0, 220, 100))
		                                                    .strong(),
		                                                );
			                                            } else if let Some((dl, total)) =
			                                                get_live_downloaded_and_total_excluding(
			                                                    &local_cache_path,
                                                    size_hint,
                                                    &excluded_subdirs,
                                                )
                                            {
                                                let pct = if total > 0 {
                                                    (dl as f64 / total as f64) * 100.0
                                                } else {
                                                    0.0
                                                };
                                                ui.label(format!(
                                                    "{} / {} ({:.2}%)",
                                                    format_size(dl),
                                                    format_size(total),
                                                    pct
                                                ));
                                            } else {
                                                ui.label(format_size(local_payload_disk));
                                            }
                                        } else if let Some((dl, total)) = get_live_downloaded_and_total_excluding(
                                            &local_cache_path,
                                            size_hint,
                                            &excluded_subdirs,
                                        ) {
                                            let pct = if total > 0 {
                                                (dl as f64 / total as f64) * 100.0
                                            } else {
                                                0.0
                                            };
                                            ui.label(format!(
                                                "{} / {} ({:.2}%)",
                                                format_size(dl),
                                                format_size(total),
                                                pct
                                            ));
                                        } else {
                                            ui.label(format_size(local_payload_disk));
                                        }
                                    });
                                    if let Some(detail) = option_startup_detail {
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "Startup diagnostics: {}",
                                                detail
                                            ))
                                            .weak(),
                                        );
                                    }

                                    if let Some(ref path) = local_media {
                                        ui.horizontal_wrapped(|ui| {
                                            ui.label(egui::RichText::new("Largest Media File:").strong());
                                            ui.label(path.file_name().unwrap_or_default().to_string_lossy());
                                        });
                                    }
                                    render_torrent_file_progress_dropdown(ui, &local_cache_path);
                                }

                                ui.add_space(8.0);

                                ui.horizontal(|ui| {
                                    if is_active {
                                        let stop_btn = ui.add_sized(
                                            [160.0, 40.0],
                                            egui::Button::new(
                                                egui::RichText::new("⏹ Stop Torrent")
                                                    .font(egui::FontId::proportional(14.0))
                                                    .strong(),
                                            )
                                            .fill(egui::Color32::from_rgb(180, 40, 40)),
                                        );
                                        if stop_btn.clicked() {
                                            let hash_clone = hash.clone();
                                            let children_clone = self.spawned_children.clone();
                                            let torrent_status_clone = self.torrent_status.clone();
                                            thread::spawn(move || {
                                                let mut children = children_clone.lock().unwrap();
                                                for child in children.iter_mut() {
                                                    let _ = child.kill();
                                                }
                                                children.clear();
                                                kill_managed_torrent_processes();
                                                let mut map = torrent_status_clone.lock().unwrap();
                                                if let Some(s) = map.get_mut(&hash_clone) {
                                                    s.active = false;
                                                }
                                            });
                                            self.status_message =
                                                format!("Stopping {} torrent...", opt.quality);
                                        }
                                    } else {
                                        let mut start_mode: Option<bool> = None;
                                        ui.horizontal(|ui| {
                                            let start_normal_btn = ui.add_sized(
                                                [170.0, 40.0],
                                                egui::Button::new(
                                                    egui::RichText::new("🧲 Start Normal")
                                                        .font(egui::FontId::proportional(14.0))
                                                        .strong(),
                                                )
                                                .fill(egui::Color32::from_rgb(0, 130, 90)),
                                            );
                                            if start_normal_btn.clicked() {
                                                start_mode = Some(false);
                                            }

                                            let start_sequential_btn = ui.add_sized(
                                                [190.0, 40.0],
                                                egui::Button::new(
                                                    egui::RichText::new("🧲 Start Sequential")
                                                        .font(egui::FontId::proportional(14.0))
                                                        .strong(),
                                                )
                                                .fill(egui::Color32::from_rgb(0, 160, 80)),
                                            );
                                            if start_sequential_btn.clicked() {
                                                start_mode = Some(true);
                                            }
                                        });
                                        if let Some(sequential_mode) = start_mode {
                                            let url_to_play = opt.url.clone();
                                            let hash_clone = hash.clone();
                                            let title_for_launch = title.clone();
                                            let dest_dir = local_cache_path.clone();
                                            let size_hint = parse_size_to_bytes(&opt.size);
                                            let children_clone = self.spawned_children.clone();
                                            let torrent_status_clone = self.torrent_status.clone();
                                            let ctx_clone = ctx.clone();

                                            thread::spawn(move || {
                                                let mut children = children_clone.lock().unwrap();
                                                let dest = dest_dir.to_string_lossy().to_string();
                                                let local_torrent_path = dest_dir
                                                    .join("movie.torrent")
                                                    .to_string_lossy()
                                                    .to_string();
                                                let torrent_source = launch_source_from_url_or_hash(
                                                    &url_to_play,
                                                    &hash_clone,
                                                    &title_for_launch,
                                                    &local_torrent_path,
                                                );

                                                let _ = fs::create_dir_all(&dest_dir);
                                                initialize_torrent_runtime_state(
                                                    &torrent_status_clone,
                                                    &hash_clone,
                                                    &dest_dir,
                                                    size_hint,
                                                    sequential_mode,
                                                );
                                                ctx_clone.request_repaint();

                                                let mut cmd = build_libtorrent_worker_command(
                                                    torrent_source.as_str(),
                                                    dest.as_str(),
                                                    title_for_launch.as_str(),
                                                    sequential_mode,
                                                    sequential_mode.then_some(20),
                                                );

			                                                match cmd.spawn() {
			                                                    Ok(mut child) => {
	                                                        let _child_pid = child.id();
		                                                        if let Some(stderr) = child.stderr.take() {
			                                                            spawn_torrent_client_output_reader(
			                                                                stderr,
			                                                                "LIBTORRENT ERR",
			                                                                torrent_status_clone.clone(),
			                                                                hash_clone.clone(),
			                                                                dest_dir.clone(),
			                                                                size_hint,
			                                                                ctx_clone.clone(),
			                                                            );
			                                                        }
			                                                        if let Some(stdout) = child.stdout.take() {
			                                                            spawn_torrent_client_output_reader(
			                                                                stdout,
			                                                                "LIBTORRENT",
			                                                                torrent_status_clone.clone(),
			                                                                hash_clone.clone(),
			                                                                dest_dir.clone(),
			                                                                size_hint,
			                                                                ctx_clone.clone(),
			                                                            );
		                                                        }
	                                                        children.push(child);
	                                                    }
	                                                    Err(err) => {
	                                                        let mut map = torrent_status_clone.lock().unwrap();
	                                                        if let Some(s) = map.get_mut(&hash_clone) {
	                                                            s.active = false;
	                                                            s.speed = "Error".to_string();
	                                                            s.peers = err.to_string();
	                                                        }
	                                                        ctx_clone.request_repaint();
	                                                    }
		                                                }
		                                            });
	                                            self.status_message = if sequential_mode {
                                                    format!(
                                                        "Starting {} sequential torrent download...",
                                                        opt.quality
                                                    )
                                                } else {
                                                    format!(
                                                        "Starting {} normal torrent download...",
                                                        opt.quality
                                                    )
                                                };
                                        }
                                    }

                                    let play_enabled = local_media.is_some();
                                    ui.add_enabled_ui(play_enabled, |ui| {
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
                                            if let Some(ref media_path) = local_media {
                                                let progress_file = resolve_playback_progress_file(
                                                    &local_cache_path,
                                                    media_path,
                                                );
                                                match launch_mpv(
                                                    &self.spawned_children,
                                                    media_path,
                                                    &progress_file,
                                                ) {
                                                    Ok(()) => {
                                                        self.status_message = format!(
                                                            "Opened {} local file in mpv.",
                                                            opt.quality
                                                        );
                                                    }
                                                    Err(err) => {
                                                        self.status_message = format!(
                                                            "Failed to open {} local file in mpv: {}",
                                                            opt.quality, err
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    });

                                    let delete_target = local_media.clone().or_else(|| {
                                        find_largest_payload_file_excluding(
                                            &local_cache_path,
                                            &excluded_subdirs,
                                        )
                                    });
                                    ui.add_enabled_ui(delete_target.is_some(), |ui| {
                                        let delete_file_btn = ui.add_sized(
                                            [150.0, 40.0],
                                            egui::Button::new("🗑 Delete File")
                                                .fill(egui::Color32::from_rgb(145, 70, 35)),
                                        );
                                        if delete_file_btn.clicked() {
                                            if let Some(ref payload_path) = delete_target {
                                                self.pending_delete_file =
                                                    Some((local_cache_path.clone(), payload_path.clone()));
                                            }
                                        }
                                    });
                                });
                            });
                        });
                        ui.add_space(8.0);
                    }
                }
            }
        }
 
        if let Some(torrent) = primary_torrent.clone() {
            let parent_cache_path = get_cache_dir().join(&torrent.dir_name);

            ui.separator();
            ui.add_space(8.0);
            let delete_btn = ui.add_sized(
                [220.0, 40.0],
                egui::Button::new("🗑 Delete Film Library Item")
                    .fill(egui::Color32::from_rgb(180, 40, 40)),
            );
            if delete_btn.clicked() {
                self.pending_delete_dir = Some(parent_cache_path);
            }
        }
    }
}

fn main() -> eframe::Result<()> {
    // Clean up any stale background instances at startup
    kill_managed_torrent_processes();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Film Torrent Streaming Manager")
            .with_inner_size([1100.0, 750.0]),
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
        kill_managed_torrent_processes();
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
