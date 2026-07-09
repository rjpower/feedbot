# feedbot

An RSS-shaped reading room for blogs, without depending on anyone's RSS.

feedbot visits a list of sites on a schedule, works out which links are
top-level articles, renders each one in a real browser, extracts the prose with
Readability, and files it in an inbox you can read on a screen or export to an
e-reader as EPUB.

Running at [feedbot.rjp.io](https://feedbot.rjp.io).

## How a crawl works

Each site is visited every `interval_secs` (24 hours by default). One crawl:

1. **Renders the front page** in Chromium and collects every `<a href>` plus any
   `<link rel=alternate>` feed. A feed found this way is remembered.
2. **Reads the feed**, if there is one. Feeds give clean titles and real
   publication dates, but they are not sufficient on their own: the CRPG
   Addict's Atom feed serves only 3 posts.
3. **Unions the two** and keeps the links that look like articles. Candidates
   are deduplicated by a key that ignores scheme, `www.`, tracking parameters,
   comment anchors, and trailing slashes — so the feed's
   `http://blog/2026/07/post.html` and the index page's
   `https://blog/2026/07/post.html?showComment=1#c58` are one article.
4. **Sorts newest-first** — using the date in the permalink when there is no
   better source — and fetches at most `max_new_per_crawl` (25) new ones, pausing
   between each. A blog with a deep archive backfills a slice per crawl rather
   than hammering the site once.
5. **Extracts and sanitizes** each article, then stores it.

### What counts as an article

Every candidate must be on the same site (ignoring `www.`), be `http(s)`, not
be an index page (`/tag/`, `/category/`, `/page/`, `/search`, `/feed`,
comments…), and not be a static asset. Feed entries stop there — a feed is the
site asserting these are its posts.

Links scraped off the front page must additionally match the site's
`url_pattern` if it has one, or else the built-in heuristic: a dated permalink
(`/2026/07/some-slug`, `/2026/07/03/some-slug/`, `/2026/07/some-slug.html`) or a
bare slug with at least three words. Both seeded blog engines use dated
permalinks, and the heuristic is what separates a post from an image attachment
page, a label index, or the day archive at `/2026/07/03/` that serves the
post's full text and would otherwise be stored as a duplicate.

Setting a `url_pattern` replaces the heuristic, not the safety rules: a pattern
can never authorize leaving the site or fetching an asset.

## Layout

| Path              | What                                                             |
| ----------------- | ---------------------------------------------------------------- |
| `server/`         | Rust: HTTP API, SQLite, crawl policy, scheduler, EPUB export.     |
| `fetcher/`        | Node: the only thing that touches the network. Playwright + Readability. |
| `web/`            | Vue 3 + Vite. Inbox, reader, site management.                     |

The Rust server owns policy and storage; the sidecar owns the network. Every
outbound URL passes through one SSRF guard that refuses private, loopback, and
link-local addresses, so a site added through the UI cannot be used to probe the
host. The server spawns and supervises the sidecar, restarting it if Chromium
dies.

Article HTML is sanitized with `ammonia` before it is stored, because the reader
renders it with `v-html`.

## Configuration

All optional except the token, which you want.

| Variable                      | Default                   | Meaning                                     |
| ----------------------------- | ------------------------- | ------------------------------------------- |
| `FEEDBOT_TOKEN`               | *(unset)*                 | Gate the API. Unset means **wide open**.    |
| `FEEDBOT_DB`                  | `/data/feedbot.db`        | SQLite file.                                |
| `FEEDBOT_STATIC`              | `/app/static`             | The built Vue bundle.                       |
| `FEEDBOT_PORT`                | `8000`                    |                                             |
| `FEEDBOT_FETCHER_SCRIPT`      | `/app/fetcher/server.mjs` | Empty means "don't spawn one" (local dev).  |
| `FEEDBOT_FETCHER_URL`         | `http://127.0.0.1:4000`   |                                             |
| `FEEDBOT_CRAWL_DELAY_MS`      | `1500`                    | Pause between article fetches.              |
| `FEEDBOT_SCHEDULER_TICK_SECS` | `300`                     | How often to look for due sites.            |

The token is sent as an `x-feedbot-token` header, or as a `?token=` query
parameter so an e-reader can pull an `.epub` from a plain URL.

## Deploy

feedbot is a [halcyon](https://github.com/rjpower/halcyon) app: no published
ports, reachable only through the front-door Caddy on the shared `web` network.

```sh
$EDITOR ~/code/halcyon/secrets/feedbot.env    # FEEDBOT_TOKEN=...
cd ~/code/halcyon && bin/sync feedbot
```

## Development

```sh
make install
cd fetcher && npx playwright install chromium   # once
make fetcher    # terminal 1 — sidecar on :4000
make server     # terminal 2 — api on :8099
make web        # terminal 3 — vite on :5173, proxying /api
make test
```

`make test-fetcher` hits the real blogs over the network; it asserts the things
the crawler depends on (a feed is found, entries have dates, an article has a
title and a body, the SSRF guard refuses private addresses).

## Reading

The inbox is a numbered index; opening an article marks it read. In the reader,
`j`/`k` move between articles, `s` stars, `esc` returns to the inbox. Three
themes (paper, sepia, ink) and a font-size control persist locally. Any article,
or a whole filtered list, downloads as EPUB.
