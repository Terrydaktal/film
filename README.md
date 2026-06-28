# Film Cache Streaming Proxy & GUI Manager

A local caching streaming proxy and player manager designed to prefetch, play, and cache films dynamically with fallbacks and seek-management.

## Project Structure
```text
.
├── stream_proxy.py       # Python FastAPI/Playwright HLS cache proxy
├── src/
│   └── main.rs           # Rust graphical Cache Manager UI
├── Cargo.toml            # Rust manager crate dependencies
├── pyproject.toml        # uv python environment settings
└── README.md             # Project documentation (this file)
```

---

## 1. Streaming Proxy (`stream_proxy.py`)
This script acts as the background proxy server that intercepts segment requests, caches them locally, and serves them to `mpv`.

### Functions
* **Auto-Extraction**: Sequencing clicks through available mirrors (UpCloud, Vidking, etc.) on the target page in a headless browser to capture active stream `.m3u8` playlists and request headers.
* **Intelligent Prefetching**: Downloads forward segments concurrently ahead of the playback playhead.
* **Seek Support**: Cancels current prefetchers on player seek, immediately re-anchors forward buffering, and backfills the skipped gap in the background.
* **Dynamic Fallback**: If a segment download fails from the primary CDN, it falls back to download the corresponding segment from alternate servers on the fly.
* **Local Player Integration**: Spawns `mpv` configured with massive demuxer caches targeting the local proxy.

#### CLI Usage
```bash
uv run python stream_proxy.py "<MOVIE_PAGE_URL>"
```

---

## 2. GUI Manager App (`src/main.rs`)
A desktop GUI dashboard built in Rust using `eframe` (`egui`) to list, delete, and control movie caches.

### Features
* **Visual Dashboard**: Displays all cached movies in a sleek dark mode layout.
* **Progress Bars**: Shows visual percentages and segment ratios of cache coverage.
* **Resuming**: Allows clicking on any cached movie to immediately launch `stream_proxy.py` and resume playback from the local disk cache.
* **Stream Launcher**: Input field at the bottom to paste a new URL slug and initialize a new stream.
* **Disk Cleaner**: Clears space by deleting individual caches or cleaning the entire directory.

#### Compilation & Launch
```bash
# Build & Run the GUI
cargo run --release
```
The compiled binary will be available at:
`./target/release/manager`
