#!/usr/bin/env python3

import argparse
import json
import mimetypes
import os
import shutil
import subprocess
import threading
import time
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import unquote


POLL_INTERVAL_SECONDS = 0.25
READ_CHUNK_BYTES = 512 * 1024
URGENT_MARGIN_BYTES = 8 * 1024 * 1024
HLS_SEGMENT_SECONDS = 4


def load_progress(progress_path: Path) -> dict:
    try:
        return json.loads(progress_path.read_text())
    except Exception:
        return {}


def normalized_ranges(progress: dict, total_bytes: int) -> list[tuple[int, int]]:
    raw_ranges = progress.get("ranges")
    ranges: list[tuple[int, int]] = []
    if isinstance(raw_ranges, list):
        for item in raw_ranges:
            if (
                isinstance(item, list)
                and len(item) == 2
                and isinstance(item[0], int)
                and isinstance(item[1], int)
            ):
                start = max(0, min(item[0], total_bytes))
                end = max(start, min(item[1], total_bytes))
                if end > start:
                    ranges.append((start, end))
    if ranges:
        return ranges
    downloaded = progress.get("downloaded_bytes")
    if isinstance(downloaded, int) and downloaded > 0:
        end = max(0, min(downloaded, total_bytes))
        return [(0, end)]
    if progress.get("complete") is True:
        return [(0, total_bytes)]
    return []


def available_end_for_offset(progress_path: Path, total_bytes: int, offset: int) -> int:
    progress = load_progress(progress_path)
    for start, end in normalized_ranges(progress, total_bytes):
        if start <= offset < end:
            return end
    return offset


def contiguous_prefix_end(progress_path: Path, total_bytes: int) -> int:
    progress = load_progress(progress_path)
    for start, end in normalized_ranges(progress, total_bytes):
        if start == 0:
            return end
        if start > 0:
            break
    return 0


def parse_range_header(range_header: str | None, total_bytes: int) -> tuple[int, int, bool]:
    if not range_header:
        return 0, total_bytes - 1, False
    unit, _, value = range_header.partition("=")
    if unit.strip().lower() != "bytes" or not value:
        raise ValueError("unsupported range unit")
    first_range = value.split(",", 1)[0].strip()
    start_str, _, end_str = first_range.partition("-")
    if not start_str:
        suffix_len = int(end_str)
        if suffix_len <= 0:
            raise ValueError("invalid suffix range")
        start = max(0, total_bytes - suffix_len)
        end = total_bytes - 1
    else:
        start = int(start_str)
        end = int(end_str) if end_str else total_bytes - 1
    if start < 0 or end < start or start >= total_bytes:
        raise ValueError("range out of bounds")
    return start, min(end, total_bytes - 1), True


class TorrentStreamProxyServer(ThreadingHTTPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, server_address, handler_class, media_path: Path, progress_path: Path):
        super().__init__(server_address, handler_class)
        self.media_path = media_path
        self.progress_path = progress_path
        self.stream_state_path = progress_path.parent / ".stream_state.json"
        self.hls_dir = progress_path.parent / ".hls_stream"
        self.playlist_path = self.hls_dir / "playlist.m3u8"
        self.segment_pattern = self.hls_dir / "segment_%05d.ts"
        self.ffmpeg_log_path = self.hls_dir / "ffmpeg.log"
        self.ffmpeg_proc: subprocess.Popen | None = None
        self.player_safe_probe_proc: subprocess.Popen | None = None
        self.progress_lock = threading.Lock()
        self.total_bytes = max(0, media_path.stat().st_size)
        guessed_type, _ = mimetypes.guess_type(media_path.name)
        self.content_type = guessed_type or "application/octet-stream"

    def prepare_hls_dir(self):
        if self.hls_dir.exists():
            shutil.rmtree(self.hls_dir, ignore_errors=True)
        self.hls_dir.mkdir(parents=True, exist_ok=True)

    def ffmpeg_running(self) -> bool:
        return self.ffmpeg_proc is not None and self.ffmpeg_proc.poll() is None

    def wait_for_file(self, path: Path, timeout: float | None = None) -> bool:
        deadline = None if timeout is None else (time.time() + timeout)
        while True:
            if path.exists() and path.stat().st_size > 0:
                return True
            if self.ffmpeg_proc is not None and self.ffmpeg_proc.poll() is not None:
                return path.exists() and path.stat().st_size > 0
            if deadline is not None and time.time() >= deadline:
                return path.exists() and path.stat().st_size > 0
            time.sleep(POLL_INTERVAL_SECONDS)

    def start_ffmpeg_hls(self):
        def launcher():
            time.sleep(0.25)
            self.prepare_hls_dir()
            raw_stream_url = self.contiguous_stream_url()
            base_args = [
                "ffmpeg",
                "-hide_banner",
                "-loglevel",
                "warning",
                "-fflags",
                "+genpts+discardcorrupt",
                "-err_detect",
                "ignore_err",
                "-seekable",
                "0",
                "-i",
                raw_stream_url,
                "-map",
                "0:v:0",
                "-map",
                "0:a?",
                "-sn",
                "-f",
                "hls",
                "-hls_time",
                str(HLS_SEGMENT_SECONDS),
                "-hls_list_size",
                "0",
                "-hls_playlist_type",
                "event",
                "-start_number",
                "0",
                "-hls_segment_filename",
                str(self.segment_pattern),
                "-hls_flags",
                "append_list+independent_segments+temp_file",
            ]
            copy_args = base_args + [
                "-c",
                "copy",
                str(self.playlist_path),
            ]
            transcode_args = base_args + [
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-tune",
                "zerolatency",
                "-g",
                "48",
                "-sc_threshold",
                "0",
                "-c:a",
                "aac",
                "-b:a",
                "160k",
                str(self.playlist_path),
            ]
            log_file = self.ffmpeg_log_path.open("ab")
            proc = subprocess.Popen(
                copy_args,
                stdout=log_file,
                stderr=log_file,
            )
            self.ffmpeg_proc = proc
            time.sleep(1.0)
            if proc.poll() is None:
                return
            proc = subprocess.Popen(
                transcode_args,
                stdout=log_file,
                stderr=log_file,
            )
            self.ffmpeg_proc = proc

        threading.Thread(target=launcher, daemon=True).start()

    def stop_ffmpeg(self):
        if self.ffmpeg_proc is None:
            return
        if self.ffmpeg_proc.poll() is None:
            try:
                self.ffmpeg_proc.terminate()
                self.ffmpeg_proc.wait(timeout=2)
            except Exception:
                try:
                    self.ffmpeg_proc.kill()
                except Exception:
                    pass
        self.ffmpeg_proc = None

    def contiguous_stream_url(self) -> str:
        media_ext = self.media_path.suffix.lower()
        contiguous_route = (
            f"/contiguous-stream{media_ext}" if media_ext else "/contiguous-stream"
        )
        return f"http://127.0.0.1:{self.server_address[1]}{contiguous_route}"

    def update_progress_fields(self, fields: dict):
        with self.progress_lock:
            progress = load_progress(self.progress_path)
            if "player_safe_seconds" in fields:
                existing = progress.get("player_safe_seconds")
                if isinstance(existing, (int, float)):
                    fields["player_safe_seconds"] = max(
                        float(existing),
                        float(fields["player_safe_seconds"]),
                    )
            progress.update(fields)
            tmp_path = self.progress_path.with_suffix(self.progress_path.suffix + ".tmp")
            try:
                tmp_path.write_text(json.dumps(progress, ensure_ascii=True))
                tmp_path.replace(self.progress_path)
            except Exception:
                try:
                    tmp_path.unlink(missing_ok=True)
                except Exception:
                    pass

    def start_player_safe_probe(self):
        def launcher():
            time.sleep(0.5)
            stream_url = self.contiguous_stream_url()
            probe_log_path = self.progress_path.parent / ".player_safe_probe.log"
            cached_progress = load_progress(self.progress_path)
            cached_safe_seconds = cached_progress.get("player_safe_seconds")
            resume_seconds = (
                max(0.0, float(cached_safe_seconds) - 5.0)
                if isinstance(cached_safe_seconds, (int, float))
                else 0.0
            )
            args = [
                "ffmpeg",
                "-hide_banner",
                "-nostdin",
                "-loglevel",
                "error",
            ]
            if resume_seconds > 0:
                args.extend(["-ss", f"{resume_seconds:.3f}"])
            args.extend([
                "-i",
                stream_url,
                "-map",
                "0:v:0",
                "-an",
                "-sn",
                "-f",
                "null",
                "-",
                "-progress",
                "pipe:1",
            ])
            log_file = probe_log_path.open("ab")
            proc = subprocess.Popen(
                args,
                stdout=subprocess.PIPE,
                stderr=log_file,
                text=True,
                bufsize=1,
            )
            self.player_safe_probe_proc = proc
            last_written_seconds = -1.0
            if proc.stdout is None:
                return
            for line in proc.stdout:
                key, _, value = line.strip().partition("=")
                if key == "out_time_ms":
                    try:
                        seconds = resume_seconds + max(0.0, int(value) / 1_000_000.0)
                    except ValueError:
                        continue
                elif key == "out_time":
                    parts = value.split(":")
                    if len(parts) != 3:
                        continue
                    try:
                        seconds = resume_seconds + (
                            int(parts[0]) * 3600
                            + int(parts[1]) * 60
                            + float(parts[2])
                        )
                    except ValueError:
                        continue
                else:
                    continue
                if seconds >= last_written_seconds + 1.0:
                    self.update_progress_fields(
                        {
                            "player_safe_seconds": seconds,
                            "player_safe_source": "ffmpeg_decode_probe",
                            "player_safe_updated_at": time.time(),
                        }
                    )
                    last_written_seconds = seconds

        threading.Thread(target=launcher, daemon=True).start()

    def stop_player_safe_probe(self):
        if self.player_safe_probe_proc is None:
            return
        if self.player_safe_probe_proc.poll() is None:
            try:
                self.player_safe_probe_proc.terminate()
                self.player_safe_probe_proc.wait(timeout=2)
            except Exception:
                try:
                    self.player_safe_probe_proc.kill()
                except Exception:
                    pass
        self.player_safe_probe_proc = None


class TorrentStreamHandler(BaseHTTPRequestHandler):
    server: TorrentStreamProxyServer

    def write_stream_state(self, position: int, available_end: int):
        remaining_ready = max(0, available_end - position)
        urgent_offset = None
        if remaining_ready <= URGENT_MARGIN_BYTES:
            urgent_offset = available_end
        payload = {
            "playback_active": True,
            "current_offset": position,
            "available_end": available_end,
            "urgent_offset": urgent_offset,
            "updated_at": time.time(),
        }
        try:
            self.server.stream_state_path.write_text(json.dumps(payload, ensure_ascii=True))
        except Exception:
            pass

    def do_HEAD(self):
        self._handle_request(send_body=False)

    def do_GET(self):
        self._handle_request(send_body=True)

    def log_message(self, format, *args):
        return

    def _handle_request(self, send_body: bool):
        if self.path == "/health":
            self.send_response(HTTPStatus.OK)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", "2")
            self.end_headers()
            if send_body:
                self.wfile.write(b"ok")
            return
        route = self.path.split("?", 1)[0]
        is_contiguous_stream = route == "/contiguous-stream" or route.startswith("/contiguous-stream.")
        if route == "/playlist.m3u8":
            if not self.server.wait_for_file(self.server.playlist_path, timeout=30):
                self.send_error(HTTPStatus.SERVICE_UNAVAILABLE, "playlist not ready")
                return
            self._serve_file(self.server.playlist_path, "application/vnd.apple.mpegurl", send_body)
            return
        if (
            route.startswith("/segments/")
            or route.endswith(".ts")
            or route.endswith(".m4s")
            or (route.endswith(".mp4") and not is_contiguous_stream)
        ):
            segment_name = (
                Path(unquote(route.removeprefix("/segments/"))).name
                if route.startswith("/segments/")
                else Path(unquote(route.lstrip("/"))).name
            )
            segment_path = self.server.hls_dir / segment_name
            if not self.server.wait_for_file(segment_path, timeout=None):
                self.send_error(HTTPStatus.SERVICE_UNAVAILABLE, "segment not ready")
                return
            self._serve_file(segment_path, "video/mp2t", send_body)
            return
        if route != "/stream" and not is_contiguous_stream:
            self.send_error(HTTPStatus.NOT_FOUND)
            return
        total_bytes = self.server.total_bytes
        if total_bytes <= 0:
            self.send_error(HTTPStatus.SERVICE_UNAVAILABLE, "media file is empty")
            return
        if is_contiguous_stream:
            available_prefix = contiguous_prefix_end(
                self.server.progress_path,
                total_bytes,
            )
            if available_prefix <= 0:
                self.send_error(HTTPStatus.SERVICE_UNAVAILABLE, "no contiguous media bytes available")
                return
            range_header = self.headers.get("Range")
            if range_header:
                try:
                    start, end, is_partial = parse_range_header(range_header, total_bytes)
                except ValueError:
                    self.send_response(HTTPStatus.REQUESTED_RANGE_NOT_SATISFIABLE)
                    self.send_header("Content-Range", f"bytes */{total_bytes}")
                    self.end_headers()
                    return
                if start >= available_prefix:
                    self.send_response(HTTPStatus.REQUESTED_RANGE_NOT_SATISFIABLE)
                    self.send_header("Content-Range", f"bytes */{total_bytes}")
                    self.end_headers()
                    return
                end = min(end, available_prefix - 1)
            else:
                start, end, is_partial = 0, available_prefix - 1, False
        else:
            try:
                start, end, is_partial = parse_range_header(
                    self.headers.get("Range"),
                    total_bytes,
                )
            except ValueError:
                self.send_response(HTTPStatus.REQUESTED_RANGE_NOT_SATISFIABLE)
                self.send_header("Content-Range", f"bytes */{total_bytes}")
                self.end_headers()
                return

        content_length = (end - start) + 1
        status = HTTPStatus.PARTIAL_CONTENT if is_partial else HTTPStatus.OK
        self.send_response(status)
        self.send_header("Content-Type", self.server.content_type)
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Length", str(content_length))
        if is_partial:
            self.send_header("Content-Range", f"bytes {start}-{end}/{total_bytes}")
        self.end_headers()

        if not send_body:
            return

        position = start
        with self.server.media_path.open("rb") as media_file:
            while position <= end:
                if is_contiguous_stream:
                    available_end = contiguous_prefix_end(
                        self.server.progress_path,
                        total_bytes,
                    )
                else:
                    available_end = available_end_for_offset(
                        self.server.progress_path,
                        total_bytes,
                        position,
                    )
                self.write_stream_state(position, available_end)
                if available_end <= position:
                    time.sleep(POLL_INTERVAL_SECONDS)
                    continue
                next_position = min(end + 1, available_end, position + READ_CHUNK_BYTES)
                media_file.seek(position)
                payload = media_file.read(next_position - position)
                if not payload:
                    time.sleep(POLL_INTERVAL_SECONDS)
                    continue
                try:
                    self.wfile.write(payload)
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    return
                position += len(payload)

    def _serve_file(self, path: Path, content_type: str, send_body: bool):
        file_size = path.stat().st_size
        self.send_response(HTTPStatus.OK)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(file_size))
        self.end_headers()
        if not send_body:
            return
        with path.open("rb") as fh:
            while True:
                chunk = fh.read(READ_CHUNK_BYTES)
                if not chunk:
                    break
                try:
                    self.wfile.write(chunk)
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    return


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--media-path", required=True)
    parser.add_argument("--progress-file", required=True)
    parser.add_argument("--port", required=True, type=int)
    args = parser.parse_args()

    media_path = Path(args.media_path).resolve()
    progress_path = Path(args.progress_file).resolve()
    if not media_path.exists():
        raise SystemExit(f"media file not found: {media_path}")

    server = TorrentStreamProxyServer(
        ("127.0.0.1", args.port),
        TorrentStreamHandler,
        media_path=media_path,
        progress_path=progress_path,
    )
    try:
        server.start_player_safe_probe()
        server.serve_forever(poll_interval=0.25)
    finally:
        server.stop_player_safe_probe()
        server.stop_ffmpeg()
        server.server_close()


if __name__ == "__main__":
    main()
