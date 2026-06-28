import asyncio
import os
import re
import sys
import time
import urllib.parse
import aiohttp
import uvicorn
from fastapi import FastAPI, Query, HTTPException
from fastapi.responses import Response, PlainTextResponse
import subprocess
from playwright.async_api import async_playwright

from contextlib import asynccontextmanager

# State management
state = {
    "page_url": "",
    "sources_dict": {},       # server_name -> original m3u8 URL
    "sources": {},            # server_name -> list of segment URLs
    "source_headers": {},     # server_name -> captured request headers
    "source_order": [],       # Order of servers to try
    "source_status": {},      # server_name -> "Online" / "Offline" / "Checking"
    "cache": {},             # segment_index -> local_path
    "current_index": 0,      # Last index requested by the player
    "prefetch_task": None,   # Active asyncio task for forward prefetching
    "backfill_task": None,   # Active asyncio task for gap filling
    "download_events": {},   # segment_index -> asyncio.Event (active download coordinators)
    "real_sources_resolved": None, # Will be set to asyncio.Event in lifespan
    "scrape_lock": None,     # Will be set to asyncio.Lock in lifespan
    "sources_version": 0,    # Incremented on every successful Playwright scrape
    "download_semaphore": None, # Will be set to asyncio.Semaphore(3) in lifespan
    "session": None,
    "cache_dir": "./stream_cache"
}

def get_safe_movie_name(url: str) -> str:
    """Extracts a filesystem-safe directory name from the movie page URL."""
    parsed = urllib.parse.urlparse(url)
    path = parsed.path.strip("/")
    # Get last parts of path, e.g. "movie/apex-02351" -> "apex-02351"
    parts = [p for p in path.split("/") if p]
    if parts:
        return parts[-1]
    return "unknown_movie"

async def check_source_health(name: str):
    """Performs a quick HTTP GET check on the first segment URL to determine if tokens are still valid."""
    state["source_status"][name] = "Checking"
    
    # Get the first segment URL for this server to test actual segment availability
    segment_list = state["sources"].get(name, [])
    if not segment_list or not segment_list[0]:
        # If we don't have segments resolved yet, fall back to checking the playlist URL
        playlist_url = state["sources_dict"].get(name)
        if not playlist_url:
            state["source_status"][name] = "Offline"
            return
        url = playlist_url
    else:
        url = segment_list[0]
        
    session = await get_session()
    headers = state["source_headers"].get(name, {
        "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
    })
    try:
        # Request with a short timeout. A 200 OK means the segment token is still valid.
        async with session.get(url, headers=headers, timeout=3) as resp:
            if resp.status == 200:
                state["source_status"][name] = "Online"
            else:
                state["source_status"][name] = "Offline"
    except Exception:
        state["source_status"][name] = "Offline"

async def check_all_sources_health():
    """Checks health for all captured source playlists/segments."""
    tasks = []
    for name in state["sources_dict"].keys():
        tasks.append(check_source_health(name))
    if tasks:
        await asyncio.gather(*tasks)


def scan_existing_cache():
    """Scans the movie's cache directory for already downloaded segments and loads metadata."""
    os.makedirs(state["cache_dir"], exist_ok=True)
    for filename in os.listdir(state["cache_dir"]):
        match = re.match(r"^segment_(\d+)\.ts$", filename)
        if match:
            idx = int(match.group(1))
            file_path = os.path.join(state["cache_dir"], filename)
            state["cache"][idx] = file_path
            
    # Load metadata to allow immediate player launch and fallback source loading
    import json
    metadata_path = os.path.join(state["cache_dir"], "metadata.json")
    if os.path.exists(metadata_path):
        try:
            with open(metadata_path, "r") as f:
                meta = json.load(f)
                total_seg = meta.get("total_segments", 0)
                cached_sources = meta.get("sources")
                cached_order = meta.get("source_order")
                cached_sources_dict = meta.get("sources_dict")
                cached_headers = meta.get("source_headers")
                
                if cached_sources_dict:
                    state["sources_dict"] = cached_sources_dict
                if cached_headers:
                    sanitized_headers = {}
                    for srv_name, hdrs in cached_headers.items():
                        clean_hdrs = {}
                        for k, v in hdrs.items():
                            if k.lower() not in ["sec-ch-ua", "sec-ch-ua-mobile", "sec-ch-ua-platform"]:
                                clean_hdrs[k] = v
                        sanitized_headers[srv_name] = clean_hdrs
                    state["source_headers"] = sanitized_headers
                
                if total_seg > 0:
                    if cached_sources and cached_order:
                        state["sources"] = cached_sources
                        state["source_order"] = cached_order
                        state["real_sources_resolved"].set()
                        print(f"[CACHE] Pre-resolved sources list from cache metadata for instant playback fallback.")
                        # Check health in background
                        asyncio.create_task(check_all_sources_health())
                    else:
                        state["sources"] = { "Cache": [""] * total_seg }
                        state["source_order"] = ["Cache"]
                        
                    state["sources_resolved_event"].set()
                    print(f"[CACHE] Pre-resolved {total_seg} segments from metadata.json to load player immediately.")
        except Exception as e:
            print(f"Failed to pre-resolve metadata: {e}")
            
    if state["cache"]:
        print(f"[CACHE] Found {len(state['cache'])} existing cached segments. Resuming playback context.")

async def write_metadata(total_segments: int):
    """Writes a metadata.json file containing segment count and segment lists to the cache folder."""
    import json
    metadata_path = os.path.join(state["cache_dir"], "metadata.json")
    metadata = {
        "title": get_safe_movie_name(state["page_url"]),
        "url": state["page_url"],
        "total_segments": total_segments,
        "source_order": state["source_order"],
        "sources": state["sources"],
        "sources_dict": state["sources_dict"],
        "source_headers": state["source_headers"],
        "port": state.get("port", 8000)
    }
    try:
        with open(metadata_path, "w") as f:
            json.dump(metadata, f, indent=2)
    except Exception as e:
        print(f"Failed to write cache metadata: {e}")

async def get_session():
    if state["session"] is None or state["session"].closed:
        # High connection limit ensures player requests are never queued behind prefetchers
        connector = aiohttp.TCPConnector(limit=100)
        state["session"] = aiohttp.ClientSession(connector=connector)
    return state["session"]

async def extract_sources_selectively(page_url, target_servers=None):
    """Extracts stream sources, optionally targeting only offline servers."""
    captured_sources = {}
    
    async with async_playwright() as p:
        print(f"Launching Playwright to extract sources (selective: {target_servers})...")
        browser = await p.chromium.launch(
            headless=True,
            args=["--no-sandbox", "--disable-dev-shm-usage", "--disable-blink-features=AutomationControlled"]
        )
        context = await browser.new_context(
            user_agent="Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
        )
        
        # Optimize page load speed by blocking unnecessary heavy resources (images, css, fonts, ads, media)
        async def block_resources(route):
            req = route.request
            if req.resource_type in ["image", "stylesheet", "font", "media"]:
                await route.abort()
            else:
                await route.continue_()
                
        await context.route("**/*", block_resources)
        
        page = await context.new_page()

        active_server = None
        video_extensions = ('.m3u8',)

        def handle_response(response):
            nonlocal active_server
            try:
                res_url = response.url
                url_lower = res_url.lower()
                headers = response.headers
                content_type = headers.get("content-type", "").lower()
                
                is_m3u8 = any(url_lower.split('?')[0].endswith(ext) for ext in video_extensions) or "mpegurl" in content_type
                if is_m3u8 and active_server:
                    if active_server not in captured_sources:
                        print(f"[EXTRACTOR] Captured m3u8 for '{active_server}': {res_url}")
                        captured_sources[active_server] = res_url
                        
                        req_headers = response.request.headers
                        headers_to_save = {}
                        for k, v in req_headers.items():
                            if k.lower() in ["referer", "origin", "user-agent", "cookie"]:
                                headers_to_save[k] = v
                        state["source_headers"][active_server] = headers_to_save
            except Exception:
                pass

        page.on("response", handle_response)

        try:
            await page.goto(page_url, wait_until="domcontentloaded", timeout=30000)
            await page.wait_for_timeout(5000)

            # Find all clickables
            elements = await page.query_selector_all("a.sv-item, [data-srv], [data-id]")
            server_buttons = []
            seen = set()
            for elem in elements:
                try:
                    srv_name = await elem.get_attribute("data-srv")
                    data_id = await elem.get_attribute("data-id")
                    identifier = srv_name or data_id
                    if identifier and data_id and data_id.startswith("http") and identifier not in seen:
                        seen.add(identifier)
                        server_buttons.append((srv_name or "Unknown", elem))
                except Exception:
                    pass

            # Map the initial autoloaded stream if target_servers is not filtering it out
            first_server_name = server_buttons[0][0] if server_buttons else "Default"
            if "Default" in captured_sources:
                if target_servers is None or first_server_name in target_servers:
                    print(f"[EXTRACTOR] Mapping initial stream to first server: '{first_server_name}'")
                    captured_sources[first_server_name] = captured_sources.pop("Default")
                    if "Default" in state["source_headers"]:
                        state["source_headers"][first_server_name] = state["source_headers"].pop("Default")
                else:
                    captured_sources.pop("Default", None)

            # Click each server sequentially
            for name, btn in server_buttons:
                # If target_servers is specified, only click targets that are currently offline
                if target_servers is not None and name not in target_servers:
                    continue
                # Skip clicking if we already resolved it (e.g. initial load)
                if name in captured_sources:
                    continue
                    
                active_server = name
                print(f"[EXTRACTOR] Clicking server button: '{name}'")
                try:
                    await btn.evaluate("node => node.click()")
                    await page.wait_for_timeout(3000)
                except Exception as e:
                    print(f"Error clicking {name}: {e}")

        except Exception as e:
            print(f"Extraction error: {e}")
        finally:
            await browser.close()

    return captured_sources

async def background_extraction_and_resolution(target_servers=None, version_before=None):
    """Background task to extract and resolve streaming sources concurrently."""
    async with state["scrape_lock"]:
        try:
            # If another thread already completed a Playwright scrape while we were waiting in lock, abort!
            if version_before is not None and state["sources_version"] > version_before:
                print(f"[RESOLVER] Sources already refreshed (version {state['sources_version']} > {version_before}). Skipping redundant scrape.")
                return

            # If we already have cached sources_dict and are NOT selectively refreshing, test cache TTL first
            if state["sources_dict"] and target_servers is None:
                metadata_path = os.path.join(state["cache_dir"], "metadata.json")
                cache_is_fresh = False
                if os.path.exists(metadata_path):
                    mtime = os.path.getmtime(metadata_path)
                    age_seconds = time.time() - mtime
                    if age_seconds < 600:  # 10 minutes
                        cache_is_fresh = True
                        
                if cache_is_fresh:
                    print("[RESOLVER] Cached manifest links are fresh (<10 min old). Skipping Playwright scrape.")
                    resolved_sources = await resolve_playlists(state["sources_dict"])
                    if resolved_sources:
                        state["sources"] = resolved_sources
                        state["source_order"] = list(resolved_sources.keys())
                        state["real_sources_resolved"].set()
                        state["sources_resolved_event"].set()
                        return
                else:
                    print("[RESOLVER] Cached manifest links are expired/stale (>10 min old). Proceeding to rescrape with Playwright...")

            # 1. Extract sources (optionally selective)
            sources_dict = await extract_sources_selectively(state["page_url"], target_servers)
            if not sources_dict:
                print("Could not find any new streaming sources on page.")
                return
                
            # Update our sources dictionary
            state["sources_dict"].update(sources_dict)
            
            # 2. Resolve playlists to segments lists
            resolved_sources = await resolve_playlists(state["sources_dict"])
            if not resolved_sources:
                print("Could not resolve any playlists.")
                return
                
            # Update state with fresh sources
            state["sources"] = resolved_sources
            state["source_order"] = list(resolved_sources.keys())
            print(f"Active fallback sources hierarchy: {state['source_order']}")
            
            # Write/Update metadata info
            primary_source = state["source_order"][0]
            total_segments = len(state["sources"][primary_source])
            await write_metadata(total_segments)
            state["sources_version"] += 1
            
            # Run health checks
            await check_all_sources_health()
            
            # Mark as resolved to release any pending downloads
            state["real_sources_resolved"].set()
            state["sources_resolved_event"].set()
        except Exception as e:
            print(f"Background extraction/resolution failed: {e}")

@asynccontextmanager
async def lifespan(app: FastAPI):
    # Startup logic
    port = state.get("port", 8000)
    
    # Configure movie-specific cache directory
    movie_slug = get_safe_movie_name(state["page_url"])
    state["cache_dir"] = os.path.join("./stream_cache", movie_slug)
    os.makedirs(state["cache_dir"], exist_ok=True)
    
    state["sources_resolved_event"] = asyncio.Event()
    state["real_sources_resolved"] = asyncio.Event()
    state["scrape_lock"] = asyncio.Lock()
    state["download_semaphore"] = asyncio.Semaphore(3)
    
    # Scan any previously downloaded segments to support resuming
    scan_existing_cache()
    
    # 1. Launch mpv player immediately in background
    asyncio.create_task(run_mpv(port))
    
    # 2. Spawn extraction and resolution concurrently in the background
    asyncio.create_task(background_extraction_and_resolution())
    
    yield
    # Shutdown logic
    if state["session"] and not state["session"].closed:
        await state["session"].close()

app = FastAPI(lifespan=lifespan)


async def resolve_playlists(sources_dict):
    """Downloads all captured m3u8 files and resolves their segments list."""
    resolved = {}
    session = await get_session()
    
    for name, playlist_url in sources_dict.items():
        try:
            headers = state["source_headers"].get(name, {
                "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
            })
            async with session.get(playlist_url, headers=headers, timeout=15) as resp:
                if resp.status != 200:
                    print(f"[RESOLVER] Playlist download for '{name}' failed with HTTP status: {resp.status}")
                    continue
                content = await resp.text()
                
            parsed_playlist = urllib.parse.urlparse(playlist_url)
            base_url = playlist_url.rsplit('?', 1)[0].rsplit('/', 1)[0] + '/'
            lines = content.splitlines()
            segments = []
            
            for line in lines:
                line = line.strip()
                if not line or line.startswith("#"):
                    continue
                
                # Resolve relative URL
                if line.startswith("http"):
                    full_segment_url = line
                else:
                    full_segment_url = urllib.parse.urljoin(base_url, line)
                    
                # Append original query parameters if missing in the segment URL
                parsed_segment = urllib.parse.urlparse(full_segment_url)
                if not parsed_segment.query and parsed_playlist.query:
                    separator = "&" if "?" in full_segment_url else "?"
                    full_segment_url = f"{full_segment_url}{separator}{parsed_playlist.query}"
                    
                segments.append(full_segment_url)
                
            if segments:
                resolved[name] = segments
                print(f"[RESOLVER] Parsed {len(segments)} segments for server '{name}'")
        except Exception as e:
            print(f"Failed to resolve segments for server '{name}': {e}")
            
    return resolved


async def download_segment(index: int, priority="high"):
    """Downloads a single segment, falling back through all available sources if needed."""
    if index in state["cache"]:
        return
        
    # Check if this segment is already being downloaded
    if index in state["download_events"]:
        # Wait for the active download to finish
        print(f"[{priority.upper()}] Segment {index} is already downloading. Waiting for completion...")
        await state["download_events"][index].wait()
        
        # If the other task successfully cached it, return
        if index in state["cache"]:
            return
        # Otherwise, the previous download failed or was cancelled. Fall through to download it ourselves.
        print(f"[{priority.upper()}] Coordinated download of segment {index} failed or cancelled. Retrying ourselves...")
        
    # Register the download event coordinator
    event = asyncio.Event()
    state["download_events"][index] = event
    
    dest_path = os.path.join(state["cache_dir"], f"segment_{index}.ts")
    session = await get_session()
    
    version_before = state["sources_version"]
    success = False
    try:
        # If we need to download from network but real sources are not resolved yet, block wait
        if "Cache" in state["source_order"]:
            print(f"[{priority.upper()}] Uncached segment {index} requested before sources resolved. Waiting for background extraction...")
            await state["real_sources_resolved"].wait()
            
        async with state["download_semaphore"]:
            # Try sources sequentially until one succeeds
            for srv_name in state["source_order"]:
                segment_list = state["sources"].get(srv_name, [])
                if index >= len(segment_list):
                    continue
                    
                url = segment_list[index]
                # Use captured headers for this server
                headers = state["source_headers"].get(srv_name, {
                    "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
                })
                
                try:
                    print(f"[{priority.upper()}] Downloading segment {index} from '{srv_name}'...")
                    async with session.get(url, headers=headers, timeout=8) as resp:
                        if resp.status == 200:
                            data = await resp.read()
                            await asyncio.to_thread(write_file, dest_path, data)
                            state["cache"][index] = dest_path
                            print(f"[{priority.upper()}] Cached segment {index} ({len(data)} bytes) from '{srv_name}'")
                            success = True
                            break
                        else:
                            print(f"[{priority.upper()}] Server '{srv_name}' returned HTTP {resp.status} for segment {index}")
                            print(f"[{priority.upper()}] Requesting Playwright rescrape for '{srv_name}'...")
                            await background_extraction_and_resolution(target_servers=[srv_name], version_before=version_before)
                            
                            segment_list = state["sources"].get(srv_name, [])
                            if index < len(segment_list):
                                url = segment_list[index]
                                print(f"[{priority.upper()}] Retrying segment {index} from '{srv_name}' after Playwright refresh...")
                                async with session.get(url, headers=headers, timeout=8) as resp_retry:
                                    if resp_retry.status == 200:
                                        data = await resp_retry.read()
                                        await asyncio.to_thread(write_file, dest_path, data)
                                        state["cache"][index] = dest_path
                                        print(f"[{priority.upper()}] Cached segment {index} ({len(data)} bytes) from '{srv_name}' (retry)")
                                        success = True
                                        break
                except Exception as e:
                    print(f"[{priority.upper()}] Server '{srv_name}' failed for segment {index}: {type(e).__name__} - {e}")
                    print(f"[{priority.upper()}] Requesting Playwright rescrape for '{srv_name}'...")
                    await background_extraction_and_resolution(target_servers=[srv_name], version_before=version_before)
                    
                    segment_list = state["sources"].get(srv_name, [])
                    if index < len(segment_list):
                        url = segment_list[index]
                        try:
                            print(f"[{priority.upper()}] Retrying segment {index} from '{srv_name}' after Playwright refresh...")
                            async with session.get(url, headers=headers, timeout=8) as resp_retry:
                                if resp_retry.status == 200:
                                    data = await resp_retry.read()
                                    await asyncio.to_thread(write_file, dest_path, data)
                                    state["cache"][index] = dest_path
                                    print(f"[{priority.upper()}] Cached segment {index} ({len(data)} bytes) from '{srv_name}' (retry)")
                                    success = True
                                    break
                        except Exception as re_e:
                            print(f"[{priority.upper()}] Retry failed for segment {index}: {type(re_e).__name__} - {re_e}")
                            
        if not success:
            print(f"[ERROR] Failed to download segment {index} from all available sources.")
    finally:
        # Clean up and fire the event
        state["download_events"].pop(index, None)
        event.set()

def write_file(path, data):
    with open(path, "wb") as f:
        f.write(data)

async def forward_prefetcher(start_index: int):
    """Prefetches segments sequentially from start_index to the end of the film."""
    # Find max segment count across all resolved sources
    total_segments = max([len(lst) for lst in state["sources"].values()]) if state["sources"] else 0
    concurrency_limit = 2
    
    idx = start_index
    while idx < total_segments:
        to_download = []
        while len(to_download) < concurrency_limit and idx < total_segments:
            if idx not in state["cache"] and idx not in state["download_events"]:
                to_download.append(idx)
            idx += 1
            
        if to_download:
            tasks = [download_segment(i, "high") for i in to_download]
            await asyncio.gather(*tasks)
        else:
            await asyncio.sleep(0.1)

async def gap_backfiller(gap_start: int, gap_end: int):
    """Backfills segments that were skipped during a seek."""
    print(f"[BACKFILL] Starting backfill for indices {gap_start} to {gap_end}")
    concurrency_limit = 2
    
    idx = gap_start
    while idx <= gap_end:
        to_download = []
        while len(to_download) < concurrency_limit and idx <= gap_end:
            if idx not in state["cache"] and idx not in state["download_events"]:
                to_download.append(idx)
            idx += 1
            
        if to_download:
            tasks = [download_segment(i, "backfill") for i in to_download]
            await asyncio.gather(*tasks)
        else:
            await asyncio.sleep(0.1)
    print(f"[BACKFILL] Completed backfill for indices {gap_start} to {gap_end}")

def update_playback_position(index: int):
    """Triggered whenever the player requests a segment. Handles seek detection."""
    old_index = state["current_index"]
    state["current_index"] = index
    is_seek = index > old_index + 2 or index < old_index
    
    if is_seek:
        print(f"\n[SEEK DETECTED] Seeked from index {old_index} to {index}")
        if state["prefetch_task"]:
            state["prefetch_task"].cancel()
        state["prefetch_task"] = asyncio.create_task(forward_prefetcher(index + 1))
        
        if index > old_index + 1:
            if state["backfill_task"]:
                state["backfill_task"].cancel()
            
            # Limit backfill to prevent bandwidth starvation on large skips
            gap_size = index - old_index
            if gap_size <= 20:
                # Small gap: backfill the entire range
                state["backfill_task"] = asyncio.create_task(
                    gap_backfiller(old_index + 1, index - 1)
                )
            else:
                # Large gap: only backfill the 10 segments immediately preceding the new seek point
                # (e.g. to support minor rewinds) and avoid downloading the middle of the film.
                start_backfill = max(old_index + 1, index - 10)
                state["backfill_task"] = asyncio.create_task(
                    gap_backfiller(start_backfill, index - 1)
                )
    else:
        if state["prefetch_task"] is None or state["prefetch_task"].done():
            state["prefetch_task"] = asyncio.create_task(forward_prefetcher(index + 1))

def find_free_port(start_port=8000):
    import socket
    port = start_port
    while port < 9000:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            try:
                s.bind(('127.0.0.1', port))
                return port
            except OSError:
                port += 1
    return start_port

@app.get("/playlist.m3u8")
async def get_playlist():
    """Generates the unified virtual playlist with segments pointing to our proxy."""
    # Wait until the sources have been resolved (either from Cache metadata or background Playwright extraction)
    await state["sources_resolved_event"].wait()
    
    if not state["sources"]:
        raise HTTPException(status_code=500, detail="No source playlists resolved")
        
    # Get the length of the primary stream (first active source)
    primary_source = state["source_order"][0]
    total_segments = len(state["sources"][primary_source])
    
    port = state.get("port", 8000)
    # We will build a generic m3u8 playlist template
    lines = [
        "#EXTM3U",
        "#EXT-X-VERSION:3",
        "#EXT-X-TARGETDURATION:10",
        "#EXT-X-MEDIA-SEQUENCE:0",
    ]
    
    for i in range(total_segments):
        lines.append("#EXTINF:10.0,")
        lines.append(f"http://localhost:{port}/segment?idx={i}")
        
    lines.append("#EXT-X-ENDLIST")
    
    # Start initial prefetching
    state["prefetch_task"] = asyncio.create_task(forward_prefetcher(0))
    
    return PlainTextResponse("\n".join(lines), media_type="application/vnd.apple.mpegurl")

@app.get("/segment")
async def get_segment(idx: int = Query(...)):
    """Serves a segment. Updates playback position and triggers prefetching/backfilling."""
    update_playback_position(idx)
    
    # Check if segment is in cache
    if idx in state["cache"]:
        file_path = state["cache"][idx]
        if os.path.exists(file_path):
            with open(file_path, "rb") as f:
                return Response(content=f.read(), media_type="video/MP2T")
                
    # If not cached, download immediately (blocking player request until done)
    await download_segment(idx, "urgent")
    if idx in state["cache"]:
        file_path = state["cache"][idx]
        with open(file_path, "rb") as f:
            return Response(content=f.read(), media_type="video/MP2T")
            
    raise HTTPException(status_code=504, detail="Segment download timed out or failed on all sources")

@app.get("/sources_status")
async def get_sources_status():
    """Returns the list of stream sources and their online/offline status."""
    return {"sources": state["source_status"]}

@app.post("/refresh_sources")
async def refresh_sources():
    """Triggers a background Playwright run to rescrape only the sources that are Offline."""
    # Find all sources that are offline or missing
    offline_servers = [name for name, status in state["source_status"].items() if status == "Offline"]
    # If we have no status but have sources in the order, consider checking them
    if not offline_servers:
        # Check if any sources failed completely
        offline_servers = [name for name in state["sources_dict"].keys() if state["source_status"].get(name) == "Offline"]
        
    if not offline_servers and state["sources_dict"]:
        # Fallback: if nothing is marked offline but user requests it, check if we need to refresh anything
        return {"status": "ok", "message": "All sources appear online. No selective refresh needed."}
        
    # If we have no sources at all, scrape all
    targets = offline_servers if offline_servers else None
    print(f"[REFRESH] Triggering selective background refresh for: {targets}")
    asyncio.create_task(background_extraction_and_resolution(target_servers=targets))
    return {"status": "ok", "message": f"Refreshing sources: {targets if targets else 'All'}"}


async def run_mpv(port):
    await asyncio.sleep(2)  # Wait for server to start up
    print(f"Launching mpv on port {port}...")
    cmd = [
        "mpv",
        "--demuxer-max-bytes=1000MiB",
        "--demuxer-max-back-bytes=500MiB",
        f"http://localhost:{port}/playlist.m3u8"
    ]
    process = await asyncio.create_subprocess_exec(*cmd)
    await process.wait()

def main():
    if len(sys.argv) < 2:
        print("Usage: python stream_proxy.py <MOVIE_PAGE_URL>")
        sys.exit(1)
        
    state["page_url"] = sys.argv[1]
    
    # Find a free port
    port = find_free_port(8000)
    state["port"] = port
    print(f"Using local proxy port: {port}")
    
    # Start the local HTTP proxy server directly through uvicorn
    # This runs the ASGI application and triggers our lifespan handler
    uvicorn.run(app, host="127.0.0.1", port=port, log_level="warning")

if __name__ == "__main__":
    main()
