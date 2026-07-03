#!/usr/bin/env python3

import argparse
import json
import mimetypes
import os
import socketserver
import threading
import time
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


POLL_INTERVAL_SECONDS = 0.25
READ_CHUNK_BYTES = 512 * 1024
URGENT_MARGIN_BYTES = 8 * 1024 * 1024


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
        self.total_bytes = max(0, media_path.stat().st_size)
        guessed_type, _ = mimetypes.guess_type(media_path.name)
        self.content_type = guessed_type or "application/octet-stream"


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
        if self.path.split("?", 1)[0] != "/stream":
            self.send_error(HTTPStatus.NOT_FOUND)
            return
        total_bytes = self.server.total_bytes
        if total_bytes <= 0:
            self.send_error(HTTPStatus.SERVICE_UNAVAILABLE, "media file is empty")
            return
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
        server.serve_forever(poll_interval=0.25)
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
