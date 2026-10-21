#!/usr/bin/env python3
"""Fetch each source and report what the server actually returned.

A link in a note is a claim that the page exists and says what the note says. This checks the first half:
status, final URL, content type and title. Quoting the claim is still the reader's job.
"""

from __future__ import annotations

import argparse
import json
import re
import urllib.error
import urllib.request
from html.parser import HTMLParser

USER_AGENT = "hades-source-check/1.0 (+https://github.com/GuideboardLabs/hades-agent)"


class Title(HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.in_title = False
        self.text = ""

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        if tag.lower() == "title":
            self.in_title = True

    def handle_endtag(self, tag: str) -> None:
        if tag.lower() == "title":
            self.in_title = False

    def handle_data(self, data: str) -> None:
        if self.in_title:
            self.text += data


def check(url: str, timeout: float) -> dict:
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            body = response.read(2_000_000)
            charset = response.headers.get_content_charset() or "utf-8"
            text = body.decode(charset, "replace")
            parser = Title()
            try:
                parser.feed(text)
            except Exception:
                pass
            published = re.search(
                r"""<meta[^>]+(?:property|name)=["'](?:article:published_time|date|pubdate)["'][^>]+content=["']([^"']+)""",
                text,
                re.I,
            )
            return {
                "url": url,
                "ok": True,
                "status": response.status,
                "final_url": response.geturl(),
                "content_type": response.headers.get_content_type(),
                "title": " ".join(parser.text.split())[:240],
                "published": published.group(1) if published else None,
                "bytes": len(body),
            }
    except urllib.error.HTTPError as error:
        return {"url": url, "ok": False, "status": error.code, "error": str(error)}
    except Exception as error:
        return {"url": url, "ok": False, "status": None, "error": str(error)}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("urls", nargs="+")
    parser.add_argument("--timeout", type=float, default=20.0)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    results = [check(url, args.timeout) for url in args.urls]
    if args.json:
        print(json.dumps(results, indent=2))
    else:
        for item in results:
            if item["ok"]:
                print(f"{item['status']} {item['url']} -> {item['final_url']}")
                print(f"  {item['content_type']}  {item['bytes']} bytes  {item['title']}")
                if item["published"]:
                    print(f"  published: {item['published']}")
            else:
                print(f"FAIL {item['url']}: {item['error']}")
    return 0 if all(item["ok"] for item in results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
