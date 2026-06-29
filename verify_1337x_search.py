import argparse
import json
import os
import re
from urllib.parse import urljoin, urlparse

from find_yify_sites import (
    BLOCKLIST,
    FULL_SITE_TEXT_MARKERS,
    SEARCH_PROBE_QUERY,
    SKIP_DOMAINS,
    fetch_url_with_optional_doh,
)

DOMAIN_CACHE_FILE = "/home/lewis/Dev/film/1337x_scraped_domains.json"
SUCCESS_OUTPUT_FILE = "/home/lewis/Dev/film/1337x_mirrors.txt"
REPORT_OUTPUT_FILE = "/home/lewis/Dev/film/1337x_search_report.json"


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
    port = f":{parsed.port}" if parsed.port else ""
    path = parsed.path or ""
    query = f"?{parsed.query}" if parsed.query else ""
    normalized = f"{parsed.scheme}://{hostname}{port}{path}{query}"
    return normalized.rstrip("/") if path not in ("", "/") and not query else normalized


def is_1337x_url(url: str) -> bool:
    host = urlparse(url).netloc.lower()
    return "1337x" in host or "x1337" in host


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
        seen.add(normalized)
        targets.append(normalized)
    return targets


def extract_linked_1337x_targets(base_url, html):
    targets = []
    seen = set()
    for match in re.finditer(r'(?is)<a\b([^>]*)href=["\']([^"\']+)["\']([^>]*)>(.*?)</a>', html):
        href = match.group(2).strip()
        text = re.sub(r"<[^>]+>", " ", match.group(4)).strip().lower()
        normalized = normalize_target_url(base_url, href)
        if not normalized or normalized in seen:
            continue
        if not is_1337x_url(normalized):
            if not any(marker in text for marker in FULL_SITE_TEXT_MARKERS):
                continue
            parsed = urlparse(normalized)
            path_lower = (parsed.path or "").lower()
            if not any(marker in path_lower for marker in ("1337x", "x1337", "mirror", "proxy", "torrent")):
                continue
        if any(blocked in normalized.lower() for blocked in ("telegram.", "t.me/", "/joinchat/", "telegram.dog")):
            continue
        seen.add(normalized)
        targets.append(normalized)
    return targets


def extract_search_candidates(base_url, html, query):
    encoded = query.replace(" ", "+")
    path_encoded = query.replace(" ", "-")
    candidates = []
    seen = set()

    def add_candidate(url, fmt):
        if not url or url in seen:
            return
        seen.add(url)
        candidates.append((url, fmt))

    add_candidate(f"{base_url.rstrip('/')}/search/{encoded}/1/", "/search/{query}/1/")
    add_candidate(f"{base_url.rstrip('/')}/sort-search/{encoded}/time/desc/1/", "/sort-search/{query}/time/desc/1/")
    add_candidate(f"{base_url.rstrip('/')}/category-search/{encoded}/Movies/1/", "/category-search/{query}/Movies/1/")
    add_candidate(f"{base_url.rstrip('/')}/browse/?q={encoded}", "/browse/?q={query}")

    for href in re.findall(r'href=["\']([^"\']+)["\']', html, re.I):
        full = urljoin(f"{base_url.rstrip('/')}/", href)
        href_lower = full.lower()
        if "/search/" in href_lower:
            add_candidate(re.sub(r"/search/[^/]+/\d+/?", f"/search/{encoded}/1/", full, flags=re.I), "/search/{query}/1/")
        if "/sort-search/" in href_lower:
            add_candidate(re.sub(r"/sort-search/[^/]+/[^/]+/[^/]+/\d+/?", f"/sort-search/{encoded}/time/desc/1/", full, flags=re.I), "/sort-search/{query}/time/desc/1/")
        if "/browse/?" in href_lower and "q=" in href_lower:
            parsed = urlparse(full)
            query_string = re.sub(r"([?&]q=)[^&]+", rf"\1{encoded}", parsed.query)
            rebuilt = parsed._replace(query=query_string).geturl()
            add_candidate(rebuilt, "/browse/?q={query}")

    return candidates


def classify_search_response(resp, query):
    if resp is None:
        return False, "request failed"
    if resp.status_code not in (200, 301, 302, 403):
        return False, f"unexpected status code {resp.status_code}"

    body = resp.text
    body_lower = body.lower()
    query_lower = query.lower()
    final_url = getattr(resp, "url", "")
    final_host = urlparse(final_url).netloc.lower() if final_url else ""

    if any(blocked in final_host for blocked in ("telegram.dog", "t.me")):
        return False, "redirected to Telegram instead of a 1337x mirror"

    if final_host and "1337x" not in final_host and "x1337" not in final_host:
        return False, f"redirected to non-1337x host {final_host}"

    if any(marker in body_lower for marker in ("cloudflare", "cf-browser-verification", "just a moment", "captcha")):
        return False, "gateway, parking, or challenge page"

    if (
        "<title>redirecting" in body_lower
        or "<title>loading" in body_lower
        or "window.location.replace(" in body_lower
        or "router.parklogic.com" in body_lower
        or "xmlhttprequest" in body_lower
    ):
        return False, "javascript redirect wrapper instead of a real 1337x results page"

    has_result_shell = (
        "table-list" in body_lower
        or "coll-1 name" in body_lower
        or "/torrent/" in body_lower
    )
    if has_result_shell:
        has_actual_result_rows = (
            "table-list" in body_lower
            and "coll-1 name" in body_lower
            and "/torrent/" in body_lower
        )
        if not has_actual_result_rows:
            return False, "page shell loaded but actual torrent rows were missing"
        if query_lower in body_lower:
            return True, "1337x result table markers found with query present"
        return False, "1337x result table present but query term missing"

    if "/search/" in body_lower or "search torrents" in body_lower or "browse torrents" in body_lower:
        return False, "search shell present but no actual search results detected"

    return False, "not a recognizable 1337x search result page"


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

    candidates = extract_search_candidates(url, homepage_resp.text, query)
    last_reason = "no search candidates discovered"
    successful = []
    for candidate_url, fmt in candidates:
        resp = fetch_url_with_optional_doh(candidate_url, timeout=6.0)
        is_valid, reason = classify_search_response(resp, query)
        if is_valid:
            successful.append({
                "searchable": True,
                "format": fmt,
                "url": candidate_url,
                "status_code": resp.status_code if resp is not None else None,
                "reason": reason,
            })
            continue
        last_reason = f"{fmt}: {reason}"

    if successful:
        preference = {
            "/search/{query}/1/": 0,
            "/category-search/{query}/Movies/1/": 1,
            "/browse/?q={query}": 2,
            "/sort-search/{query}/time/desc/1/": 3,
        }
        successful.sort(key=lambda item: preference.get(item.get("format"), 99))
        return successful[0]

    return {
        "searchable": False,
        "format": candidates[0][1] if candidates else None,
        "url": candidates[0][0] if candidates else url,
        "status_code": None,
        "reason": last_reason,
    }


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

    linked_targets = extract_linked_1337x_targets(domain, homepage_resp.text)
    for target in linked_targets:
        if target == domain:
            continue
        target_probe = probe_site_search(target, query)
        if target_probe and target_probe.get("searchable"):
            target_probe["effective_domain"] = target
            target_probe["source_domain"] = domain
            target_probe["reason"] = f"discovered via linked 1337x mirror on {domain}: {target_probe.get('reason', 'search probe succeeded')}"
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
    if linked_targets:
        discovered_targets.extend([t for t in linked_targets if t not in discovered_targets])
    if discovered_targets:
        probe["discovered_targets"] = discovered_targets
        probe["reason"] = f"{probe.get('reason', 'search probe failed')}; discovered targets tried: {', '.join(discovered_targets)}"
    probe["effective_domain"] = domain
    probe["source_domain"] = domain
    return probe


def main():
    parser = argparse.ArgumentParser(description="Verify gathered 1337x candidate domains for actual searchable torrent pages")
    parser.add_argument("--query", default=SEARCH_PROBE_QUERY, help=f"Probe query used to verify search behavior (default: {SEARCH_PROBE_QUERY})")
    parser.add_argument("--skip-known", action="store_true", help="Skip gathered domains that already have a status entry in 1337x_search_report.json")
    args = parser.parse_args()

    cache = load_stage1_cache()
    all_domains = cache.get("all_domains", [])
    stage2_mirrors = set(cache.get("stage2_mirrors", []))
    existing_report_entries = load_existing_report()
    clean_domains = sorted(set(all_domains))
    loaded_total = len(clean_domains)
    print(f"[*] Loaded {loaded_total} gathered domains from {DOMAIN_CACHE_FILE}")
    if args.skip_known:
        original_total = len(clean_domains)
        clean_domains = [domain for domain in clean_domains if domain not in existing_report_entries]
        print(f"[*] --skip-known reduced work from {original_total} to {len(clean_domains)} domains")

    results = []
    total_domains = len(clean_domains)
    if total_domains == 0:
        print("[*] No domains left to verify in this run.")

    for idx, domain in enumerate(clean_domains, start=1):
        probe = probe_with_full_site_fallback(domain, args.query)
        effective_domain = probe.get("effective_domain", domain) if probe else domain
        inferred = is_1337x_url(domain) or is_1337x_url(effective_domain) or domain in stage2_mirrors
        searchable = bool(probe and probe.get("searchable"))
        reason = probe.get("reason") if probe else "probe returned no result"
        if searchable and not inferred:
            reason = "search probe succeeded but domain was not classified as a 1337x mirror"
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
            "inferred_yify_domain": False,
            "inferred_1337x_domain": inferred,
            "api_supported": False,
        }
        results.append(result)
        verdict = "OK" if result["searchable"] and result["inferred_1337x_domain"] else "FAIL"
        shown_domain = effective_domain if effective_domain != domain else domain
        print(f"[*] [{idx}/{total_domains}] {verdict} {shown_domain} (from {domain}) :: {reason}", flush=True)

    merged_entries = dict(existing_report_entries)
    for result in results:
        merged_entries[result["domain"]] = result

    merged_results = list(merged_entries.values())
    succeeded = sorted(
        [r for r in merged_results if r["searchable"] and r["inferred_1337x_domain"]],
        key=lambda item: item["domain"],
    )
    failed = sorted(
        [r for r in merged_results if not (r["searchable"] and r["inferred_1337x_domain"])],
        key=lambda item: item["domain"],
    )

    merged_success_urls = merge_preserving_order([], [item["effective_domain"] for item in succeeded])

    with open(SUCCESS_OUTPUT_FILE, "w") as f:
        f.write(f"# Searchable 1337x Mirrors — {len(merged_success_urls)} found\n")
        for url in merged_success_urls:
            f.write(url + "\n")

    report = {
        "query": args.query,
        "source_cache": DOMAIN_CACHE_FILE,
        "total_gathered": loaded_total,
        "visited_this_run": len(results),
        "successful_count": len(succeeded),
        "failed_count": len(failed),
        "api_successful_count": 0,
        "successful": succeeded,
        "api_successful": [],
        "failed": failed,
    }

    with open(REPORT_OUTPUT_FILE, "w") as f:
        json.dump(report, f, indent=2)

    print(f"[*] Searchable mirrors written to {SUCCESS_OUTPUT_FILE}")
    print(f"[*] Full verification report written to {REPORT_OUTPUT_FILE}")
    print(f"[*] Search successes: {len(succeeded)} | Failures: {len(failed)}")


if __name__ == "__main__":
    main()
