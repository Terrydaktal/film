#!/usr/bin/env python3

import argparse
import json
import os
import signal
import sys
import time
from pathlib import Path

import libtorrent as lt

VIDEO_EXTENSIONS = {
    ".mp4",
    ".mkv",
    ".avi",
    ".m4v",
    ".mov",
    ".flv",
    ".webm",
}

DEFAULT_FILE_PRIORITY = 4
LOW_FILE_PRIORITY = 1
HIGH_FILE_PRIORITY = 7
DEFAULT_PIECE_PRIORITY = 4
HIGH_PIECE_PRIORITY = 7
PIECE_BLOCK_BYTES = 16 * 1024
STREAM_STATE_STALE_SECONDS = 3.0


def format_size(num_bytes: int) -> str:
    kb = 1024
    mb = kb * 1024
    gb = mb * 1024
    if num_bytes >= gb:
        return f"{num_bytes / gb:.2f} GB"
    if num_bytes >= mb:
        return f"{num_bytes / mb:.2f} MB"
    if num_bytes >= kb:
        return f"{num_bytes / kb:.2f} KB"
    return f"{num_bytes} B"


def format_rate(num_bytes: int) -> str:
    return f"{format_size(max(num_bytes, 0))}/s"


def emit(event: dict) -> None:
    sys.stdout.write(json.dumps(event, ensure_ascii=True) + "\n")
    sys.stdout.flush()


def save_torrent_metadata(handle: lt.torrent_handle, target_path: Path) -> None:
    try:
        if not handle.status().has_metadata:
            return
        torrent_info = handle.torrent_file()
        if torrent_info is None:
            return
        ct = lt.create_torrent(torrent_info)
        data = lt.bencode(ct.generate())
        target_path.write_bytes(data)
        emit({"event": "metadata_saved", "path": str(target_path)})
    except Exception as exc:
        emit({"event": "log", "level": "warn", "message": f"metadata save failed: {exc}"})


def get_file_progress_snapshot(handle: lt.torrent_handle):
    status = handle.status()
    if not status.has_metadata:
        return None
    torrent_info = handle.torrent_file()
    if torrent_info is None:
        return None
    files = torrent_info.files()
    progress = handle.file_progress()
    payload = []
    downloaded_total = 0
    overall_total = 0
    for index in range(files.num_files()):
        downloaded = int(progress[index]) if index < len(progress) else 0
        total = int(files.file_size(index))
        payload.append(
            {
                "path": files.file_path(index),
                "downloaded": downloaded,
                "total": total,
            }
        )
        downloaded_total += downloaded
        overall_total += total
    return payload, downloaded_total, overall_total


def save_file_progress(handle: lt.torrent_handle, save_path: Path):
    try:
        snapshot = get_file_progress_snapshot(handle)
        if snapshot is None:
            return None
        payload, downloaded_total, overall_total = snapshot
        (save_path / ".torrent_file_progress.json").write_text(
            json.dumps(payload, ensure_ascii=True)
        )
        return downloaded_total, overall_total
    except Exception as exc:
        emit({"event": "log", "level": "warn", "message": f"file progress save failed: {exc}"})
        return None


def english_subtitle_file_indexes(handle: lt.torrent_handle):
    torrent_info = handle.torrent_file()
    if torrent_info is None:
        return []
    files = torrent_info.files()
    matches = []
    for index in range(files.num_files()):
        path = files.file_path(index).lower()
        is_sub = path.endswith((".srt", ".ass", ".ssa", ".vtt", ".sub"))
        looks_english = any(
            marker in path
            for marker in (
                "/english",
                "\\english",
                "english.srt",
                ".eng.",
                ".eng_",
                ".eng-",
                ".eng.srt",
                "forced.eng",
            )
        )
        if is_sub and looks_english:
            matches.append(index)
    return matches


def primary_media_file_index(handle: lt.torrent_handle):
    torrent_info = handle.torrent_file()
    if torrent_info is None:
        return None
    files = torrent_info.files()
    largest_index = None
    largest_size = -1
    for index in range(files.num_files()):
        path = files.file_path(index).lower()
        suffix = Path(path).suffix
        if suffix not in VIDEO_EXTENSIONS:
            continue
        size = int(files.file_size(index))
        if size > largest_size:
            largest_size = size
            largest_index = index
    return largest_index


def apply_english_subtitle_priority(handle: lt.torrent_handle, prioritize_subs: bool):
    torrent_info = handle.torrent_file()
    if torrent_info is None:
        return
    files = torrent_info.files()
    subtitle_indexes = english_subtitle_file_indexes(handle)
    if not subtitle_indexes:
        return
    priorities = [4] * files.num_files()
    if prioritize_subs:
        priorities = [1] * files.num_files()
        for index in subtitle_indexes:
            priorities[index] = 7
    handle.prioritize_files(priorities)


def apply_primary_media_file_priority(handle: lt.torrent_handle, layout):
    torrent_info = handle.torrent_file()
    if torrent_info is None or layout is None:
        return False
    files = torrent_info.files()
    file_index = layout.get("file_index")
    if file_index is None or file_index < 0 or file_index >= files.num_files():
        return False
    try:
        priorities = [0] * files.num_files()
        priorities[file_index] = HIGH_FILE_PRIORITY
        handle.prioritize_files(priorities)
        return True
    except Exception:
        return False


def restore_default_file_priority(handle: lt.torrent_handle):
    torrent_info = handle.torrent_file()
    if torrent_info is None:
        return
    files = torrent_info.files()
    try:
        handle.prioritize_files([DEFAULT_FILE_PRIORITY] * files.num_files())
    except Exception:
        pass


def build_primary_media_layout(handle: lt.torrent_handle, target_bytes: int):
    torrent_info = handle.torrent_file()
    if torrent_info is None:
        return None
    files = torrent_info.files()
    file_index = primary_media_file_index(handle)
    if file_index is None or file_index < 0 or file_index >= files.num_files():
        return None
    file_offset = 0
    for index in range(file_index):
        file_offset += int(files.file_size(index))
    file_size = int(files.file_size(file_index))
    if file_size <= 0 or target_bytes <= 0:
        return None
    total_size = 0
    for index in range(files.num_files()):
        total_size += int(files.file_size(index))
    piece_length = int(torrent_info.piece_length())
    first_piece = file_offset // piece_length
    last_piece = (file_offset + file_size - 1) // piece_length
    piece_spans = []
    for piece in range(first_piece, last_piece + 1):
        piece_start = piece * piece_length
        piece_end = min(piece_start + piece_length, total_size)
        overlap_start = max(piece_start, file_offset)
        overlap_end = min(piece_end, file_offset + file_size)
        if overlap_start >= overlap_end:
            continue
        piece_spans.append(
            {
                "piece": piece,
                "file_rel_start": overlap_start - file_offset,
                "file_rel_end": overlap_end - file_offset,
                "piece_rel_start": overlap_start - piece_start,
                "piece_rel_end": overlap_end - piece_start,
                "piece_size": piece_end - piece_start,
            }
        )
    window_bytes = min(file_size, target_bytes)
    return {
        "file_index": file_index,
        "file_size": file_size,
        "startup_window_bytes": window_bytes,
        "piece_spans": piece_spans,
    }


def prioritized_pieces_for_window(layout, start_offset: int, window_bytes: int):
    if layout is None or window_bytes <= 0:
        return []
    window_start = max(0, start_offset)
    window_end = window_start + window_bytes
    pieces = []
    for span in layout.get("piece_spans", []):
        rel_start = int(span.get("file_rel_start", 0))
        rel_end = int(span.get("file_rel_end", 0))
        if rel_end <= window_start:
            continue
        if rel_start >= window_end:
            break
        pieces.append(int(span["piece"]))
    return pieces


def apply_priority_window(handle: lt.torrent_handle, layout, start_offset: int, window_bytes: int):
    torrent_info = handle.torrent_file()
    if torrent_info is None:
        return None
    if layout is None:
        return None
    priority_pieces = prioritized_pieces_for_window(layout, start_offset, window_bytes)
    if not priority_pieces:
        return None
    try:
        priorities = [0] * torrent_info.num_pieces()
        for deadline_index, piece in enumerate(priority_pieces):
            priorities[piece] = HIGH_PIECE_PRIORITY
            try:
                handle.set_piece_deadline(piece, deadline_index * 50)
            except Exception:
                pass
        handle.prioritize_pieces(priorities)
        return {
            "piece_count": len(priority_pieces),
            "window_bytes": window_bytes,
            "start_offset": start_offset,
            "first_piece": priority_pieces[0],
        }
    except Exception:
        return None


def clear_priority_window(handle: lt.torrent_handle):
    torrent_info = handle.torrent_file()
    if torrent_info is None:
        return
    try:
        for piece in range(torrent_info.num_pieces()):
            try:
                handle.reset_piece_deadline(piece)
            except Exception:
                pass
        handle.prioritize_pieces([DEFAULT_PIECE_PRIORITY] * torrent_info.num_pieces())
    except Exception:
        pass


def merge_ranges(ranges):
    if not ranges:
        return []
    ranges = sorted(ranges, key=lambda item: item[0])
    merged = [list(ranges[0])]
    for start, end in ranges[1:]:
        if start <= merged[-1][1]:
            merged[-1][1] = max(merged[-1][1], end)
        else:
            merged.append([start, end])
    return [(start, end) for start, end in merged]


def partial_piece_bitfield(entry, blocks_in_piece: int):
    finished = getattr(entry, "finished", None)
    if finished is None:
        return []
    bits = []
    for index in range(blocks_in_piece):
        try:
            bits.append(bool(finished[index]))
        except Exception:
            bits.append(False)
    return bits


def partial_piece_ranges(handle: lt.torrent_handle, layout):
    queue_ranges = []
    try:
        download_queue = handle.get_download_queue()
    except Exception:
        return queue_ranges
    piece_spans = {int(span["piece"]): span for span in layout.get("piece_spans", [])}
    for entry in download_queue:
        piece = getattr(entry, "piece_index", None)
        if piece is None:
            piece = getattr(entry, "piece", None)
        if piece is None:
            continue
        span = piece_spans.get(int(piece))
        if span is None:
            continue
        blocks_in_piece = int(getattr(entry, "blocks_in_piece", 0) or 0)
        if blocks_in_piece <= 0:
            continue
        finished_blocks = partial_piece_bitfield(entry, blocks_in_piece)
        if not any(finished_blocks):
            continue
        piece_rel_start = int(span["piece_rel_start"])
        piece_rel_end = int(span["piece_rel_end"])
        piece_size = int(span["piece_size"])
        file_rel_start = int(span["file_rel_start"])
        for block_index, block_finished in enumerate(finished_blocks):
            if not block_finished:
                continue
            block_start = block_index * PIECE_BLOCK_BYTES
            block_end = min(block_start + PIECE_BLOCK_BYTES, piece_size)
            overlap_start = max(block_start, piece_rel_start)
            overlap_end = min(block_end, piece_rel_end)
            if overlap_start >= overlap_end:
                continue
            file_start = file_rel_start + (overlap_start - piece_rel_start)
            file_end = file_rel_start + (overlap_end - piece_rel_start)
            queue_ranges.append((file_start, file_end))
    return queue_ranges


def read_stream_state(save_path: Path):
    state_path = save_path / ".stream_state.json"
    try:
        payload = json.loads(state_path.read_text())
    except Exception:
        return None
    if not isinstance(payload, dict):
        return None
    updated_at = payload.get("updated_at")
    if not isinstance(updated_at, (int, float)):
        return None
    if (time.time() - float(updated_at)) > STREAM_STATE_STALE_SECONDS:
        return None
    return payload


def write_piece_progress(handle: lt.torrent_handle, save_path: Path, layout):
    if layout is None:
        return None
    downloaded_ranges = []
    contiguous_prefix = 0
    verified_bytes = 0
    for span in layout.get("piece_spans", []):
        piece = int(span["piece"])
        rel_start = int(span["file_rel_start"])
        rel_end = int(span["file_rel_end"])
        try:
            have_piece = handle.have_piece(piece)
        except Exception:
            have_piece = False
        if have_piece:
            downloaded_ranges.append((rel_start, rel_end))
            verified_bytes += rel_end - rel_start
    downloaded_ranges.extend(partial_piece_ranges(handle, layout))
    merged_ranges = merge_ranges(downloaded_ranges)
    if merged_ranges and merged_ranges[0][0] == 0:
        contiguous_prefix = merged_ranges[0][1]
    startup_window_bytes = int(layout.get("startup_window_bytes", 0))
    startup_window_downloaded = min(contiguous_prefix, startup_window_bytes)
    startup_window_complete = (
        startup_window_bytes > 0 and contiguous_prefix >= startup_window_bytes
    )
    progress_path = save_path / ".torrent_progress.json"
    try:
        if progress_path.exists():
            progress_json = json.loads(progress_path.read_text())
            if not isinstance(progress_json, dict):
                progress_json = {}
        else:
            progress_json = {}
    except Exception:
        progress_json = {}
    progress_json.update(
        {
            "verifier_version": 4,
            "range_source": "libtorrent_live_chunks",
            "total_bytes": int(layout.get("file_size", 0)),
            "contiguous_prefix_bytes": contiguous_prefix,
            "piece_verified_bytes": verified_bytes,
            "startup_window_bytes": startup_window_bytes,
            "startup_window_downloaded_bytes": startup_window_downloaded,
            "startup_window_complete": startup_window_complete,
            "ranges": [[start, end] for start, end in merged_ranges],
        }
    )
    try:
        progress_path.write_text(json.dumps(progress_json, ensure_ascii=True))
    except Exception as exc:
        emit(
            {
                "event": "log",
                "level": "warn",
                "message": f"piece progress save failed: {exc}",
            }
        )
    return {
        "startup_window_downloaded": startup_window_downloaded,
        "startup_window_bytes": startup_window_bytes,
        "startup_window_complete": startup_window_complete,
        "contiguous_prefix_bytes": contiguous_prefix,
        "ranges": merged_ranges,
    }


def build_session() -> lt.session:
    ses = lt.session(
        {
            "enable_dht": True,
            "enable_lsd": True,
            "enable_upnp": True,
            "enable_natpmp": True,
            "alert_queue_size": 10000,
            "announce_to_all_tiers": True,
            "announce_to_all_trackers": True,
            "prefer_udp_trackers": True,
            "listen_interfaces": "0.0.0.0:0,[::]:0",
            "user_agent": "film-libtorrent-worker/1.0",
        }
    )
    try:
        ses.start_dht()
    except Exception:
        pass
    try:
        ses.start_upnp()
    except Exception:
        pass
    try:
        ses.start_natpmp()
    except Exception:
        pass
    try:
        ses.start_lsd()
    except Exception:
        pass
    return ses


def add_torrent(session: lt.session, source: str, save_path: Path, resume_path: Path) -> lt.torrent_handle:
    atp: dict = {
        "save_path": str(save_path),
        "storage_mode": lt.storage_mode_t.storage_mode_sparse,
    }
    if resume_path.exists():
        try:
            atp["resume_data"] = lt.read_resume_data(resume_path.read_bytes())
        except Exception:
            pass
    if source.startswith("magnet:"):
        params = lt.parse_magnet_uri(source)
        params.save_path = str(save_path)
        if "resume_data" in atp:
            params.resume_data = atp["resume_data"]
        return session.add_torrent(params)
    source_path = Path(source)
    if source_path.exists():
        atp["ti"] = lt.torrent_info(str(source_path))
        return session.add_torrent(atp)
    raise RuntimeError(f"unsupported torrent source: {source}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", required=True)
    parser.add_argument("--save-path", required=True)
    parser.add_argument("--display-name", required=True)
    parser.add_argument("--sequential", action="store_true")
    parser.add_argument("--sequential-start-mib", type=float, default=0.0)
    args = parser.parse_args()

    save_path = Path(args.save_path)
    save_path.mkdir(parents=True, exist_ok=True)
    torrent_path = save_path / "movie.torrent"
    resume_path = save_path / "torrent.fastresume"

    session = build_session()
    handle = add_torrent(session, args.source, save_path, resume_path)
    if args.sequential:
        try:
            handle.set_sequential_download(True)
        except Exception:
            pass

    stop_requested = False
    last_status_key = None
    last_detail = ""
    metadata_saved = torrent_path.exists()
    startup_detail = ""
    tracker_dns_failures = 0
    tracker_http_failures = 0
    tracker_connection_failures = 0
    port_forwarding_unavailable = 0
    subtitle_priority_active = False
    primary_media_layout = None
    hard_priority_active = False
    hard_priority_piece = None

    def request_stop(signum, frame):
        nonlocal stop_requested
        stop_requested = True

    signal.signal(signal.SIGTERM, request_stop)
    signal.signal(signal.SIGINT, request_stop)

    emit({"event": "log", "level": "info", "message": f"libtorrent {lt.version}"})

    while not stop_requested:
        try:
            alerts = session.pop_alerts()
        except Exception:
            alerts = []

        for alert in alerts:
            name = type(alert).__name__
            message = alert.message()
            lower = message.lower()

            if name == "metadata_received_alert" or "metadata" in lower:
                if not metadata_saved:
                    save_torrent_metadata(handle, torrent_path)
                    metadata_saved = torrent_path.exists()
                startup_detail = ""

            if "couldn't look up" in lower:
                tracker_dns_failures += 1
                startup_detail = "tracker DNS failures"
            elif "http response 521" in lower:
                tracker_http_failures += 1
                startup_detail = "tracker HTTP failures"
            elif "could not connect to tracker" in lower or "connection failed" in lower:
                tracker_connection_failures += 1
                startup_detail = "tracker connection failures"
            elif "not forwarded" in lower or "port-forwarding" in lower:
                port_forwarding_unavailable += 1
                if not startup_detail:
                    startup_detail = "port forwarding unavailable"

            if name in {"save_resume_data_alert", "save_resume_data_failed_alert"}:
                try:
                    if hasattr(alert, "params"):
                        resume_path.write_bytes(lt.write_resume_data_buf(alert.params))
                except Exception:
                    pass

        status = handle.status()

        if status.has_metadata and not metadata_saved:
            save_torrent_metadata(handle, torrent_path)
            metadata_saved = torrent_path.exists()

        file_progress_totals = None
        progress_state = None
        subtitle_indexes = []
        subtitles_complete = False
        if status.has_metadata:
            if args.sequential and args.sequential_start_mib > 0 and primary_media_layout is None:
                primary_media_layout = build_primary_media_layout(
                    handle,
                    int(args.sequential_start_mib * 1024 * 1024),
                )
            file_progress_totals = save_file_progress(handle, save_path)
            if primary_media_layout is not None:
                progress_state = write_piece_progress(
                    handle,
                    save_path,
                    primary_media_layout,
                )
            stream_state = read_stream_state(save_path) if args.sequential else None
            playback_active = bool(stream_state and stream_state.get("playback_active"))
            target_offset = None
            if (
                args.sequential
                and primary_media_layout is not None
                and args.sequential_start_mib > 0
                and progress_state is not None
            ):
                if playback_active:
                    urgent_offset = stream_state.get("urgent_offset") if stream_state else None
                    if isinstance(urgent_offset, (int, float)) and urgent_offset >= 0:
                        target_offset = int(urgent_offset)
                else:
                    target_offset = int(progress_state.get("contiguous_prefix_bytes", 0))
                if target_offset is not None:
                    window = apply_priority_window(
                        handle,
                        primary_media_layout,
                        target_offset,
                        int(args.sequential_start_mib * 1024 * 1024),
                    )
                    if window is not None:
                        current_piece = window.get("first_piece")
                        if not hard_priority_active or current_piece != hard_priority_piece:
                            hard_priority_piece = current_piece
                        hard_priority_active = True
                        apply_primary_media_file_priority(handle, primary_media_layout)
                    elif hard_priority_active:
                        clear_priority_window(handle)
                        restore_default_file_priority(handle)
                        hard_priority_active = False
                        hard_priority_piece = None
                elif hard_priority_active:
                    clear_priority_window(handle)
                    restore_default_file_priority(handle)
                    hard_priority_active = False
                    hard_priority_piece = None
            subtitle_indexes = english_subtitle_file_indexes(handle)
            if subtitle_indexes:
                files = handle.torrent_file().files()
                subtitles_complete = all(
                    handle.file_progress()[index] >= files.file_size(index)
                    for index in subtitle_indexes
                )
                if hard_priority_active:
                    subtitle_priority_active = False
                elif not subtitles_complete and not subtitle_priority_active:
                    apply_english_subtitle_priority(handle, True)
                    subtitle_priority_active = True
                elif subtitles_complete and subtitle_priority_active:
                    apply_english_subtitle_priority(handle, False)
                    subtitle_priority_active = False

        if file_progress_totals is not None:
            downloaded, total = file_progress_totals
        else:
            total = status.total_wanted or status.total_wanted_done or 0
            downloaded = min(status.total_wanted_done, total) if total else status.total_wanted_done

        if total and status.is_seeding:
            speed = "Complete"
            detail = ""
            complete = True
        elif not status.has_metadata:
            speed = "Fetching metadata"
            detail = startup_detail
            complete = False
        elif status.num_peers <= 0:
            speed = "Waiting for peers..."
            detail = startup_detail
            complete = False
        elif status.download_rate <= 0:
            speed = "Waiting for data..."
            detail = startup_detail
            complete = False
        else:
            speed = format_rate(status.download_rate)
            detail = ""
            complete = False

        peers_text = "connecting" if status.num_peers <= 0 else f"{status.num_peers} peers"
        downloaded_text = (
            f"{format_size(downloaded)}/{format_size(total)}"
            if total
            else format_size(downloaded)
        )
        status_key = (downloaded_text, speed, peers_text, detail, complete)
        if status_key != last_status_key or detail != last_detail:
            emit(
                {
                    "event": "status",
                    "downloaded": downloaded_text,
                    "speed": speed,
                    "peers": peers_text,
                    "detail": detail,
                    "complete": complete,
                }
            )
            last_status_key = status_key
            last_detail = detail

        if complete:
            break

        time.sleep(0.5)

    try:
        handle.save_resume_data()
        deadline = time.time() + 5.0
        while time.time() < deadline:
            for alert in session.pop_alerts():
                if type(alert).__name__ == "save_resume_data_alert":
                    try:
                        resume_path.write_bytes(lt.write_resume_data_buf(alert.params))
                    except Exception:
                        pass
                    raise StopIteration
            time.sleep(0.1)
    except StopIteration:
        pass
    except Exception:
        pass

    if handle.status().has_metadata and not torrent_path.exists():
        save_torrent_metadata(handle, torrent_path)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
