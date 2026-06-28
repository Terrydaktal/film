import sys
import json
import requests
import urllib3
from urllib.parse import urlparse, quote

# Suppress SSL warnings for mirror sites
urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)

MIRRORS_FILE = "/home/lewis/Dev/film/yify_mirrors.txt"

def load_yts_mirrors():
    """Load and clean the list of verified YIFY mirrors from yify_mirrors.txt"""
    mirrors = []
    try:
        with open(MIRRORS_FILE, "r") as f:
            for line in f:
                line = line.strip()
                if not line or line.startswith("#"):
                    continue
                parsed = urlparse(line)
                domain = parsed.netloc.lower()
                # Filter out any non-YTS/YIFY domains to ensure absolute relevance
                if "yts" in domain or "yify" in domain:
                    mirrors.append(line)
    except FileNotFoundError:
        print(f"[!] Error: {MIRRORS_FILE} not found. Run find_yify_sites.py first.")
        sys.exit(1)
    
    # Put HTTPS mirrors first for security, then HTTP
    mirrors.sort(key=lambda x: x.startswith("https://"), reverse=True)
    return mirrors

def resolve_via_cloudflare(hostname):
    """Resolve a hostname using Cloudflare DNS-over-HTTPS, bypassing ISP DNS blocks."""
    try:
        resp = requests.get(
            f"https://cloudflare-dns.com/dns-query?name={hostname}&type=A",
            headers={"Accept": "application/dns-json"},
            timeout=5.0
        )
        data = resp.json()
        answers = data.get("Answer", [])
        for answer in answers:
            if answer.get("type") == 1:  # A record
                return answer["data"]
    except Exception:
        pass
    return None

def make_magnet_link(info_hash, title):
    """Generate a standard BitTorrent magnet link from a YTS torrent hash."""
    trackers = [
        "udp://open.demonii.com:1337/announce",
        "udp://tracker.openbittorrent.com:80",
        "udp://tracker.coppersurfer.tk:6969",
        "udp://glotorrents.pw:6969/announce",
        "udp://tracker.opentrackr.org:1337/announce",
        "udp://p4p.arenabg.com:1337",
        "udp://tracker.leechers-paradise.org:6969"
    ]
    tracker_args = "".join([f"&tr={quote(t)}" for t in trackers])
    return f"magnet:?xt=urn:btih:{info_hash}&dn={quote(title)}{tracker_args}"

def search_mirrors(query):
    print(f"[*] Searching for '{query}' across mirrors...")
    mirrors = load_yts_mirrors()
    
    if not mirrors:
        print("[!] No working YTS/YIFY mirrors found in your mirror list.")
        return

    import socket as _socket

    for idx, mirror in enumerate(mirrors):
        parsed = urlparse(mirror)
        hostname = parsed.netloc
        scheme = parsed.scheme
        port = 443 if scheme == "https" else 80

        # Check if system DNS can resolve it
        system_ok = False
        try:
            _socket.getaddrinfo(hostname, port)
            system_ok = True
        except OSError:
            pass

        ip = None
        if not system_ok:
            ip = resolve_via_cloudflare(hostname)
            if not ip:
                # DNS-over-HTTPS also failed, skip mirror
                continue

        # Monkey-patch getaddrinfo if we're using a bypassed IP
        original_getaddrinfo = _socket.getaddrinfo
        if ip:
            def patched_getaddrinfo(host, p, *args, **kwargs):
                if host == hostname:
                    return [(2, 1, 6, '', (ip, p))]
                return original_getaddrinfo(host, p, *args, **kwargs)
            _socket.getaddrinfo = patched_getaddrinfo

        api_url = f"{mirror}/api/v2/list_movies.json"
        print(f" [{idx + 1}/{len(mirrors)}] Querying: {mirror}")
        
        try:
            resp = requests.get(api_url, params={"query_term": query, "limit": 5}, timeout=6.0, headers={
                "User-Agent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36"
            }, verify=False)
            
            if resp.status_code != 200:
                print(f"      -> Failed (HTTP {resp.status_code})")
                continue
                
            data = resp.json()
            if data.get("status") != "ok":
                print("      -> API returned non-ok status")
                continue
                
            movie_data = data.get("data", {})
            movie_count = movie_data.get("movie_count", 0)
            
            if movie_count == 0:
                print(f"[*] Film '{query}' was not found in the YTS database (returned 0 results).")
                print("[*] Since YTS mirrors share the same database, this film is likely not on YIFY.")
                return
                
            movies = movie_data.get("movies", [])
            if not movies:
                print("      -> No movies list in data payload")
                continue
                
            # Success! Print film matches
            print(f"\n[+] Success! Found {len(movies)} matches on {mirror}:\n")
            for movie in movies:
                title = movie.get("title_long", movie.get("title"))
                rating = movie.get("rating", 0)
                genres = ", ".join(movie.get("genres", []))
                runtime = movie.get("runtime", 0)
                
                print(f"🎥 {title}")
                print(f"   ★ Rating: {rating}/10  |  ⏱ Runtime: {runtime} min  |  🏷 Genres: {genres}")
                print("   🧲 Available Downloads:")
                
                for t in movie.get("torrents", []):
                    quality = t.get("quality")
                    size = t.get("size")
                    info_hash = t.get("hash")
                    magnet = make_magnet_link(info_hash, title)
                    
                    print(f"     - [{quality}] ({size})")
                    print(f"       Hash:   {info_hash}")
                    print(f"       Magnet: {magnet}\n")
            return
            
        except Exception as e:
            # Fall back to next mirror on timeout/connection/SSL errors
            print(f"      -> Connection failed: {type(e).__name__}")
            continue
        finally:
            if ip:
                _socket.getaddrinfo = original_getaddrinfo

    print("\n[!] Checked all available mirrors, but could not retrieve search results.")

if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("Usage: uv run python search_movie.py <movie_name>")
        sys.exit(1)
        
    search_query = " ".join(sys.argv[1:])
    search_mirrors(search_query)
