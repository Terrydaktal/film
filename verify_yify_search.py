import argparse
import asyncio
import concurrent.futures
import json
import os
import re
from urllib.parse import urljoin, urlparse

from find_yify_sites import (
    CHATBOT_SESSION_FILE,
    DOMAIN_CACHE_FILE,
    FULL_SITE_TEXT_MARKERS,
    SEARCH_PROBE_QUERY,
    async_playwright,
    fetch_url_with_optional_doh,
    normalize_site_url,
    probe_site_search,
)

SUCCESS_OUTPUT_FILE = "/home/lewis/Dev/film/yify_mirrors.txt"
API_OUTPUT_FILE = "/home/lewis/Dev/film/yify_api_mirrors.txt"
REPORT_OUTPUT_FILE = "/home/lewis/Dev/film/yify_search_report.json"


def load_stage1_cache():
    with open(DOMAIN_CACHE_FILE, "r") as f:
        return json.load(f)


def extract_full_site_targets(base_url, html):
    targets = []
    seen = set()

    for match in re.finditer(r'(?is)<a\b([^>]*)href=["\']([^"\']+)["\']([^>]*)>(.*?)</a>', html):
        href = match.group(2).strip()
        text = match.group(4).strip().lower()
        if not any(marker in text for marker in FULL_SITE_TEXT_MARKERS):
            continue

        if href.startswith("/"):
            href = f"{base_url.rstrip('/')}{href}"
        normalized = normalize_site_url(href)
        if not normalized or normalized in seen:
            continue
        seen.add(normalized)
        targets.append(normalized)

    return targets


def extract_linked_yts_targets(base_url, html):
    targets = []
    seen = set()

    for match in re.finditer(r'(?is)<a\b([^>]*)href=["\']([^"\']+)["\']([^>]*)>(.*?)</a>', html):
        href = match.group(2).strip()
        if href.startswith("/"):
            href = f"{base_url.rstrip('/')}{href}"
        normalized = normalize_site_url(href)
        if not normalized or normalized in seen:
            continue

        netloc = urlparse(normalized).netloc.lower()
        if "yts" not in netloc and "yify" not in netloc:
            continue

        seen.add(normalized)
        targets.append(normalized)

    return targets


def extract_script_urls(base_url, html):
    script_urls = []
    seen = set()
    for match in re.finditer(r'(?is)<script\b[^>]*src=["\']([^"\']+)["\']', html):
        src = match.group(1).strip()
        if src.startswith("data:") or src.startswith("javascript:") or not src:
            continue
        src = urljoin(f"{base_url.rstrip('/')}/", src)
        if not src.startswith("http"):
            continue
        if src in seen:
            continue
        seen.add(src)
        script_urls.append(src)
    return script_urls


def discover_api_bases_from_text(text):
    candidates = []
    seen = set()
    url_pattern = re.compile(r'https?://[^\s"\'<>\\)]+', re.I)
    hint_tokens = ("api/v2/list_movies", "list_movies.json", "query_term", "movies-api", "/api/v2/")

    for match in url_pattern.finditer(text):
        raw_url = match.group(0).rstrip(".,;)")
        snippet = text[max(0, match.start() - 160):match.end() + 160].lower()
        raw_lower = raw_url.lower()
        if not any(token in raw_lower or token in snippet for token in hint_tokens):
            continue
        normalized = normalize_site_url(raw_url)
        if not normalized or normalized in seen:
            continue
        seen.add(normalized)
        candidates.append(normalized)

    return candidates


def probe_with_full_site_fallback(domain, query):
    direct_probe = probe_site_search(domain, query)
    if direct_probe and direct_probe.get("searchable"):
        direct_probe["effective_domain"] = domain
        direct_probe["source_domain"] = domain
        return direct_probe

    homepage_resp = fetch_url_with_optional_doh(domain, timeout=6.0)
    if homepage_resp is None:
        probe = direct_probe or {}
        probe["effective_domain"] = domain
        probe["source_domain"] = domain
        return probe

    full_site_targets = extract_full_site_targets(domain, homepage_resp.text)
    for target in full_site_targets:
        target_probe = probe_site_search(target, query)
        if target_probe and target_probe.get("searchable"):
            target_probe["effective_domain"] = target
            target_probe["source_domain"] = domain
            target_probe["reason"] = f"discovered via full-site link on {domain}: {target_probe.get('reason', 'search probe succeeded')}"
            return target_probe

    linked_yts_targets = extract_linked_yts_targets(domain, homepage_resp.text)
    for target in linked_yts_targets:
        if target == domain:
            continue
        target_probe = probe_site_search(target, query)
        if target_probe and target_probe.get("searchable"):
            target_probe["effective_domain"] = target
            target_probe["source_domain"] = domain
            target_probe["reason"] = f"discovered via linked yts/yify mirror on {domain}: {target_probe.get('reason', 'search probe succeeded')}"
            return target_probe

    probe = direct_probe or {
        "searchable": False,
        "format": None,
        "url": domain,
        "status_code": homepage_resp.status_code,
        "reason": "probe returned no result",
    }
    discovered_targets = []
    if full_site_targets:
        discovered_targets.extend(full_site_targets)
    if linked_yts_targets:
        discovered_targets.extend([t for t in linked_yts_targets if t not in discovered_targets])
    if discovered_targets:
        probe["discovered_targets"] = discovered_targets
        probe["reason"] = f"{probe.get('reason', 'search probe failed')}; discovered targets tried: {', '.join(discovered_targets)}"
    probe["effective_domain"] = domain
    probe["source_domain"] = domain
    return probe


def probe_site_api(url, query):
    encoded_query = query.replace(" ", "+")
    direct_api_base = normalize_site_url(url) or url.rstrip("/")
    homepage_resp = fetch_url_with_optional_doh(url, timeout=6.0)
    static_candidates = []
    if homepage_resp is not None and homepage_resp.status_code in (200, 301, 302, 403):
        static_candidates.extend(discover_api_bases_from_text(homepage_resp.text))
        for script_url in extract_script_urls(url, homepage_resp.text)[:8]:
            script_resp = fetch_url_with_optional_doh(script_url, timeout=6.0)
            if script_resp is None or script_resp.status_code != 200:
                continue
            static_candidates.extend(discover_api_bases_from_text(script_resp.text))

    unique_candidates = []
    seen = set()
    for candidate in [direct_api_base] + static_candidates:
        normalized = normalize_site_url(candidate)
        if not normalized or normalized in seen:
            continue
        seen.add(normalized)
        unique_candidates.append(normalized)

    last_failure = None
    tried_bases = []
    for candidate_base in unique_candidates:
        api_url = f"{candidate_base.rstrip('/')}/api/v2/list_movies.json?query_term={encoded_query}&limit=1"
        resp = fetch_url_with_optional_doh(api_url, timeout=6.0)
        tried_bases.append(candidate_base)
        source_label = "direct mirror probe" if candidate_base == direct_api_base else f"static HTML/JS discovery from {url}"
        probe = {
            "api_supported": False,
            "api_effective_domain": candidate_base,
            "api_url": api_url,
            "api_status_code": None,
            "api_reason": "",
        }

        if resp is None:
            probe["api_reason"] = f"{source_label}: API request failed or DNS resolution failed"
            last_failure = probe
            continue

        status_code = resp.status_code
        probe["api_status_code"] = status_code
        if status_code != 200:
            probe["api_reason"] = f"{source_label}: API returned status {status_code}"
            last_failure = probe
            continue

        content_type = resp.headers.get("Content-Type", "")
        try:
            payload = resp.json()
        except ValueError:
            body_lower = resp.text.lower()
            if "browse-movie-wrap" in body_lower or "<html" in body_lower:
                reason = "API path returned HTML instead of JSON"
            else:
                reason = "API path returned non-JSON response"
            probe["api_reason"] = f"{source_label}: {reason}"
            last_failure = probe
            continue

        if not isinstance(payload, dict):
            probe["api_reason"] = f"{source_label}: API response JSON was not an object"
            last_failure = probe
            continue

        status_ok = str(payload.get("status", "")).lower() == "ok"
        data = payload.get("data")
        has_movies_key = isinstance(data, dict) and "movies" in data
        if status_ok and has_movies_key:
            probe["api_supported"] = True
            probe["api_reason"] = f"{source_label}: YTS API responded with JSON ({content_type or 'unknown content-type'})"
            return probe

        probe["api_reason"] = f"{source_label}: API JSON did not match YTS list_movies structure"
        last_failure = probe

    if last_failure is None:
        return {
            "api_supported": False,
            "api_effective_domain": direct_api_base,
            "api_url": f"{direct_api_base.rstrip('/')}/api/v2/list_movies.json?query_term={encoded_query}&limit=1",
            "api_status_code": homepage_resp.status_code if homepage_resp is not None else None,
            "api_reason": "No API candidates discovered",
        }

    if len(tried_bases) > 1:
        discovered = ", ".join(base for base in tried_bases if base != direct_api_base)
        if discovered:
            last_failure["discovered_api_candidates"] = [base for base in tried_bases if base != direct_api_base]
            last_failure["api_reason"] = f"{last_failure['api_reason']}; discovered API bases tried: {discovered}"

    return last_failure


async def discover_api_bases_via_browser_async(domain, query):
    if async_playwright is None or not os.path.exists(CHATBOT_SESSION_FILE):
        return []

    ws_endpoint = open(CHATBOT_SESSION_FILE, "r").read().strip()
    if not ws_endpoint:
        return []

    found_bases = []
    seen = set()

    def maybe_add_request_url(request_url):
        request_lower = request_url.lower()
        if "/api/v2/list_movies.json" not in request_lower:
            return
        normalized = normalize_site_url(request_url)
        if not normalized or normalized in seen:
            return
        seen.add(normalized)
        found_bases.append(normalized)

    async with async_playwright() as p:
        browser = await p.chromium.connect_over_cdp(ws_endpoint)
        created_context = None
        try:
            had_context = bool(browser.contexts)
            context = browser.contexts[0] if had_context else await browser.new_context()
            if not had_context:
                created_context = context
            page = await context.new_page()
            page.on("request", lambda request: maybe_add_request_url(request.url))
            try:
                await page.goto(domain, wait_until="domcontentloaded", timeout=15000)
            except Exception:
                await page.close()
                return found_bases

            search_selectors = [
                'input[name="keyword"]',
                'input[name="query_term"]',
                'input[name="query"]',
                'input[name="q"]',
                'input[type="search"]',
                'input[placeholder*="search" i]',
            ]
            for selector in search_selectors:
                locator = page.locator(selector).first
                try:
                    if await locator.count() == 0:
                        continue
                    await locator.fill(query)
                    await locator.press("Enter")
                    await page.wait_for_timeout(2500)
                    break
                except Exception:
                    continue

            await page.wait_for_timeout(1500)
            await page.close()
        finally:
            if created_context is not None:
                await created_context.close()

    return found_bases


def discover_api_bases_via_browser(domain, query):
    try:
        return asyncio.run(discover_api_bases_via_browser_async(domain, query))
    except Exception:
        return []


def main():
    parser = argparse.ArgumentParser(
        description="Verify gathered YIFY/YTS candidate domains for actual searchable movie pages"
    )
    parser.add_argument(
        "--query",
        default=SEARCH_PROBE_QUERY,
        help=f"Movie query used to probe site search behavior (default: {SEARCH_PROBE_QUERY})",
    )
    parser.add_argument(
        "--workers",
        type=int,
        default=20,
        help="Number of parallel verification workers",
    )
    args = parser.parse_args()

    cache = load_stage1_cache()
    all_domains = cache.get("all_domains", [])
    stage2_mirrors = set(cache.get("stage2_mirrors", []))

    clean_domains = sorted(set(all_domains))
    print(f"[*] Loaded {len(clean_domains)} gathered domains from {DOMAIN_CACHE_FILE}")

    results = []
    total_domains = len(clean_domains)
    completed = 0
    print(f"[*] Verifying search behavior for {total_domains} domains with {args.workers} workers...")
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as executor:
        future_to_domain = {
            executor.submit(probe_with_full_site_fallback, domain, args.query): domain
            for domain in clean_domains
        }
        for future in concurrent.futures.as_completed(future_to_domain):
            domain = future_to_domain[future]
            completed += 1
            try:
                probe = future.result()
            except Exception as exc:
                probe = {
                    "searchable": False,
                    "format": None,
                    "url": domain,
                    "status_code": None,
                    "reason": f"unhandled verifier exception: {type(exc).__name__}: {exc}",
                }

            domain_lower = urlparse(domain).netloc.lower()
            effective_domain = probe.get("effective_domain", domain) if probe else domain
            effective_domain_lower = urlparse(effective_domain).netloc.lower()
            inferred_yify = (
                any(x in domain_lower for x in ("yify", "yts"))
                or any(x in effective_domain_lower for x in ("yify", "yts"))
                or domain in stage2_mirrors
            )
            searchable = bool(probe and probe.get("searchable"))
            reason = probe.get("reason") if probe else "probe returned no result"
            if searchable and not inferred_yify:
                reason = "search probe succeeded but domain was not classified as a YIFY/YTS mirror"
            api_probe = probe_site_api(effective_domain, args.query)
            result = {
                "domain": domain,
                "effective_domain": effective_domain,
                "source_domain": probe.get("source_domain", domain) if probe else domain,
                "query": args.query,
                "searchable": searchable,
                "detected_search_format": probe.get("format") if probe else None,
                "sample_search_url": probe.get("url") if probe else None,
                "status_code": probe.get("status_code") if probe else None,
                "reason": reason,
                "inferred_yify_domain": inferred_yify,
                "api_supported": bool(api_probe.get("api_supported")),
                "api_effective_domain": api_probe.get("api_effective_domain", effective_domain),
                "api_url": api_probe.get("api_url"),
                "api_status_code": api_probe.get("api_status_code"),
                "api_reason": api_probe.get("api_reason"),
            }
            results.append(result)
            verdict = "OK" if result["searchable"] and result["inferred_yify_domain"] else "FAIL"
            shown_domain = effective_domain if effective_domain != domain else domain
            print(f"[*] [{completed}/{total_domains}] {verdict} {shown_domain} (from {domain}) :: {reason}", flush=True)

    browser_api_candidates = [
        result for result in results
        if result["searchable"] and result["inferred_yify_domain"] and not result["api_supported"]
    ]
    if browser_api_candidates:
        print(f"[*] Browser API discovery fallback for {len(browser_api_candidates)} searchable mirrors without API support...", flush=True)
        for idx, result in enumerate(browser_api_candidates, start=1):
            domain = result["effective_domain"]
            discovered_bases = discover_api_bases_via_browser(domain, args.query)
            if not discovered_bases:
                continue
            for candidate_base in discovered_bases:
                api_probe = probe_site_api(candidate_base, args.query)
                if not api_probe.get("api_supported"):
                    continue
                result["api_supported"] = True
                result["api_effective_domain"] = api_probe.get("api_effective_domain", candidate_base)
                result["api_url"] = api_probe.get("api_url")
                result["api_status_code"] = api_probe.get("api_status_code")
                result["api_reason"] = (
                    f"browser network capture on {domain} discovered {candidate_base}; "
                    f"{api_probe.get('api_reason', 'API probe succeeded')}"
                )
                print(
                    f"[*] [browser {idx}/{len(browser_api_candidates)}] API OK {result['api_effective_domain']} (discovered from {domain})",
                    flush=True,
                )
                break

    succeeded = sorted(
        [r for r in results if r["searchable"] and r["inferred_yify_domain"]],
        key=lambda item: item["domain"],
    )
    failed = sorted(
        [r for r in results if not (r["searchable"] and r["inferred_yify_domain"])],
        key=lambda item: item["domain"],
    )
    api_succeeded = sorted(
        [r for r in results if r["api_supported"] and r["inferred_yify_domain"]],
        key=lambda item: item["api_effective_domain"],
    )
    unique_api_domains = []
    seen_api = set()
    for item in api_succeeded:
        api_domain = item["api_effective_domain"]
        if api_domain in seen_api:
            continue
        seen_api.add(api_domain)
        unique_api_domains.append(api_domain)

    with open(SUCCESS_OUTPUT_FILE, "w") as f:
        f.write(f"# Searchable YIFY/YTS Mirrors — {len(succeeded)} found\n")
        for item in succeeded:
            f.write(item["effective_domain"] + "\n")

    with open(API_OUTPUT_FILE, "w") as f:
        f.write(f"# YIFY/YTS JSON API Mirrors — {len(unique_api_domains)} found\n")
        for api_domain in unique_api_domains:
            f.write(api_domain + "\n")

    report = {
        "query": args.query,
        "source_cache": DOMAIN_CACHE_FILE,
        "total_gathered": len(clean_domains),
        "successful_count": len(succeeded),
        "failed_count": len(failed),
        "api_successful_count": len(unique_api_domains),
        "successful": succeeded,
        "api_successful": api_succeeded,
        "failed": failed,
    }

    with open(REPORT_OUTPUT_FILE, "w") as f:
        json.dump(report, f, indent=2)

    print(f"[*] Searchable mirrors written to {SUCCESS_OUTPUT_FILE}")
    print(f"[*] JSON API mirrors written to {API_OUTPUT_FILE}")
    print(f"[*] Full verification report written to {REPORT_OUTPUT_FILE}")
    print(f"[*] Search successes: {len(succeeded)} | API successes: {len(unique_api_domains)} | Failures: {len(failed)}")


if __name__ == "__main__":
    main()
