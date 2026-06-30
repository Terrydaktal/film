#!/usr/bin/env python3

import argparse
import json
import os
import signal
import sys
import time
from pathlib import Path

import libtorrent as lt


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
        if status.has_metadata:
            file_progress_totals = save_file_progress(handle, save_path)
            subtitle_indexes = english_subtitle_file_indexes(handle)
            if subtitle_indexes:
                files = handle.torrent_file().files()
                subtitles_complete = all(
                    handle.file_progress()[index] >= files.file_size(index)
                    for index in subtitle_indexes
                )
                if not subtitles_complete and not subtitle_priority_active:
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
