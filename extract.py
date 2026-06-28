import asyncio
import re
from playwright.async_api import async_playwright

async def extract_video_links(url):
    video_urls = set()
    
    async with async_playwright() as p:
        # Launch browser with options to blend in
        browser = await p.chromium.launch(
            headless=True,
            args=[
                "--no-sandbox",
                "--disable-dev-shm-usage",
                "--disable-blink-features=AutomationControlled"
            ]
        )
        
        # Create a new context with a generic user-agent
        context = await browser.new_context(
            user_agent="Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
        )
        page = await context.new_page()

        # Listen to network responses to capture dynamic stream requests
        video_extensions = ('.mp4', '.m3u8', '.ts', '.webm', '.mkv', '.flv', '.avi', '.mov')
        
        def handle_response(response):
            try:
                res_url = response.url
                url_lower = res_url.lower()
                
                # Check extension
                if any(url_lower.split('?')[0].endswith(ext) for ext in video_extensions):
                    video_urls.add(res_url)
                    return
                
                # Check headers
                headers = response.headers
                content_type = headers.get("content-type", "").lower()
                if "video/" in content_type or "application/vnd.apple.mpegurl" in content_type or "application/x-mpegurl" in content_type:
                    video_urls.add(res_url)
            except Exception:
                pass

        page.on("response", handle_response)

        try:
            print(f"Loading page: {url}")
            await page.goto(url, wait_until="domcontentloaded", timeout=30000)
            await page.wait_for_timeout(3000)  # Wait for initial scripts

            # Targeted selectors for individual server buttons
            selectors = [
                "a.sv-item",
                "[data-srv]",
                "[data-id]"
            ]
            
            elements_to_click = []
            for sel in selectors:
                try:
                    found = await page.query_selector_all(sel)
                    if found:
                        for elem in found:
                            # Verify it has a valid data-id containing HTTP link
                            data_id = await elem.get_attribute("data-id")
                            if data_id and data_id.startswith("http"):
                                elements_to_click.append(elem)
                except Exception:
                    pass
            
            # Click unique elements found
            clicked_servers = set()
            for elem in elements_to_click:
                try:
                    srv_name = await elem.get_attribute("data-srv")
                    data_id = await elem.get_attribute("data-id")
                    
                    identifier = srv_name or data_id
                    if identifier and identifier not in clicked_servers:
                        clicked_servers.add(identifier)
                        print(f"Clicking server: '{srv_name}' -> Embed Link: {data_id}")
                        await elem.click(timeout=5000)
                        # Wait for player to load/request to trigger
                        await page.wait_for_timeout(4000)
                except Exception:
                    pass

            # Fallback check for any iframes
            iframes = page.frames
            for iframe in iframes:
                if iframe.url and iframe.url != "about:blank":
                    print(f"Detected iframe source: {iframe.url}")

            # Allow some extra time for media requests to complete
            await page.wait_for_timeout(6000)

            # Scan the page content (including iframes) for static media links
            for frame in page.frames:
                try:
                    frame_content = await frame.content()
                    media_patterns = [
                        r'(https?://[^\s"\'<>]+?\.(?:mp4|m3u8|ts|webm|mkv|flv|avi|mov)(?:\?[^\s"\']*)?)',
                        r'(https?://[^\s"\'<>]+?(?:video|stream|cdn|play)[^\s"\'<>]*\.(?:mp4|m3u8|ts|webm)[^\s"\'<>]*)'
                    ]
                    for pattern in media_patterns:
                        matches = re.findall(pattern, frame_content, re.IGNORECASE)
                        for match in matches:
                            video_urls.add(match)
                except Exception:
                    pass

        except Exception as e:
            print(f"Navigation/extraction error: {e}")
        finally:
            await browser.close()
    return list(video_urls)


if __name__ == "__main__":
    import sys
    target = sys.argv[1] if len(sys.argv) > 1 else "https://example.com"
    
    links = asyncio.run(extract_video_links(target))
    print("\n" + "="*50)
    print(f"Found {len(links)} resource url(s):")
    print("="*50)
    for i, link in enumerate(links, 1):
        print(f"{i}. {link}")
