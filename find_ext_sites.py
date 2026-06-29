import argparse
import asyncio
import json
import os
from urllib.parse import urlparse

from find_yify_sites import (
    CHATBOT_SESSION_FILE,
    PROXY_LIST_CONCURRENCY,
    async_playwright,
    extract_domains_from_hrefs,
    normalize_site_url,
    scrape_duckduckgo_query,
    scrape_yandex_page,
)

DOMAIN_CACHE_FILE = "/home/lewis/Dev/film/ext_scraped_domains.json"
GATHERED_OUTPUT_FILE = "/home/lewis/Dev/film/ext_all_gathered_links.txt"
SEARCH_PAGES_PER_QUERY = 5

EXT_HOST_MARKERS = (
    "ext.to",
    "extto",
    "extratorrent",
    "extratorrents",
    "extra-torrent",
)


def is_ext_domain(domain: str) -> bool:
    domain = domain.lower()
    return any(marker in domain for marker in EXT_HOST_MARKERS)


async def scrape_ext_proxy_list_page(ctx, url):
    found = set()
    page = await ctx.new_page()
    try:
        print(f"  [PROXY-LIST] Visiting: {url}")
        await page.goto(url, wait_until="domcontentloaded", timeout=20000)
        await page.wait_for_timeout(1500)
        links = await page.eval_on_selector_all(
            "a[href]",
            "nodes => nodes.map(n => ({href: n.href, text: (n.innerText || n.textContent || '').trim().toLowerCase()}))",
        )
        page_is_extish = is_ext_domain(urlparse(url).netloc)
        for link in links:
            href = link.get("href", "")
            text = link.get("text", "")
            normalized = normalize_site_url(href)
            if not normalized:
                continue
            parsed = urlparse(normalized)
            domain = parsed.netloc.lower()
            path = (parsed.path or "").lower()
            if is_ext_domain(domain):
                found.add(normalized)
                continue
            if not page_is_extish:
                continue
            if any(marker in text for marker in ("view full site", "full site", "extra torrent", "ext.to")):
                found.add(normalized)
                continue
            if any(marker in path for marker in ("extratorrent", "extra-torrent", "ext.to")):
                found.add(normalized)
    except Exception as e:
        print(f"  [PROXY-LIST] Error visiting {url}: {e}")
    finally:
        await page.close()
    return found


async def run_search_plan(browser, search_plan):
    all_domains = set()
    proxy_list_result_urls = set()
    stage2_mirrors = set()
    ctx = browser.contexts[0]
    total = sum(pages for _, pages in search_plan)
    pages_done = 0

    for query, max_pages in search_plan:
        is_proxy_query = any(token in query.lower() for token in ("proxy", "mirror", "ext.to", "extratorrent"))
        print(f"\n[YANDEX] === Query: '{query}' ({max_pages} pages) ===")

        for page_idx in range(max_pages):
            pages_done += 1
            url = f"https://yandex.com/search/?text={query}&p={page_idx}"
            print(f"[YANDEX] Page {page_idx + 1}/{max_pages}  (overall {pages_done}/{total})")

            hrefs, _snippet_map, organic_urls = await scrape_yandex_page(ctx, url)
            if hrefs is None:
                print(f"[YANDEX] No more results for '{query}'. Moving on.")
                break

            page_domains = {d for d in extract_domains_from_hrefs(hrefs) if is_ext_domain(urlparse(d).netloc)}
            all_domains.update(page_domains)

            if is_proxy_query and organic_urls:
                for full_url in organic_urls:
                    parsed = urlparse(full_url)
                    if parsed.netloc:
                        proxy_list_result_urls.add(full_url)

            print(
                f"[YANDEX] Found {len(page_domains)} ext candidate domains"
                + (f", {len(organic_urls)} result page URLs" if is_proxy_query else "")
                + "."
            )
            await asyncio.sleep(0.8)

    if proxy_list_result_urls:
        print(f"\n[PROXY-LIST] Visiting {len(proxy_list_result_urls)} proxy-list article pages to harvest embedded ext links...")
        semaphore = asyncio.Semaphore(PROXY_LIST_CONCURRENCY)

        async def scrape_page_limited(site_url):
            async with semaphore:
                extra = await scrape_ext_proxy_list_page(ctx, site_url)
                return site_url, extra

        tasks = [asyncio.create_task(scrape_page_limited(site_url)) for site_url in sorted(proxy_list_result_urls)]
        for task in asyncio.as_completed(tasks):
            site_url, extra = await task
            if extra:
                print(f"  [PROXY-LIST] Found {len(extra)} ext links on {site_url}")
                stage2_mirrors.update(extra)
                all_domains.update(extra)

    return all_domains, stage2_mirrors


async def revisit_cached_domains(browser, cached_domains):
    all_domains = set(cached_domains)
    discovered_mirrors = set()
    ctx = browser.contexts[0]

    if not cached_domains:
        return all_domains, discovered_mirrors

    print(f"\n[CACHED-VISIT] Visiting {len(cached_domains)} cached domains to harvest linked ext mirrors...")
    semaphore = asyncio.Semaphore(PROXY_LIST_CONCURRENCY)

    async def revisit_one(site_url):
        async with semaphore:
            extra = await scrape_ext_proxy_list_page(ctx, site_url)
            return site_url, extra

    tasks = [asyncio.create_task(revisit_one(site_url)) for site_url in sorted(cached_domains)]
    for task in asyncio.as_completed(tasks):
        site_url, extra = await task
        if extra:
            print(f"  [CACHED-VISIT] Found {len(extra)} ext links on {site_url}")
            discovered_mirrors.update(extra)
            all_domains.update(extra)

    return all_domains, discovered_mirrors


async def main():
    parser = argparse.ArgumentParser(description="ext mirror finder")
    parser.add_argument("queries", nargs="*", help="Search terms to run through stage 1 gathering; each term is searched for five pages")
    parser.add_argument(
        "--search-engine",
        choices=("yandex", "duckduckgo", "both"),
        default="both",
        help="Which discovery source to use for stage 1 gathering (default: both)",
    )
    parser.add_argument("--recheck", action="store_true", help="Load existing cached domains without rerunning search gathering")
    parser.add_argument(
        "--visit-cached-domains",
        action="store_true",
        help="Visit each cached domain in the browser and harvest linked ext mirrors without rerunning search scraping",
    )
    args = parser.parse_args()

    print("[*] Initializing ext Site Finder...")

    if not (args.recheck or args.visit_cached_domains):
        if not args.queries:
            parser.error("stage 1 now requires one or more search terms unless --recheck or --visit-cached-domains is used")
        search_plan = [(query, SEARCH_PAGES_PER_QUERY) for query in args.queries]
    else:
        search_plan = []

    all_domains = set()
    stage2_mirrors = set()
    existing_all_domains = set()
    existing_stage2_mirrors = set()

    if (args.recheck or args.visit_cached_domains) and os.path.exists(DOMAIN_CACHE_FILE):
        print(f"[*] --recheck mode: loading domains from {DOMAIN_CACHE_FILE}")
        cache = json.loads(open(DOMAIN_CACHE_FILE).read())
        all_domains = set(cache.get("all_domains", []))
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

        use_yandex = args.search_engine in {"yandex", "both"}
        use_duckduckgo = args.search_engine in {"duckduckgo", "both"}
        print(
            f"[*] Search plan: {len(search_plan)} queries × {SEARCH_PAGES_PER_QUERY} pages each = "
            f"up to {sum(p for _, p in search_plan)} pages per enabled engine ({args.search_engine})."
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
                    all_domains, stage2_mirrors = await run_search_plan(browser, search_plan)

        if use_duckduckgo:
            print("[*] Running DuckDuckGo stage 1 gathering...")
            for query in args.queries:
                print(f"[DUCKDUCKGO] '{query}'")
                all_domains.update({d for d in scrape_duckduckgo_query(query) if is_ext_domain(urlparse(d).netloc)})

        all_domains.update(existing_all_domains)
        stage2_mirrors.update(existing_stage2_mirrors)
    elif args.visit_cached_domains:
        if async_playwright is None:
            raise SystemExit("Playwright is required for --visit-cached-domains")
        if not os.path.exists(CHATBOT_SESSION_FILE):
            raise SystemExit(f"Missing browser session file: {CHATBOT_SESSION_FILE}")
        ws_endpoint = open(CHATBOT_SESSION_FILE).read().strip()
        async with async_playwright() as p:
            browser = await p.chromium.connect_over_cdp(ws_endpoint)
            cache = json.loads(open(DOMAIN_CACHE_FILE).read()) if os.path.exists(DOMAIN_CACHE_FILE) else {}
            cached_domains = cache.get("all_domains", [])
            all_domains, stage2_mirrors = await revisit_cached_domains(browser, cached_domains)

    out_cache = {
        "all_domains": sorted(all_domains),
        "stage2_mirrors": sorted(stage2_mirrors),
    }
    with open(DOMAIN_CACHE_FILE, "w") as f:
        json.dump(out_cache, f, indent=2)
    print(f"[*] Updated scraped domain cache written to {DOMAIN_CACHE_FILE}")

    gathered_domains = sorted(all_domains)
    with open(GATHERED_OUTPUT_FILE, "w") as f:
        f.write(f"# Gathered ext Candidate Links — {len(gathered_domains)} found\n")
        for site in gathered_domains:
            f.write(site + "\n")

    print(f"\n[*] Gathered {len(gathered_domains)} unique candidate domains.")
    print(f"[*] Raw gathered list saved to {GATHERED_OUTPUT_FILE}")
    print("[*] Run verify_ext_search.py to classify searchable mirrors and record failures.")


if __name__ == "__main__":
    asyncio.run(main())
