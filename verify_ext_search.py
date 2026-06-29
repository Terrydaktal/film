import argparse
import asyncio
import concurrent.futures
import json
import os
import re
import threading
import time
from urllib.parse import quote_plus, urljoin, urlparse

from find_yify_sites import (
    BLOCKLIST,
    CHATBOT_SESSION_FILE,
    FULL_SITE_TEXT_MARKERS,
    SEARCH_PROBE_QUERY,
    SKIP_DOMAINS,
    async_playwright,
    fetch_url_with_optional_doh,
    normalize_site_url,
)

DOMAIN_CACHE_FILE = "/home/lewis/Dev/film/ext_scraped_domains.json"
SUCCESS_OUTPUT_FILE = "/home/lewis/Dev/film/ext_mirrors.txt"
API_OUTPUT_FILE = "/home/lewis/Dev/film/ext_api_mirrors.txt"
REPORT_OUTPUT_FILE = "/home/lewis/Dev/film/ext_search_report.json"

EXT_HOST_MARKERS = (
    "ext.to",
    "extto",
    "extratorrent",
    "extratorrents",
    "extra-torrent",
)

SEARCH_FORMATS = [
    ("/browse/?q={query}&with_adult=1", lambda base, q: f"{base}/browse/?q={quote_plus(q)}&with_adult=1"),
    ("/browse/?q={query}", lambda base, q: f"{base}/browse/?q={quote_plus(q)}"),
    ("/browse?q={query}&with_adult=1", lambda base, q: f"{base}/browse?q={quote_plus(q)}&with_adult=1"),
    ("/search/{query}", lambda base, q: f"{base}/search/{quote_plus(q)}"),
]

API_PATTERNS = [
    ("/api/v1/search?q={query}&limit=1", lambda base, q: f"{base}/api/v1/search?q={quote_plus(q)}&limit=1"),
    ("/api/search?q={query}&limit=1", lambda base, q: f"{base}/api/search?q={quote_plus(q)}&limit=1"),
    ("/api/torrents/search?q={query}&limit=1", lambda base, q: f"{base}/api/torrents/search?q={quote_plus(q)}&limit=1"),
]

API_PROBE_TIMEOUT = 3.0
HOMEPAGE_TIMEOUT = 6.0
SEARCH_PROBE_TIMEOUT = 6.0
BROWSER_DISCOVERY_TIMEOUT_MS = 12000
BROWSER_SETTLE_TIMEOUT_MS = 2500
MAX_DISCOVERED_TARGETS_PER_DOMAIN = 3
STALL_PROGRESS_INTERVAL = 10.0
DOMAIN_VERIFY_BUDGET = 12.0


def load_stage1_cache():
    with open(DOMAIN_CACHE_FILE, "r") as f:
        return json.load(f)


def load_existing_report():
    if not os.path.exists(REPORT_OUTPUT_FILE):
        return {}
    try:
        with open(REPORT_OUTPUT_FILE, "r") as f:
            report = json.load(f)
    except Exception:
        return {}

    entries = {}
    for item in report.get("successful", []):
        if item.get("domain"):
            entries[item["domain"]] = item
    for item in report.get("failed", []):
        if item.get("domain"):
            entries[item["domain"]] = item
    return entries


def load_existing_url_list(path):
    if not os.path.exists(path):
        return []
    urls = []
    seen = set()
    with open(path, "r") as f:
        for raw_line in f:
            line = raw_line.strip()
            if not line or line.startswith("#") or line in seen:
                continue
            seen.add(line)
            urls.append(line)
    return urls


def merge_preserving_order(existing_urls, new_urls):
    merged = []
    seen = set()
    for url in list(existing_urls) + list(new_urls):
        if not url or url in seen:
            continue
        seen.add(url)
        merged.append(url)
    return merged


def persist_outputs(existing_report_entries, run_results, existing_success_urls, existing_api_urls, query, loaded_total):
    merged_entries = dict(existing_report_entries)
    for result in run_results:
        merged_entries[result["domain"]] = result

    merged_results = list(merged_entries.values())
    succeeded = sorted(
        [r for r in merged_results if r["searchable"] and r["inferred_ext_domain"]],
        key=lambda item: item["domain"],
    )
    api_succeeded = sorted(
        [r for r in merged_results if r.get("api_supported")],
        key=lambda item: item["api_effective_domain"] or item["domain"],
    )
    failed = sorted(
        [r for r in merged_results if not (r["searchable"] and r["inferred_ext_domain"])],
        key=lambda item: item["domain"],
    )

    merged_success_urls = merge_preserving_order(existing_success_urls, [item["effective_domain"] for item in succeeded])
    merged_api_urls = merge_preserving_order(existing_api_urls, [item["api_effective_domain"] for item in api_succeeded])

    with open(SUCCESS_OUTPUT_FILE, "w") as f:
        f.write(f"# Searchable ext Mirrors — {len(merged_success_urls)} found\n")
        for url in merged_success_urls:
            f.write(url + "\n")

    with open(API_OUTPUT_FILE, "w") as f:
        f.write(f"# ext API Mirrors — {len(merged_api_urls)} found\n")
        for url in merged_api_urls:
            f.write(url + "\n")

    report = {
        "query": query,
        "source_cache": DOMAIN_CACHE_FILE,
        "total_gathered": loaded_total,
        "visited_this_run": len(run_results),
        "successful_count": len(succeeded),
        "failed_count": len(failed),
        "api_successful_count": len(api_succeeded),
        "successful": succeeded,
        "api_successful": api_succeeded,
        "failed": failed,
    }

    with open(REPORT_OUTPUT_FILE, "w") as f:
        json.dump(report, f, indent=2)

    return succeeded, api_succeeded, failed


def timeout_for(deadline, default_timeout):
    if deadline is None:
        return default_timeout
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        return None
    return max(0.5, min(default_timeout, remaining))


def is_ext_url(url: str) -> bool:
    lower = url.lower()
    return any(marker in lower for marker in EXT_HOST_MARKERS)


def is_ext_like_target(url: str, link_text: str = "") -> bool:
    parsed = urlparse(url)
    host = (parsed.netloc or "").lower()
    path = (parsed.path or "").lower()
    text = (link_text or "").lower()
    if any(marker in host for marker in EXT_HOST_MARKERS):
        return True
    if any(marker in path for marker in ("extratorrent", "extra-torrent", "ext.to", "extto")):
        return True
    if any(marker in text for marker in ("extratorrent", "extra torrent", "ext.to", "extto")):
        return True
    return False


def normalize_target_url(base_url, href):
    if not href:
        return None
    absolute = urljoin(f"{base_url.rstrip('/')}/", href.strip())
    parsed = urlparse(absolute)
    hostname = parsed.hostname.lower() if parsed.hostname else ""
    if not parsed.scheme or not hostname:
        return None
    if any(skip in hostname for skip in SKIP_DOMAINS) or hostname in BLOCKLIST:
        return None
    if any(blocked in hostname for blocked in ("telegram.", "t.me")):
        return None
    port = f":{parsed.port}" if parsed.port else ""
    path = parsed.path or ""
    query = f"?{parsed.query}" if parsed.query else ""
    normalized = f"{parsed.scheme}://{hostname}{port}{path}{query}"
    return normalized.rstrip("/") if path not in ("", "/") and not query else normalized


def extract_full_site_targets(base_url, html):
    targets = []
    seen = set()
    for match in re.finditer(r'(?is)<a\b([^>]*)href=["\']([^"\']+)["\']([^>]*)>(.*?)</a>', html):
        href = match.group(2).strip()
        text = match.group(4).strip().lower()
        if not any(marker in text for marker in FULL_SITE_TEXT_MARKERS):
            continue
        normalized = normalize_target_url(base_url, href)
        if not normalized or normalized in seen:
            continue
        if not is_ext_like_target(normalized, text):
            continue
        seen.add(normalized)
        targets.append(normalized)
    return targets


def extract_linked_ext_targets(base_url, html):
    targets = []
    seen = set()

    for match in re.finditer(r'(?is)<a\b([^>]*)href=["\']([^"\']+)["\']([^>]*)>(.*?)</a>', html):
        href = match.group(2).strip()
        text = match.group(4).strip().lower()
        normalized = normalize_target_url(base_url, href)
        if not normalized or normalized in seen:
            continue
        if not is_ext_like_target(normalized, text):
            continue
        seen.add(normalized)
        targets.append(normalized)

    return targets


def classify_search_response(resp, query):
    if resp is None:
        return False, "search request failed or DNS resolution failed"

    status_code = getattr(resp, "status_code", None)
    if status_code != 200:
        return False, f"unexpected status code {status_code}"

    body = resp.text
    body_lower = body.lower()
    query_lower = query.lower()
    title = ""
    if "<title>" in body_lower and "</title>" in body_lower:
        start = body_lower.find("<title>") + 7
        end = body_lower.find("</title>", start)
        title = body_lower[start:end].strip()

    if any(marker in body_lower for marker in ("cf-mitigated", "just a moment", "performance and security by cloudflare", "enable javascript and cookies to continue")):
        return False, "cloudflare challenge page"

    if any(marker in body_lower for marker in ("survey-smiles.com", "parklogic", "sedoparking", "this domain may be for sale")):
        return False, "parking, survey, or redirect page"

    if "search extto" in title or "search ext.to" in title or "extra torrent" in title or "ext.to" in title:
        has_torrent_markers = (
            "magnet:?xt=urn:btih:" in body_lower
            or "/torrent/" in body_lower
            or "/browse/" in body_lower
            or "seed" in body_lower and "leech" in body_lower and "size" in body_lower
        )
        if has_torrent_markers and query_lower in body_lower:
            return True, "ext search result markers found with query present"

    if query_lower in body_lower and any(marker in body_lower for marker in ("magnet:?xt=urn:btih:", "/torrent/", "seed", "leech")):
        return True, "generic torrent result markers found with query present"

    if "/browse/" in body_lower or "search torrents" in body_lower or "extratorrent" in body_lower or "ext.to" in body_lower:
        return False, "search shell present but no actual search results detected"

    return False, "not a recognizable ext search result page"


def probe_site_search(url, query=SEARCH_PROBE_QUERY, deadline=None):
    homepage_timeout = timeout_for(deadline, HOMEPAGE_TIMEOUT)
    if homepage_timeout is None:
        return {
            "searchable": False,
            "format": None,
            "url": url,
            "status_code": None,
            "reason": "per-domain time budget exhausted before homepage probe",
        }

    homepage_resp = fetch_url_with_optional_doh(url, timeout=homepage_timeout)
    if homepage_resp is None:
        return {
            "searchable": False,
            "format": None,
            "url": url,
            "status_code": None,
            "reason": "homepage request failed or DNS resolution failed",
        }

    homepage_final_url = getattr(homepage_resp, "url", url)
    homepage_body = homepage_resp.text

    last_reason = "no search candidates discovered"
    successful = []
    for fmt, builder in SEARCH_FORMATS:
        candidate_timeout = timeout_for(deadline, SEARCH_PROBE_TIMEOUT)
        if candidate_timeout is None:
            last_reason = "per-domain time budget exhausted during search probes"
            break
        candidate_url = builder(url.rstrip("/"), query)
        resp = fetch_url_with_optional_doh(candidate_url, timeout=candidate_timeout)
        is_valid, reason = classify_search_response(resp, query)
        if is_valid:
            successful.append({
                "searchable": True,
                "format": fmt,
                "url": candidate_url,
                "status_code": resp.status_code if resp is not None else None,
                "reason": reason,
                "_homepage_final_url": homepage_final_url,
                "_homepage_body": homepage_body,
            })
            continue
        last_reason = f"{fmt}: {reason}"

    if successful:
        return successful[0]

    return {
        "searchable": False,
        "format": SEARCH_FORMATS[0][0],
        "url": SEARCH_FORMATS[0][1](url.rstrip("/"), query),
        "status_code": getattr(homepage_resp, "status_code", None),
        "reason": last_reason,
        "_homepage_final_url": homepage_final_url,
        "_homepage_body": homepage_body,
    }


def probe_with_full_site_fallback(domain, query, deadline=None):
    direct_probe = probe_site_search(domain, query, deadline=deadline)
    if direct_probe and direct_probe.get("searchable"):
        direct_probe["effective_domain"] = domain
        direct_probe["source_domain"] = domain
        return direct_probe

    homepage_body = direct_probe.get("_homepage_body") if direct_probe else None
    final_url = direct_probe.get("_homepage_final_url", domain) if direct_probe else domain
    if homepage_body is None:
        probe = direct_probe or {}
        probe["effective_domain"] = domain
        probe["source_domain"] = domain
        if probe.get("reason") == "homepage request failed or DNS resolution failed":
            return probe
        homepage_timeout = timeout_for(deadline, HOMEPAGE_TIMEOUT)
        if homepage_timeout is None:
            probe["reason"] = "per-domain time budget exhausted before fallback homepage probe"
            return probe
        homepage_resp = fetch_url_with_optional_doh(domain, timeout=homepage_timeout)
        if homepage_resp is None:
            return probe
        final_url = getattr(homepage_resp, "url", domain)
        homepage_body = homepage_resp.text

    if final_url and not is_ext_url(final_url):
        probe = direct_probe or {
            "searchable": False,
            "format": None,
            "url": domain,
            "status_code": None,
            "reason": f"redirected to non-ext host {urlparse(final_url).netloc.lower()}",
        }
        probe["effective_domain"] = domain
        probe["source_domain"] = domain
        return probe

    full_site_targets = extract_full_site_targets(domain, homepage_body)[:MAX_DISCOVERED_TARGETS_PER_DOMAIN]
    for target in full_site_targets:
        target_probe = probe_site_search(target, query, deadline=deadline)
        if target_probe and target_probe.get("searchable"):
            target_probe["effective_domain"] = target
            target_probe["source_domain"] = domain
            target_probe["reason"] = f"discovered via full-site link on {domain}: {target_probe.get('reason', 'search probe succeeded')}"
            return target_probe

    linked_targets = extract_linked_ext_targets(domain, homepage_body)[:MAX_DISCOVERED_TARGETS_PER_DOMAIN]
    for target in linked_targets:
        if target == domain:
            continue
        target_probe = probe_site_search(target, query, deadline=deadline)
        if target_probe and target_probe.get("searchable"):
            target_probe["effective_domain"] = target
            target_probe["source_domain"] = domain
            target_probe["reason"] = f"discovered via linked ext mirror on {domain}: {target_probe.get('reason', 'search probe succeeded')}"
            return target_probe

        probe = direct_probe or {
            "searchable": False,
            "format": None,
            "url": domain,
            "status_code": None,
            "reason": "probe returned no result",
        }
    discovered_targets = []
    if full_site_targets:
        discovered_targets.extend(full_site_targets)
    if linked_targets:
        discovered_targets.extend([t for t in linked_targets if t not in discovered_targets])
    if discovered_targets:
        probe["discovered_targets"] = discovered_targets
        probe["reason"] = f"{probe.get('reason', 'search probe failed')}; discovered targets tried: {', '.join(discovered_targets)}"
    probe["effective_domain"] = domain
    probe["source_domain"] = domain
    return probe


def parse_generic_api_payload(payload, query):
    query_lower = query.lower()
    if not isinstance(payload, dict):
        return False
    items = None
    for key in ("results", "data", "items", "torrents"):
        value = payload.get(key)
        if isinstance(value, list):
            items = value
            break
        if isinstance(value, dict):
            for nested_key in ("results", "items", "torrents"):
                nested = value.get(nested_key)
                if isinstance(nested, list):
                    items = nested
                    break
        if items is not None:
            break
    if items is None:
        return False
    for item in items:
        if not isinstance(item, dict):
            continue
        title = ""
        for key in ("title", "name", "filename"):
            value = item.get(key)
            if isinstance(value, str):
                title = value.strip()
                break
        if not title or query_lower not in title.lower():
            continue
        hash_value = ""
        for key in ("infohash", "infoHash", "hash"):
            value = item.get(key)
            if isinstance(value, str):
                hash_value = "".join(ch for ch in value if ch.isalnum())
                break
        magnet = item.get("magnet")
        if len(hash_value) == 40 or (isinstance(magnet, str) and "magnet:?xt=urn:btih:" in magnet.lower()):
            return True
    return False


def probe_site_api(url, query, deadline=None):
    last_failure = None
    tried = []
    for fmt, builder in API_PATTERNS:
        api_timeout = timeout_for(deadline, API_PROBE_TIMEOUT)
        if api_timeout is None:
            break
        api_url = builder(url.rstrip("/"), query)
        tried.append(api_url)
        resp = fetch_url_with_optional_doh(api_url, timeout=api_timeout)
        probe = {
            "api_supported": False,
            "api_effective_domain": normalize_site_url(url) or url.rstrip("/"),
            "api_url": api_url,
            "api_status_code": None,
            "api_reason": "",
        }
        if resp is None:
            probe["api_reason"] = f"{fmt}: API request failed or DNS resolution failed"
            last_failure = probe
            continue
        probe["api_status_code"] = resp.status_code
        if resp.status_code != 200:
            probe["api_reason"] = f"{fmt}: API returned status {resp.status_code}"
            last_failure = probe
            continue
        try:
            payload = resp.json()
        except ValueError:
            probe["api_reason"] = f"{fmt}: API path returned non-JSON response"
            last_failure = probe
            continue
        if parse_generic_api_payload(payload, query):
            probe["api_supported"] = True
            probe["api_reason"] = f"{fmt}: generic torrent search API responded with matching JSON results"
            return probe
        probe["api_reason"] = f"{fmt}: JSON response did not contain recognizable torrent matches"
        last_failure = probe

    if last_failure is None:
        return {
            "api_supported": False,
            "api_effective_domain": normalize_site_url(url) or url.rstrip("/"),
            "api_url": None,
            "api_status_code": None,
            "api_reason": "No API candidates discovered before per-domain time budget expired",
        }
    if len(tried) > 1:
        last_failure["discovered_api_candidates"] = tried
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
        if "/api/" not in request_lower:
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
            target_url = f"{domain.rstrip('/')}/browse/?q={quote_plus(query)}&with_adult=1"
            try:
                await page.goto(target_url, wait_until="domcontentloaded", timeout=BROWSER_DISCOVERY_TIMEOUT_MS)
                await page.wait_for_timeout(BROWSER_SETTLE_TIMEOUT_MS)
            except Exception:
                await page.close()
                return found_bases
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


def start_skip_listener():
    skip_event = threading.Event()

    def _listen():
        while True:
            try:
                line = input()
            except EOFError:
                return
            if line == "":
                skip_event.set()

    thread = threading.Thread(target=_listen, daemon=True)
    thread.start()
    return skip_event


def verify_domain(domain, query, stage2_mirrors):
    deadline = time.monotonic() + DOMAIN_VERIFY_BUDGET
    try:
        probe = probe_with_full_site_fallback(domain, query, deadline=deadline)
    except Exception as exc:
        probe = {
            "searchable": False,
            "format": None,
            "url": domain,
            "status_code": None,
            "reason": f"unhandled verifier exception: {type(exc).__name__}: {exc}",
        }

    effective_domain = probe.get("effective_domain", domain) if probe else domain
    inferred_ext = is_ext_url(domain) or is_ext_url(effective_domain) or domain in stage2_mirrors
    searchable = bool(probe and probe.get("searchable"))
    reason = probe.get("reason") if probe else "probe returned no result"
    if searchable and not inferred_ext:
        reason = "search probe succeeded but domain was not classified as an ext mirror"
    api_probe = probe_site_api(effective_domain, query, deadline=deadline)
    return {
        "domain": domain,
        "effective_domain": effective_domain,
        "source_domain": probe.get("source_domain", domain) if probe else domain,
        "query": query,
        "searchable": searchable,
        "detected_search_format": probe.get("format") if probe else None,
        "sample_search_url": probe.get("url") if probe else None,
        "status_code": probe.get("status_code") if probe else None,
        "reason": reason,
        "inferred_yify_domain": False,
        "inferred_1337x_domain": False,
        "inferred_ext_domain": inferred_ext,
        "api_supported": bool(api_probe.get("api_supported")),
        "api_effective_domain": api_probe.get("api_effective_domain", effective_domain),
        "api_url": api_probe.get("api_url"),
        "api_status_code": api_probe.get("api_status_code"),
        "api_reason": api_probe.get("api_reason"),
    }


def main():
    parser = argparse.ArgumentParser(description="Verify gathered ext candidate domains for actual searchable torrent pages")
    parser.add_argument("--query", default="house of the dragon", help="Probe query used to verify search behavior")
    parser.add_argument("--workers", type=int, default=12, help="Number of parallel verification workers")
    parser.add_argument("--skip-known", action="store_true", help="Skip gathered domains that already have a status entry in ext_search_report.json")
    parser.add_argument(
        "--skip-browser-api-discovery",
        action="store_true",
        help="Skip the slower browser network-capture pass for searchable mirrors that still lack API support",
    )
    args = parser.parse_args()

    cache = load_stage1_cache()
    all_domains = cache.get("all_domains", [])
    stage2_mirrors = set(cache.get("stage2_mirrors", []))
    existing_report_entries = load_existing_report()
    existing_success_urls = load_existing_url_list(SUCCESS_OUTPUT_FILE)
    existing_api_urls = load_existing_url_list(API_OUTPUT_FILE)

    clean_domains = sorted(set(all_domains))
    loaded_total = len(clean_domains)
    print(f"[*] Loaded {loaded_total} gathered domains from {DOMAIN_CACHE_FILE}")
    if args.skip_known:
        original_total = len(clean_domains)
        clean_domains = [domain for domain in clean_domains if domain not in existing_report_entries]
        print(f"[*] --skip-known reduced work from {original_total} to {len(clean_domains)} domains")

    results = []
    total_domains = len(clean_domains)
    completed = 0
    if total_domains == 0:
        print("[*] No domains left to verify in this run.")
    print(f"[*] Verifying search behavior for {total_domains} domains with {args.workers} workers...")
    executor = concurrent.futures.ThreadPoolExecutor(max_workers=args.workers)
    try:
        future_to_domain = {
            executor.submit(verify_domain, domain, args.query, stage2_mirrors): domain
            for domain in clean_domains
        }
        pending = set(future_to_domain)
        last_progress_at = time.monotonic()
        while pending:
            done, pending = concurrent.futures.wait(
                pending,
                timeout=0.5,
                return_when=concurrent.futures.FIRST_COMPLETED,
            )
            if not done:
                now = time.monotonic()
                if now - last_progress_at >= STALL_PROGRESS_INTERVAL:
                    pending_domains = sorted(future_to_domain[future] for future in pending)
                    preview = ", ".join(pending_domains[:5])
                    more = "" if len(pending_domains) <= 5 else f" ... +{len(pending_domains) - 5} more"
                    print(
                        f"[*] No completed domains for {int(STALL_PROGRESS_INTERVAL)}s. Pending: {preview}{more}",
                        flush=True,
                    )
                    last_progress_at = now
                continue

            last_progress_at = time.monotonic()
            for future in done:
                domain = future_to_domain[future]
                completed += 1
                try:
                    result = future.result()
                except Exception as exc:
                    result = {
                        "domain": domain,
                        "effective_domain": domain,
                        "source_domain": domain,
                        "query": args.query,
                        "searchable": False,
                        "detected_search_format": None,
                        "sample_search_url": domain,
                        "status_code": None,
                        "reason": f"unhandled verifier exception: {type(exc).__name__}: {exc}",
                        "inferred_yify_domain": False,
                        "inferred_1337x_domain": False,
                        "inferred_ext_domain": is_ext_url(domain) or domain in stage2_mirrors,
                        "api_supported": False,
                        "api_effective_domain": domain,
                        "api_url": None,
                        "api_status_code": None,
                        "api_reason": "verifier failed before API probe",
                    }
                results.append(result)
                verdict = "OK" if result["searchable"] and result["inferred_ext_domain"] else "FAIL"
                effective_domain = result["effective_domain"]
                shown_domain = effective_domain if effective_domain != domain else domain
                print(f"[*] [{completed}/{total_domains}] {verdict} {shown_domain} (from {domain}) :: {result['reason']}", flush=True)
                persist_outputs(
                    existing_report_entries,
                    results,
                    existing_success_urls,
                    existing_api_urls,
                    args.query,
                    loaded_total,
                )
    except KeyboardInterrupt:
        print("[*] Interrupt received. Writing partial results and exiting...", flush=True)
        persist_outputs(
            existing_report_entries,
            results,
            existing_success_urls,
            existing_api_urls,
            args.query,
            loaded_total,
        )
        executor.shutdown(wait=False, cancel_futures=True)
        return
    finally:
        executor.shutdown(wait=False, cancel_futures=True)

    browser_api_candidates = [
        result for result in results
        if result["searchable"] and result["inferred_ext_domain"] and not result["api_supported"]
    ]
    if browser_api_candidates and not args.skip_browser_api_discovery:
        print(f"[*] Browser API discovery fallback for {len(browser_api_candidates)} searchable mirrors without API support...", flush=True)
        print("[*] Press Enter to skip the current browser-probed mirror.", flush=True)
        skip_event = start_skip_listener()
        for idx, result in enumerate(browser_api_candidates, start=1):
            domain = result["effective_domain"]
            if skip_event.is_set():
                skip_event.clear()
            print(f"[*] [browser {idx}/{len(browser_api_candidates)}] probing {domain}...", flush=True)
            discovered_bases = discover_api_bases_via_browser(domain, args.query)
            if skip_event.is_set():
                skip_event.clear()
                print(f"[*] [browser {idx}/{len(browser_api_candidates)}] skipped {domain}", flush=True)
                continue
            if not discovered_bases:
                print(f"[*] [browser {idx}/{len(browser_api_candidates)}] no API requests discovered on {domain}", flush=True)
                continue
            for candidate_base in discovered_bases:
                if skip_event.is_set():
                    skip_event.clear()
                    print(f"[*] [browser {idx}/{len(browser_api_candidates)}] skipped {domain}", flush=True)
                    break
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
                persist_outputs(
                    existing_report_entries,
                    results,
                    existing_success_urls,
                    existing_api_urls,
                    args.query,
                    loaded_total,
                )
                break
            else:
                print(f"[*] [browser {idx}/{len(browser_api_candidates)}] discovered API-looking requests on {domain}, but none validated", flush=True)

    succeeded, api_succeeded, failed = persist_outputs(
        existing_report_entries,
        results,
        existing_success_urls,
        existing_api_urls,
        args.query,
        loaded_total,
    )

    print(f"[*] Searchable mirrors written to {SUCCESS_OUTPUT_FILE}")
    print(f"[*] API mirrors written to {API_OUTPUT_FILE}")
    print(f"[*] Full verification report written to {REPORT_OUTPUT_FILE}")
    print(f"[*] Search successes: {len(succeeded)} | API successes: {len(api_succeeded)} | Failures: {len(failed)}")


if __name__ == "__main__":
    main()
