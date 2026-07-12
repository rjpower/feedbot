#!/usr/bin/env python3
"""
Post-deploy check against a running feedbot.

    FEEDBOT_TOKEN=... scripts/verify.py https://feedbot.rjp.io

Read-only apart from a star toggle it undoes. Asserts the things that have
actually broken before: articles carry titles and dates, the list never leaks
`content_html`, the same article is never stored twice, article HTML is
sanitized, and an exported MOBI is a well-formed Mobipocket book.
"""
import os
import re
import sys
import json
import struct
import urllib.error
import urllib.request

BASE = (sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8000").rstrip("/")
TOKEN = os.environ.get("FEEDBOT_TOKEN", "")

fails = []


def check(name, cond, detail=""):
    print(f"  {'ok  ' if cond else 'FAIL'} {name}" + (f" — {detail}" if detail else ""))
    if not cond:
        fails.append(name)


def call(path, method="GET", body=None, raw=False):
    req = urllib.request.Request(f"{BASE}{path}", method=method)
    if TOKEN:
        req.add_header("x-feedbot-token", TOKEN)
    if body is not None:
        req.add_header("content-type", "application/json")
        req.data = json.dumps(body).encode()
    with urllib.request.urlopen(req, timeout=60) as r:
        data = r.read()
    return data if raw else (json.loads(data) if data else None)


def status(path):
    try:
        req = urllib.request.Request(f"{BASE}{path}")
        if TOKEN:
            req.add_header("x-feedbot-token", TOKEN)
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status
    except urllib.error.HTTPError as e:
        return e.code


print(f"=== {BASE}")
check("healthz", urllib.request.urlopen(f"{BASE}/healthz", timeout=30).read() == b"ok")
check("spa serves index", b"<div id=\"app\">" in urllib.request.urlopen(BASE, timeout=30).read())
check("spa deep link is 200, not 404", status("/read/1") == 200)

if TOKEN:
    print("\n=== auth")
    saved, globals()["TOKEN"] = TOKEN, ""
    check("api rejects a missing token", status("/api/stats") == 401)
    globals()["TOKEN"] = saved
    check("api accepts the token", status("/api/stats") == 200)

print("\n=== content")
stats = call("/api/stats")
print(f"       {stats}")
check("has sites", stats["sites"] > 0)
check("has articles", stats["articles"] > 0)

arts = call("/api/articles?state=all&limit=200")
check("article list is non-empty", len(arts) > 0)
check("every article has a title", all(a["title"].strip() for a in arts))
check("every article has a body", all(a["word_count"] > 0 for a in arts))
check("most articles are dated", sum(1 for a in arts if a["published_at"]) >= 0.9 * len(arts))
check("list omits content_html", all("content_html" not in a for a in arts))
urls = [a["url"] for a in arts]
check("no duplicate urls", len(set(urls)) == len(urls), f"{len(urls) - len(set(urls))} dupes")
# An undated post sorts by when we fetched it, not to the bottom of the inbox
# forever — so this is COALESCE(published_at, fetched_at), as the server orders.
recency = [a["published_at"] or a["fetched_at"] for a in arts]
check("newest first", recency == sorted(recency, reverse=True))

one = call(f"/api/articles/{arts[0]['id']}?state=all")
html = one["content_html"]
check("single article has content", len(html) > 200)
check("no <script> survives sanitizing", "<script" not in html.lower())
check("no event handlers survive", not re.search(r"\son\w+\s*=", html, re.I))
check("no javascript: urls survive", "javascript:" not in html.lower())
check("readability wrapper stripped", 'id="readability-page' not in html)

print("\n=== star toggles cleanly")
aid = arts[0]["id"]
was = arts[0]["starred"]
call(f"/api/articles/{aid}/star", "POST", {"starred": not was})
check("star flipped", call(f"/api/articles/{aid}?state=all")["starred"] is (not was))
call(f"/api/articles/{aid}/star", "POST", {"starred": was})
check("star restored", call(f"/api/articles/{aid}?state=all")["starred"] is was)

print("\n=== mobi")
# A single article, so the check stays quick — the export fetches every image
# through the sidecar, which the whole-list export multiplies by every post.
blob = call(f"/api/articles/{arts[0]['id']}/mobi", raw=True)
# A Mobipocket file is a PalmDB whose type/creator at offset 60 is "BOOKMOBI".
check("mobi is a palmdb book", blob[60:68] == b"BOOKMOBI", f"{len(blob)} bytes, got {blob[60:68]!r}")
# The MOBI header magic sits at the start of the first record.
rec0 = struct.unpack(">I", blob[78:82])[0]
check("has a MOBI header", blob[rec0 + 16:rec0 + 20] == b"MOBI", f"got {blob[rec0 + 16:rec0 + 20]!r}")

print("\n=== crawls")
crawls = call("/api/crawls")
check("crawl history exists", len(crawls) > 0)
failed = [c for c in crawls if c["ok"] is False]
check("no failed crawls", not failed, "; ".join(f"{c['site_name']}: {c['error']}" for c in failed))

print("\n" + (f"{len(fails)} FAILURES: {fails}" if fails else "ALL GREEN"))
sys.exit(1 if fails else 0)
