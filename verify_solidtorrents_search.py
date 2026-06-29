import json
import os
import sys
import urllib.request
import urllib.parse

API_MIRRORS_FILE = "/home/lewis/Dev/film/solidtorrents_api_mirrors.txt"
HTML_MIRRORS_FILE = "/home/lewis/Dev/film/solidtorrents_mirrors.txt"
REPORT_OUTPUT_FILE = "/home/lewis/Dev/film/solidtorrents_search_report.json"

def test_mirror(url, is_api=True):
    test_query = "house of the dragon"
    if is_api:
        endpoint = f"{url.rstrip('/')}/api/v1/search?q={urllib.parse.quote(test_query)}&limit=1"
    else:
        endpoint = f"{url.rstrip('/')}/"
        
    try:
        req = urllib.request.Request(
            endpoint,
            headers={'User-Agent': 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36'}
        )
        with urllib.request.urlopen(req, timeout=5) as response:
            if response.status == 200:
                body = response.read().decode('utf-8')
                if is_api:
                    data = json.loads(body)
                    if data.get("success") == True:
                        return True, "Working API", url
                    else:
                        return False, "API status is not success", url
                else:
                    if "solidtorrents" in body.lower() or "search" in body.lower():
                        return True, "Working HTML", url
                    else:
                        return False, "Missing HTML elements", url
            else:
                return False, f"Status code {response.status}", url
    except Exception as e:
        return False, str(e), url

def main():
    api_successful = []
    successful = []
    failed = []

    # Read api mirrors
    api_mirrors = []
    if os.path.exists(API_MIRRORS_FILE):
        with open(API_MIRRORS_FILE, 'r') as f:
            api_mirrors = [line.strip() for line in f if line.strip() and not line.strip().startswith('#')]

    # Read html mirrors
    html_mirrors = []
    if os.path.exists(HTML_MIRRORS_FILE):
        with open(HTML_MIRRORS_FILE, 'r') as f:
            html_mirrors = [line.strip() for line in f if line.strip() and not line.strip().startswith('#')]

    print("Testing SolidTorrents API mirrors...")
    for m in api_mirrors:
        ok, reason, eff_url = test_mirror(m, is_api=True)
        entry = {
            "domain": m,
            "effective_domain": eff_url,
            "reason": reason,
            "detected_search_format": "JSON",
            "sample_search_url": f"{m.rstrip('/')}/api/v1/search?q=house+of+the+dragon",
            "query": "house of the dragon"
        }
        if ok:
            api_successful.append(entry)
        else:
            failed.append(entry)

    print("Testing SolidTorrents HTML mirrors...")
    for m in html_mirrors:
        ok, reason, eff_url = test_mirror(m, is_api=False)
        entry = {
            "domain": m,
            "effective_domain": eff_url,
            "reason": reason,
            "detected_search_format": "HTML",
            "sample_search_url": f"{m.rstrip('/')}/",
            "query": "house of the dragon"
        }
        if ok:
            successful.append(entry)
        else:
            # Only add to failed if it wasn't already marked failed in API
            if not any(f_entry["domain"] == m for f_entry in failed):
                failed.append(entry)

    report = {
        "api_successful": api_successful,
        "successful": successful,
        "failed": failed
    }

    with open(REPORT_OUTPUT_FILE, 'w') as f:
        json.dump(report, f, indent=4)
        
    print(f"Diagnostics completed. Report written to {REPORT_OUTPUT_FILE}")

if __name__ == "__main__":
    main()
