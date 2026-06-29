import asyncio
import json
import argparse
from urllib.parse import urlparse, unquote, urlencode, urljoin, parse_qsl
import requests
import urllib3
import re
import os
import socket
import threading

try:
    from playwright.async_api import async_playwright
except ImportError:
    async_playwright = None

urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)

ORIGINAL_GETADDRINFO = socket.getaddrinfo
GETADDRINFO_PATCH_LOCK = threading.Lock()

CHATBOT_SESSION_FILE = "/home/lewis/Dev/chatbot/.browser-session"
DOMAIN_CACHE_FILE    = "/home/lewis/Dev/film/scraped_domains.json"
GATHERED_OUTPUT_FILE = "/home/lewis/Dev/film/yify_all_gathered_links.txt"
SEARCH_PROBE_QUERY   = "apex"
SEARCH_PAGES_PER_QUERY = 5
SEARCH_PLAN = []

PROXY_LIST_CONCURRENCY = 8

# Domains to always exclude from results (search engines, social media, VPN sellers, etc.)
BLOCKLIST = {
    "github.com", "youtube.com", "reddit.com", "x.com", "t.me", "medium.com",
    "mastodon.social", "utorrent.com", "bittorrent.com", "wikipedia.org",
    "aol.com", "urbandictionary.com", "apkpure.net", "apkpure.com",
    "purevpn.com", "nordvpn.com", "expressvpn.com", "ufovpn.io", "onlinevpn.app",
    "momoproxy.com", "papaproxy.net", "okkproxy.com", "proxyelite.info",
    "vpnpro.com", "iprovpn.com", "swiftproxy.net", "kindproxy.com", "okeyproxy.com",
    "techpp.com", "techcult.com", "techworm.net", "tech-latest.com", "techblast.net",
    "techgloss.com", "techycoder.com", "thetechbasket.com", "technopublish.com",
    "geeksgyaan.com", "cooltechzone.com", "digitalmagazine.org", "beencrypted.com",
    "privacysavvy.com", "sguru.org", "hvtimes.com", "ipfly.net", "edramatica.com",
    "kfanhub.com", "toorgle.com", "gosites.org", "trytechnical.com",
    "creativepixelmag.com", "waybinary.com", "theunfolder.com", "positioniseverything.net",
    "biztechpost.com", "cyberkendra.com", "onlinefancier.com", "techmaish.com",
    "cartelpress.pages.dev", "austinspecialsblog.com.ng", "droidthunder.com",
}

SKIP_DOMAINS = {"yandex", "yastatic", "google", "bing", "microsoft", "yahoo", "wikipedia"}

FULL_SITE_TEXT_MARKERS = (
    "view full site",
    "full site",
    "continue to site",
    "open full site",
    "visit full site",
    "go to site",
)

def normalize_site_url(raw_url):
    if not raw_url or not raw_url.startswith("http"):
        return None
    parsed = urlparse(raw_url)
    hostname = parsed.hostname.lower() if parsed.hostname else ""
    if not parsed.scheme or not hostname:
        return None
    port = f":{parsed.port}" if parsed.port else ""
    return f"{parsed.scheme}://{hostname}{port}"

def fetch_url_with_optional_doh(url, timeout=6.0):
    """Fetch a URL, retrying with per-host DoH resolution if system DNS fails."""
    import socket as _socket

    parsed = urlparse(url)
    hostname = parsed.hostname
    scheme = parsed.scheme
    port = 443 if scheme == "https" else 80

    if not hostname or not scheme:
        return None

    headers = {"User-Agent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36"}

    def is_dns_like_failure(exc):
        text = str(exc).lower()
        return any(
            marker in text
            for marker in (
                "name or service not known",
                "temporary failure in name resolution",
                "failed to resolve",
                "nodename nor servname provided",
                "no address associated with hostname",
                "dns",
                "getaddrinfo",
            )
        )

    try:
        return requests.get(url, timeout=timeout, headers=headers, verify=False, allow_redirects=True)
    except Exception as exc:
        if not is_dns_like_failure(exc):
            return None

    ip = resolve_via_cloudflare(hostname)
    if not ip:
        return None

    def patched_getaddrinfo(host, p, *args, **kwargs):
        if host == hostname:
            return [(2, 1, 6, "", (ip, p))]
        return ORIGINAL_GETADDRINFO(host, p, *args, **kwargs)

    acquired = GETADDRINFO_PATCH_LOCK.acquire(timeout=max(1.0, timeout + 1.0))
    if not acquired:
        return None
    try:
        previous_getaddrinfo = _socket.getaddrinfo
        _socket.getaddrinfo = patched_getaddrinfo
        try:
            return requests.get(url, timeout=timeout, headers=headers, verify=False, allow_redirects=True)
        except Exception:
            return None
        finally:
            _socket.getaddrinfo = previous_getaddrinfo
    finally:
        GETADDRINFO_PATCH_LOCK.release()

def extract_search_candidates(base_url, html, query):
    candidates = []
    seen = set()
    query_param_names = {"keyword", "q", "query", "search", "query_term", "term", "s"}

    def add_candidate(url, fmt):
        if not url or url in seen:
            return
        seen.add(url)
        candidates.append((url, fmt))

    def add_rewritten_query_url(full_url):
        parsed = urlparse(full_url)
        params = parse_qsl(parsed.query, keep_blank_values=True)
        rewritten = []
        used_query_param = None
        for key, value in params:
            if key.lower() in query_param_names:
                rewritten.append((key, query))
                used_query_param = key
            else:
                rewritten.append((key, value))
        if used_query_param:
            rebuilt = parsed._replace(query=urlencode(rewritten))
            add_candidate(rebuilt.geturl(), f"{parsed.path or '/'}?{used_query_param}={{query}}")

    def add_rewritten_path_url(full_url):
        parsed = urlparse(full_url)
        path = parsed.path or "/"
        segments = [seg for seg in path.split("/") if seg]
        if not segments:
            return

        if segments[0].lower() == "browse-movies" and len(segments) >= 2:
            rewritten_segments = segments[:]
            rewritten_segments[1] = query
            rebuilt_path = "/" + "/".join(rewritten_segments)
            rebuilt = parsed._replace(path=rebuilt_path)
            add_candidate(rebuilt.geturl(), "/browse-movies/{query}/...")
        elif segments[0].lower() in {"search", "find"} and len(segments) >= 2:
            rewritten_segments = segments[:]
            rewritten_segments[1] = query
            rebuilt_path = "/" + "/".join(rewritten_segments)
            rebuilt = parsed._replace(path=rebuilt_path)
            add_candidate(rebuilt.geturl(), f"/{segments[0]}/{{query}}")

    lower_html = html.lower()

    # Probe common YTS search URL shapes first.
    add_candidate(f"{base_url}/?keyword={query}", "/?keyword={query}")
    add_candidate(
        f"{base_url}/?keyword={query}&quality=All&genre=all&rating=0&year=0&language=all&sort_by=latest",
        "/?keyword={query}&quality=All&genre=all&rating=0&year=0&language=all&sort_by=latest",
    )
    add_candidate(f"{base_url}/browse-movies?keyword={query}", "/browse-movies?keyword={query}")
    add_candidate(
        f"{base_url}/browse-movies/{query}/all/all/0/latest/0/all",
        "/browse-movies/{query}/all/all/0/latest/0/all",
    )

    # If the homepage exposes a search form, build candidates from the full form shape.
    for match in re.finditer(r"(?is)<form\b([^>]*)>(.*?)</form>", html):
        attrs = match.group(1)
        inner = match.group(2)
        blob = f"{attrs} {inner}".lower()
        if not any(marker in blob for marker in ("keyword", "search", "browse-movies", "query_term")):
            continue

        action_match = re.search(r'action=["\']([^"\']*)["\']', attrs, re.I)
        action = action_match.group(1).strip() if action_match else ""
        method_match = re.search(r'method=["\']([^"\']*)["\']', attrs, re.I)
        method = method_match.group(1).strip().lower() if method_match else "get"

        params = {}
        query_param_name = None
        for input_match in re.finditer(r'(?is)<input\b([^>]*)>', inner):
            input_attrs = input_match.group(1)
            name_match = re.search(r'name=["\']([^"\']+)["\']', input_attrs, re.I)
            if not name_match:
                continue
            name = name_match.group(1)
            value_match = re.search(r'value=["\']([^"\']*)["\']', input_attrs, re.I)
            value = value_match.group(1) if value_match else ""
            type_match = re.search(r'type=["\']([^"\']+)["\']', input_attrs, re.I)
            input_type = (type_match.group(1).strip().lower() if type_match else "text")
            lower_name = name.lower()
            if lower_name in query_param_names:
                query_param_name = name
            elif input_type in {"hidden", "submit", "button"} and value:
                params[name] = value

        for select_match in re.finditer(r'(?is)<select\b([^>]*)>(.*?)</select>', inner):
            select_attrs = select_match.group(1)
            select_inner = select_match.group(2)
            name_match = re.search(r'name=["\']([^"\']+)["\']', select_attrs, re.I)
            if not name_match:
                continue
            name = name_match.group(1)
            selected_match = re.search(r'(?is)<option\b([^>]*)value=["\']([^"\']*)["\']([^>]*)selected', select_inner, re.I)
            if selected_match:
                params[name] = selected_match.group(2)
                continue
            first_option_match = re.search(r'(?is)<option\b[^>]*value=["\']([^"\']*)["\']', select_inner, re.I)
            if first_option_match:
                params[name] = first_option_match.group(1)

        for button_match in re.finditer(r'(?is)<button\b([^>]*)>(.*?)</button>', inner):
            button_attrs = button_match.group(1)
            name_match = re.search(r'name=["\']([^"\']+)["\']', button_attrs, re.I)
            value_match = re.search(r'value=["\']([^"\']*)["\']', button_attrs, re.I)
            if name_match and value_match:
                params[name_match.group(1)] = value_match.group(1)

        if not query_param_name:
            continue

        params[query_param_name] = query
        action_url = urljoin(f"{base_url}/", action or "/")
        if method == "get":
            separator = "&" if urlparse(action_url).query else "?"
            candidate_url = f"{action_url}{separator}{urlencode(params)}"
            add_candidate(candidate_url, f"{urlparse(action_url).path or '/'}?{query_param_name}={{query}}")

    # Learn from explicit internal browse/search links on the homepage.
    for href in re.findall(r'href=["\']([^"\']+)["\']', html, re.I):
        if href.startswith("#") or href.startswith("javascript:"):
            continue
        full = urljoin(f"{base_url}/", href)
        parsed = urlparse(full)
        if parsed.netloc and parsed.netloc.lower() != urlparse(base_url).netloc.lower():
            continue
        href_lower = full.lower()
        if any(token in href_lower for token in ("browse-movies", "search", "keyword=", "query=", "query_term=", "q=")):
            add_rewritten_query_url(full)
            add_rewritten_path_url(full)

    # If the shell clearly advertises path-based browsing, keep that pattern high confidence.
    if "browse-movies" in lower_html:
        add_candidate(
            f"{base_url}/browse-movies/{query}/all/all/0/latest/0/all",
            "/browse-movies/{query}/all/all/0/latest/0/all",
        )

    return candidates

def classify_search_response(resp, query):
    if resp is None:
        return False, "request failed"
    if resp.status_code not in (200, 301, 302, 403):
        return False, f"unexpected status code {resp.status_code}"

    body = resp.text
    body_lower = body.lower()
    query_lower = query.lower()

    if any(marker in body_lower for marker in (
        "primewire", "sflix", "expireddomains", "domain sale",
        "view full site", "continue to site", "open full site",
        "visit full site", "go to site", "challenges.cloudflare.com",
        "just a moment", "cf-cookie"
    )):
        return False, "gateway, parking, or challenge page"

    if any(marker in body for marker in (
        "browse-movie-wrap", "browse-movie-link", "browse-movie-title",
        "browse-movie-year", "movie-info"
    )):
        if query_lower in body_lower:
            return True, "movie grid markers found with query present"
        return False, "movie grid present but query term missing"

    if any(marker in body_lower for marker in (
        'name="keyword"', "name='keyword'", "/browse-movies",
        "?keyword=", "search movies", "browse movies"
    )):
        if "no movies found" in body_lower or "movie not found" in body_lower:
            if query_lower in body_lower:
                return True, "search endpoint responded with empty results state for query"
            return False, "empty results shell present but query term missing"
        if any(marker in body_lower for marker in (
            "browse movie", "latest yify movies", "yts.mx", "yts.lt", "yts.rs"
        )):
            return False, "search shell present but no actual search results detected"
        return False, "query page loaded but result markers were missing"

    return False, "not a recognizable YTS search result page"

def probe_site_search(url, query=SEARCH_PROBE_QUERY):
    homepage_resp = fetch_url_with_optional_doh(url, timeout=6.0)
    if homepage_resp is None:
        return {
            "searchable": False,
            "format": None,
            "url": url,
            "status_code": None,
            "reason": "homepage request failed or DNS resolution failed",
        }

    if homepage_resp.status_code not in (200, 301, 302, 403):
        return {
            "searchable": False,
            "format": None,
            "url": url,
            "status_code": homepage_resp.status_code,
            "reason": f"homepage returned unsupported status {homepage_resp.status_code}",
        }

    homepage_html = homepage_resp.text
    candidates = extract_search_candidates(url, homepage_html, query)
    last_reason = "no search candidates discovered"

    for candidate_url, fmt in candidates:
        resp = fetch_url_with_optional_doh(candidate_url, timeout=6.0)
        is_valid, reason = classify_search_response(resp, query)
        if is_valid:
            return {
                "searchable": True,
                "format": fmt,
                "url": candidate_url,
                "status_code": resp.status_code if resp is not None else None,
                "reason": reason,
            }
        last_reason = f"{fmt}: {reason}"

    has_search_box = any(token in homepage_html.lower() for token in (
        'name="keyword"', "name='keyword'", 'type="search"', "type='search'", "search movies"
    ))
    if has_search_box:
        return {
            "searchable": False,
            "format": "search box detected but search URL pattern probe failed",
            "url": url,
            "status_code": homepage_resp.status_code,
            "reason": last_reason,
        }

    return {
        "searchable": False,
        "format": "no working search form or browse pattern detected",
        "url": url,
        "status_code": homepage_resp.status_code,
        "reason": last_reason,
    }

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

def scrape_duckduckgo_query(query):
    """DuckDuckGo HTML fallback scraper."""
    domains = set()
    url = f"https://html.duckduckgo.com/html/?q={requests.utils.quote(query)}"
    try:
        resp = requests.get(url, headers={
            "User-Agent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
        }, timeout=10.0)
        if resp.status_code == 200:
            links = re.findall(r'/l/\?uddg=(https?%3A%2F%2F[^&\"\']+)', resp.text)
            for l in links:
                decoded = unquote(l)
                parsed = urlparse(decoded)
                domain = parsed.netloc.lower()
                if domain and "duckduckgo" not in domain:
                    domains.add(f"{parsed.scheme}://{domain}")
    except Exception as e:
        print(f"[FALLBACK] Error scraping DuckDuckGo for '{query}': {e}")
    return domains

async def is_captcha_page(page):
    """Detect Yandex SmartCaptcha reliably via page title or the dedicated captcha form."""
    try:
        title = await page.title()
        if "robot" in title.lower() or "captcha" in title.lower():
            return True
        captcha_wrapper = page.locator('[class*="SmartCaptcha"], [id*="captcha-form"]')
        return await captcha_wrapper.count() > 0
    except Exception:
        return False

async def wait_for_captcha_solve(page):
    """Pause until the user solves the CAPTCHA in the visible browser window."""
    print("\n[!] CAPTCHA detected in the browser window.")
    print("[!] Please solve the CAPTCHA in the open Chrome window, then press Enter here to continue...")
    await asyncio.get_event_loop().run_in_executor(None, input)
    await page.wait_for_timeout(1500)
    print("[*] Resuming scrape...")

def extract_domains_from_hrefs(hrefs, context_label=""):
    """Filter a list of hrefs down to unique base domain URLs, skipping search engine noise."""
    domains = set()
    for href in hrefs:
        if not href.startswith("http"):
            continue
        parsed = urlparse(href)
        domain = parsed.netloc.lower()
        if not domain:
            continue
        if any(skip in domain for skip in SKIP_DOMAINS):
            continue
        domains.add(f"{parsed.scheme}://{domain}")
    return domains

async def scrape_yandex_page(ctx, url):
    """
    Load a single Yandex search page.
    Returns (all_hrefs, snippet_map, organic_result_urls) or (None, None, None) on failure.
      - all_hrefs: every link on the page (used to collect domains)
      - snippet_map: netloc -> snippet text (used for classification)
      - organic_result_urls: the actual full URLs of the organic search results
                             (used to visit proxy-list pages with their full path intact)
    """
    page = await ctx.new_page()
    hrefs = []
    snippet_map = {}
    organic_urls = []
    try:
        await page.goto(url, wait_until="domcontentloaded", timeout=20000)
        await page.wait_for_timeout(1500)

        if await is_captcha_page(page):
            await wait_for_captcha_solve(page)
            if await is_captcha_page(page):
                print("[YANDEX] CAPTCHA still present after solve attempt. Skipping page.")
                return None, None, None
            await page.goto(url, wait_until="domcontentloaded", timeout=20000)
            await page.wait_for_timeout(1500)

        # Stop if no organic results exist (end of result pages)
        results = page.locator('.serp-item, .organic, [class*="organic__"]')
        if await results.count() == 0:
            return None, None, None

        hrefs = await page.eval_on_selector_all("a[href]", "nodes => nodes.map(n => n.href)")

        # Organic result title links — these are the actual page URLs Yandex is linking to
        organic_urls = await page.eval_on_selector_all(
            ".OrganicTitle-Link, h2 a[href], .organic__title-wrapper a[href]",
            "nodes => nodes.map(n => n.href).filter(h => h.startsWith('http'))"
        )

        snippets = await page.eval_on_selector_all(
            ".organic__url, .OrganicTitle-Link, [class*='organic'] a[href]",
            "nodes => nodes.map(n => ({href: n.href, text: (n.innerText || n.textContent || '').toLowerCase()}))"
        )
        for s in snippets:
            parsed = urlparse(s['href'])
            if parsed.netloc:
                snippet_map[parsed.netloc.lower()] = s['text']

    except Exception as e:
        print(f"[YANDEX] Error loading {url}: {e}")
    finally:
        await page.close()

    return hrefs, snippet_map, organic_urls

async def scrape_proxy_list_page(ctx, url):
    """
    Visit a proxy-list result page directly and extract hrefs whose domain
    contains yify/yts.
    """
    found = set()
    page = await ctx.new_page()
    try:
        print(f"  [PROXY-LIST] Visiting: {url}")
        await page.goto(url, wait_until="domcontentloaded", timeout=20000)
        await page.wait_for_timeout(1500)
        links = await page.eval_on_selector_all(
            "a[href]",
            "nodes => nodes.map(n => ({href: n.href, text: (n.innerText || n.textContent || '').trim().toLowerCase()}))"
        )
        for link in links:
            href = link.get("href", "")
            text = link.get("text", "")
            normalized = normalize_site_url(href)
            if not normalized:
                continue

            parsed = urlparse(normalized)
            domain = parsed.netloc.lower()

            # Only count it by domain if yify/yts is in the domain itself —
            # avoids social share buttons like twitter.com/share?url=https://yts.ac/...
            if "yify" in domain or "yts" in domain:
                found.add(normalized)
                continue

    except Exception as e:
        print(f"  [PROXY-LIST] Error visiting {url}: {e}")
    finally:
        await page.close()
    return found

async def run_search_plan(browser):
    """
    Execute the full search plan:
      - 5 pages of 'yify'            → collect result domains
      - 5 pages of 'yts'             → collect result domains
      - 5 pages of 'yify proxy list' → collect result domains AND visit each
                                        actual result page URL (with full path)
                                        to scrape embedded yify/yts links
    Returns (all_domains, stage2_mirrors).
      - all_domains: every domain seen across all Yandex pages
      - stage2_mirrors: domains explicitly found as yify/yts links inside proxy-list articles
    """
    all_domains = set()
    proxy_list_result_urls = set()  # full article URLs to visit in stage 2
    stage2_mirrors = set()          # domains explicitly found as yify/yts links in stage 2
    ctx = browser.contexts[0]
    total = sum(pages for _, pages in SEARCH_PLAN)
    pages_done = 0

    for query, max_pages in SEARCH_PLAN:
        is_proxy_query = "proxy list" in query
        print(f"\n[YANDEX] === Query: '{query}' ({max_pages} pages) ===")

        for page_idx in range(max_pages):
            pages_done += 1
            url = f"https://yandex.com/search/?text={requests.utils.quote(query)}&p={page_idx}"
            print(f"[YANDEX] Page {page_idx + 1}/{max_pages}  (overall {pages_done}/{total})")

            hrefs, snippet_map, organic_urls = await scrape_yandex_page(ctx, url)
            if hrefs is None:
                print(f"[YANDEX] No more results for '{query}'. Moving on.")
                break

            page_domains = extract_domains_from_hrefs(hrefs)


            if is_proxy_query and organic_urls:
                # Store the FULL result URLs (with path) so we visit the actual article page
                for full_url in organic_urls:
                    parsed = urlparse(full_url)
                    if any(skip in parsed.netloc.lower() for skip in SKIP_DOMAINS):
                        continue
                    proxy_list_result_urls.add(full_url)

            print(f"[YANDEX] Found {len(page_domains)} domains" +
                  (f", {len(organic_urls)} result page URLs" if is_proxy_query else "") + ".")
            all_domains.update(page_domains)
            await asyncio.sleep(0.8)


    # Stage 2: visit proxy-list result pages and harvest embedded yify/yts links
    if proxy_list_result_urls:
        print(f"\n[PROXY-LIST] Visiting {len(proxy_list_result_urls)} proxy-list article pages to harvest embedded yify/yts links...")
        semaphore = asyncio.Semaphore(PROXY_LIST_CONCURRENCY)

        async def scrape_proxy_list_page_limited(site_url):
            async with semaphore:
                extra = await scrape_proxy_list_page(ctx, site_url)
                return site_url, extra

        tasks = [
            asyncio.create_task(scrape_proxy_list_page_limited(site_url))
            for site_url in sorted(proxy_list_result_urls)
        ]
        for task in asyncio.as_completed(tasks):
            site_url, extra = await task
            if extra:
                print(f"  [PROXY-LIST] Found {len(extra)} yify/yts links on {site_url}")
                stage2_mirrors.update(extra)
                all_domains.update(extra)

    return all_domains, stage2_mirrors


async def revisit_cached_domains(browser, cached_domains):
    """
    Visit each cached domain directly and harvest any linked yify/yts mirrors from
    those pages, without rerunning the search-engine scrape.
    """
    all_domains = set(cached_domains)
    discovered_mirrors = set()
    ctx = browser.contexts[0]

    if not cached_domains:
        return all_domains, discovered_mirrors

    print(f"\n[CACHED-VISIT] Visiting {len(cached_domains)} cached domains to harvest linked yify/yts mirrors...")
    semaphore = asyncio.Semaphore(PROXY_LIST_CONCURRENCY)

    async def revisit_one(site_url):
        async with semaphore:
            extra = await scrape_proxy_list_page(ctx, site_url)
            return site_url, extra

    tasks = [
        asyncio.create_task(revisit_one(site_url))
        for site_url in sorted(cached_domains)
    ]
    for task in asyncio.as_completed(tasks):
        site_url, extra = await task
        if extra:
            print(f"  [CACHED-VISIT] Found {len(extra)} yify/yts links on {site_url}")
            discovered_mirrors.update(extra)
            all_domains.update(extra)

    return all_domains, discovered_mirrors

async def main():
    parser = argparse.ArgumentParser(description="YIFY mirror finder")
    parser.add_argument(
        "queries",
        nargs="*",
        help="Search terms to run through Yandex and DuckDuckGo fallback; each term is searched for five pages",
    )
    parser.add_argument(
        "--search-engine",
        choices=("yandex", "duckduckgo", "both"),
        default="both",
        help="Which discovery source to use for stage 1 gathering (default: both)",
    )
    parser.add_argument("--recheck", action="store_true",
                        help="Skip scraping and re-verify domains from the cached scraped_domains.json")
    parser.add_argument("--visit-cached-domains", action="store_true",
                        help="Load scraped_domains.json, visit each cached domain in the browser, and rebuild the gathered list without rerunning search scraping")
    args = parser.parse_args()

    print("[*] Initializing YIFY Site Finder...")

    if not (args.recheck or args.visit_cached_domains):
        if not args.queries:
            parser.error("stage 1 now requires one or more search terms unless --recheck or --visit-cached-domains is used")
        search_plan = [(query, SEARCH_PAGES_PER_QUERY) for query in args.queries]
    else:
        search_plan = []

    all_domains  = set()
    stage2_mirrors = set()
    existing_all_domains = set()
    existing_stage2_mirrors = set()

    if (args.recheck or args.visit_cached_domains) and os.path.exists(DOMAIN_CACHE_FILE):
        print(f"[*] --recheck mode: loading domains from {DOMAIN_CACHE_FILE}")
        cache = json.loads(open(DOMAIN_CACHE_FILE).read())
        all_domains    = set(cache.get("all_domains",    []))
        stage2_mirrors = set(cache.get("stage2_mirrors", []))
        print(f"[*] Loaded {len(all_domains)} domains ({len(stage2_mirrors)} stage-2 mirrors).")
    elif not args.visit_cached_domains:
        if os.path.exists(DOMAIN_CACHE_FILE):
            try:
                existing_cache = json.loads(open(DOMAIN_CACHE_FILE).read())
                existing_all_domains = set(existing_cache.get("all_domains", []))
                existing_stage2_mirrors = set(existing_cache.get("stage2_mirrors", []))
                if existing_all_domains or existing_stage2_mirrors:
                    print(
                        f"[*] Loaded existing gathered cache with {len(existing_all_domains)} domains "
                        f"and {len(existing_stage2_mirrors)} stage-2 mirrors for merge."
                    )
            except Exception as e:
                print(f"[!] Failed to read existing gathered cache for merge: {e}")

        global SEARCH_PLAN
        SEARCH_PLAN = search_plan
        use_yandex = args.search_engine in {"yandex", "both"}
        use_duckduckgo = args.search_engine in {"duckduckgo", "both"}
        print(
            f"[*] Search plan: {len(SEARCH_PLAN)} queries × {SEARCH_PAGES_PER_QUERY} pages each = "
            f"up to {sum(p for _, p in SEARCH_PLAN)} pages per enabled engine "
            f"({args.search_engine})."
        )
        ws_endpoint = None
        if use_yandex and os.path.exists(CHATBOT_SESSION_FILE):
            ws_endpoint = open(CHATBOT_SESSION_FILE).read().strip()
            print(f"[*] Found chatbot Chrome session: {ws_endpoint}")
        elif use_yandex:
            if use_duckduckgo:
                print(f"[!] No chatbot session at {CHATBOT_SESSION_FILE}. Yandex scraping is unavailable, DuckDuckGo will still run.")
            else:
                print(f"[!] No chatbot session at {CHATBOT_SESSION_FILE}. Yandex scraping is unavailable in yandex-only mode.")

        if use_yandex and async_playwright is None:
            if use_duckduckgo:
                print("[!] Playwright is not installed. Skipping browser-based Yandex scrape; DuckDuckGo will still run.")
            else:
                print("[!] Playwright is not installed. Yandex scraping is unavailable in yandex-only mode.")
        elif use_yandex:
            async with async_playwright() as p:
                browser = None
                if ws_endpoint:
                    try:
                        browser = await p.chromium.connect_over_cdp(ws_endpoint)
                        print(f"[*] Connected to Chrome via CDP ({len(browser.contexts[0].pages)} open tabs).")
                    except Exception as e:
                        print(f"[!] CDP connection failed: {e}")

                if browser:
                    all_domains, stage2_mirrors = await run_search_plan(browser)
                    await browser.close()

        should_run_duckduckgo = use_duckduckgo
        if should_run_duckduckgo:
            if use_yandex and not all_domains:
                stage2_mirrors = set()
                print("[!] Yandex scrape returned nothing. Continuing with DuckDuckGo...")
            else:
                print("[*] Running DuckDuckGo stage 1 gathering...")
            for query in args.queries:
                print(f"[DUCKDUCKGO] '{query}'")
                all_domains.update(scrape_duckduckgo_query(query))

        all_domains.update(existing_all_domains)
        stage2_mirrors.update(existing_stage2_mirrors)

        # Save raw scraped domains to cache for future --recheck runs
        cache = {"all_domains": list(all_domains), "stage2_mirrors": list(stage2_mirrors)}
        with open(DOMAIN_CACHE_FILE, "w") as f:
            json.dump(cache, f, indent=2)
        print(f"[*] Scraped domain list merged and cached to {DOMAIN_CACHE_FILE}")

    if args.visit_cached_domains:
        if not os.path.exists(CHATBOT_SESSION_FILE):
            print(f"[!] No chatbot session at {CHATBOT_SESSION_FILE}. Cannot visit cached domains in browser.")
        elif async_playwright is None:
            print("[!] Playwright is not installed. Cannot visit cached domains in browser.")
        else:
            ws_endpoint = open(CHATBOT_SESSION_FILE).read().strip()
            async with async_playwright() as p:
                browser = None
                try:
                    browser = await p.chromium.connect_over_cdp(ws_endpoint)
                    print(f"[*] Connected to Chrome via CDP ({len(browser.contexts[0].pages)} open tabs).")
                except Exception as e:
                    print(f"[!] CDP connection failed: {e}")

                if browser:
                    all_domains, discovered = await revisit_cached_domains(browser, all_domains)
                    stage2_mirrors.update(discovered)
                    await browser.close()
                    cache = {"all_domains": list(all_domains), "stage2_mirrors": list(stage2_mirrors)}
                    with open(DOMAIN_CACHE_FILE, "w") as f:
                        json.dump(cache, f, indent=2)
                    print(f"[*] Updated scraped domain cache written to {DOMAIN_CACHE_FILE}")

    gathered_domains = sorted(all_domains)
    with open(GATHERED_OUTPUT_FILE, "w") as f:
        f.write(f"# Gathered YIFY/YTS Candidate Links — {len(gathered_domains)} found\n")
        for site in gathered_domains:
            f.write(site + "\n")

    print(f"\n[*] Gathered {len(gathered_domains)} unique candidate domains.")
    print(f"[*] Raw gathered list saved to {GATHERED_OUTPUT_FILE}")
    print("[*] Run verify_yify_search.py to classify searchable mirrors and record failures.")

if __name__ == "__main__":
    asyncio.run(main())
