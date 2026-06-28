import asyncio
import json
import argparse
from urllib.parse import urlparse, unquote
from playwright.async_api import async_playwright
import requests
import concurrent.futures
import urllib3
import re
import os

urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)

CHATBOT_SESSION_FILE = "/home/lewis/Dev/chatbot/.browser-session"
DOMAIN_CACHE_FILE    = "/home/lewis/Dev/film/scraped_domains.json"
OUTPUT_FILE          = "/home/lewis/Dev/film/yify_mirrors.txt"

# Queries and how many pages to scrape per query: (query, pages)
SEARCH_PLAN = [
    ("yify",            5),
    ("yts",             5),
    ("yify proxy list", 5),
]

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

def check_site_working(url):
    """Test if a site is online, using Cloudflare DoH to bypass ISP DNS blocks."""
    import socket as _socket
    parsed = urlparse(url)
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

    if not system_ok:
        # Resolve via Cloudflare DoH
        ip = resolve_via_cloudflare(hostname)
        if not ip:
            return False
        # Monkey-patch getaddrinfo for this hostname only, then restore
        original_getaddrinfo = _socket.getaddrinfo
        def patched_getaddrinfo(host, p, *args, **kwargs):
            if host == hostname:
                return [(2, 1, 6, '', (ip, p))]
            return original_getaddrinfo(host, p, *args, **kwargs)
        _socket.getaddrinfo = patched_getaddrinfo
        try:
            resp = requests.get(url, timeout=6.0, headers={
                "User-Agent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36"
            }, verify=False, allow_redirects=True)
            return resp.status_code in [200, 301, 302, 403]
        except Exception:
            return False
        finally:
            _socket.getaddrinfo = original_getaddrinfo
    else:
        try:
            resp = requests.get(url, timeout=6.0, headers={
                "User-Agent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36"
            }, verify=False, allow_redirects=True)
            return resp.status_code in [200, 301, 302, 403]
        except Exception:
            return False


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
    Visit a proxy-list result page directly and extract all hrefs that contain
    'yify' or 'yts' anywhere in the link URL — these are the actual mirror links.
    """
    found = set()
    page = await ctx.new_page()
    try:
        print(f"  [PROXY-LIST] Visiting: {url}")
        await page.goto(url, wait_until="domcontentloaded", timeout=20000)
        await page.wait_for_timeout(1500)
        hrefs = await page.eval_on_selector_all("a[href]", "nodes => nodes.map(n => n.href)")
        for href in hrefs:
            if not href.startswith("http"):
                continue
            parsed = urlparse(href)
            domain = parsed.netloc.lower()
            # Only count it if yify/yts is in the domain itself —
            # avoids social share buttons like twitter.com/share?url=https://yts.ac/...
            if "yify" in domain or "yts" in domain:
                found.add(f"{parsed.scheme}://{domain}")
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
        for site_url in sorted(proxy_list_result_urls):
            extra = await scrape_proxy_list_page(ctx, site_url)
            if extra:
                print(f"  [PROXY-LIST] Found {len(extra)} yify/yts links on {site_url}")
                stage2_mirrors.update(extra)
                all_domains.update(extra)

    return all_domains, stage2_mirrors

async def main():
    parser = argparse.ArgumentParser(description="YIFY mirror finder")
    parser.add_argument("--recheck", action="store_true",
                        help="Skip scraping and re-verify domains from the cached scraped_domains.json")
    args = parser.parse_args()

    print("[*] Initializing YIFY Site Finder...")

    all_domains  = set()
    stage2_mirrors = set()

    if args.recheck and os.path.exists(DOMAIN_CACHE_FILE):
        print(f"[*] --recheck mode: loading domains from {DOMAIN_CACHE_FILE}")
        cache = json.loads(open(DOMAIN_CACHE_FILE).read())
        all_domains    = set(cache.get("all_domains",    []))
        stage2_mirrors = set(cache.get("stage2_mirrors", []))
        print(f"[*] Loaded {len(all_domains)} domains ({len(stage2_mirrors)} stage-2 mirrors).")
    else:
        print(f"[*] Search plan: {len(SEARCH_PLAN)} queries × up to 5 pages each = up to {sum(p for _,p in SEARCH_PLAN)} Yandex pages total.")
        ws_endpoint = None
        if os.path.exists(CHATBOT_SESSION_FILE):
            ws_endpoint = open(CHATBOT_SESSION_FILE).read().strip()
            print(f"[*] Found chatbot Chrome session: {ws_endpoint}")
        else:
            print(f"[!] No chatbot session at {CHATBOT_SESSION_FILE}. Will fall back to DuckDuckGo.")

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

        if not all_domains:
            stage2_mirrors = set()
            print("[!] Yandex scrape returned nothing. Falling back to DuckDuckGo...")
            fallback_queries = ["yify", "yts", "yify proxy", "yify mirror", "yts proxy", "yts mirror"]
            for query in fallback_queries:
                print(f"[FALLBACK] DuckDuckGo: '{query}'")
                all_domains.update(scrape_duckduckgo_query(query))

        # Save raw scraped domains to cache for future --recheck runs
        cache = {"all_domains": list(all_domains), "stage2_mirrors": list(stage2_mirrors)}
        with open(DOMAIN_CACHE_FILE, "w") as f:
            json.dump(cache, f, indent=2)
        print(f"[*] Scraped domain list cached to {DOMAIN_CACHE_FILE}")

    print(f"\n[*] Found {len(all_domains)} unique domains. Verifying which are online...")

    # Filter out blocklisted domains before checking
    clean_domains = {
        d for d in all_domains
        if not any(blocked in urlparse(d).netloc.lower() for blocked in BLOCKLIST)
    }

    print(f"[*] Checking {len(clean_domains)} domains (after filtering {len(all_domains) - len(clean_domains)} blocklisted)...")

    working_mirrors = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=20) as executor:
        results = list(executor.map(check_site_working, list(clean_domains)))
        for domain, is_working in zip(list(clean_domains), results):
            if not is_working:
                continue
            # Classify: YIFY mirror only if domain name contains yify/yts,
            # OR it was explicitly scraped as a yify/yts link from a proxy-list article
            is_yify = (
                any(x in urlparse(domain).netloc.lower() for x in ["yify", "yts"]) or
                domain in stage2_mirrors
            )
            if is_yify:
                working_mirrors.append(domain)

    working_mirrors.sort()

    # Write to file
    with open(OUTPUT_FILE, "w") as f:
        f.write(f"# YIFY/YTS Working Mirrors — {len(working_mirrors)} found\n")
        for site in working_mirrors:
            f.write(site + "\n")
    print(f"[*] Results saved to {OUTPUT_FILE}")

    print(f"\n================ WORKING YIFY MIRRORS ({len(working_mirrors)} found) ================")
    for site in working_mirrors:
        print(site)
    print("====================================================")

if __name__ == "__main__":
    asyncio.run(main())
