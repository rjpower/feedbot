// feedbot's fetch sidecar.
//
// The Rust server owns policy and storage; this process owns the network. It
// keeps one warm Chromium around and exposes three things over loopback HTTP:
//
//   POST /discover  { url }  -> page title, <link rel=alternate> feeds, every <a href>
//   POST /feed      { url }  -> entries parsed out of an RSS 2.0 or Atom document
//   POST /article   { url }  -> Readability's take on the page
//
// Every outbound URL passes through assertPublicUrl() first, so the whole
// process has exactly one place where a hostname turns into a socket.

import http from "node:http";
import dns from "node:dns/promises";
import net from "node:net";
import { chromium } from "playwright";
import { JSDOM } from "jsdom";
import { Readability } from "@mozilla/readability";
import { XMLParser } from "fast-xml-parser";

const PORT = Number(process.env.FETCHER_PORT || 4000);
const HOST = process.env.FETCHER_HOST || "127.0.0.1";
const NAV_TIMEOUT_MS = Number(process.env.FETCHER_NAV_TIMEOUT_MS || 45_000);
const SETTLE_TIMEOUT_MS = Number(process.env.FETCHER_SETTLE_TIMEOUT_MS || 5_000);
const MAX_BODY_BYTES = 8 * 1024 * 1024;

const log = (...a) => console.log(new Date().toISOString(), ...a);

// ---------------------------------------------------------------------------
// SSRF guard
// ---------------------------------------------------------------------------

/** Is this literal IP somewhere we must never send a request? */
function isBlockedIp(ip) {
  const v = net.isIP(ip);
  if (v === 4) {
    const [a, b] = ip.split(".").map(Number);
    if (a === 0 || a === 10 || a === 127) return true;
    if (a === 169 && b === 254) return true; // link-local / cloud metadata
    if (a === 172 && b >= 16 && b <= 31) return true;
    if (a === 192 && b === 168) return true;
    if (a === 100 && b >= 64 && b <= 127) return true; // CGNAT
    if (a >= 224) return true; // multicast + reserved
    return false;
  }
  if (v === 6) {
    const s = ip.toLowerCase();
    if (s === "::" || s === "::1") return true;
    // IPv4-mapped (::ffff:10.0.0.1) — re-check as v4.
    const mapped = s.match(/^::ffff:(\d+\.\d+\.\d+\.\d+)$/);
    if (mapped) return isBlockedIp(mapped[1]);
    const head = parseInt(s.split(":")[0] || "0", 16);
    if ((head & 0xfe00) === 0xfc00) return true; // fc00::/7 unique-local
    if ((head & 0xffc0) === 0xfe80) return true; // fe80::/10 link-local
    return false;
  }
  return true; // not an IP at all — refuse
}

/**
 * Parse, scheme-check, and resolve `raw`, refusing anything that lands on a
 * private or loopback address. Returns the parsed URL.
 */
async function assertPublicUrl(raw) {
  let u;
  try {
    u = new URL(raw);
  } catch {
    throw new HttpError(400, `not a URL: ${raw}`);
  }
  if (u.protocol !== "http:" && u.protocol !== "https:") {
    throw new HttpError(400, `refusing scheme ${u.protocol}`);
  }
  const host = u.hostname.replace(/^\[|\]$/g, "");
  let addrs;
  if (net.isIP(host)) {
    addrs = [{ address: host }];
  } else {
    try {
      addrs = await dns.lookup(host, { all: true });
    } catch (e) {
      throw new HttpError(400, `cannot resolve ${host}: ${e.code || e.message}`);
    }
  }
  for (const { address } of addrs) {
    if (isBlockedIp(address)) {
      throw new HttpError(403, `refusing private address ${address} for ${host}`);
    }
  }
  return u;
}

class HttpError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

// ---------------------------------------------------------------------------
// Browser
// ---------------------------------------------------------------------------

let browserPromise = null;

async function getBrowser() {
  if (!browserPromise) {
    log("launching chromium");
    browserPromise = chromium
      .launch({
        args: ["--no-sandbox", "--disable-dev-shm-usage", "--disable-gpu"],
      })
      .then((b) => {
        // A crashed browser must not poison every later request.
        b.on("disconnected", () => {
          log("chromium disconnected");
          browserPromise = null;
        });
        return b;
      })
      .catch((e) => {
        browserPromise = null;
        throw e;
      });
  }
  return browserPromise;
}

/**
 * How we introduce ourselves. The version has to come from the browser we are
 * actually driving: Chromium sends `Sec-CH-UA` headers that we do not control,
 * so a hardcoded version silently rots the next time the base image moves, and
 * a UA that disagrees with its own client hints reads as a spoof. WordPress.com
 * answers that with a 403 "Checking your browser..." page.
 */
async function identity() {
  const major = (await getBrowser()).version().split(".")[0];
  return {
    userAgent:
      process.env.FETCHER_USER_AGENT ||
      "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) " +
        `Chrome/${major}.0.0.0 Safari/537.36 feedbot/1.0 (+https://feedbot.rjp.io)`,
    headers: {
      "sec-ch-ua": `"Chromium";v="${major}", "Google Chrome";v="${major}", "Not)A;Brand";v="24"`,
      "sec-ch-ua-mobile": "?0",
      "sec-ch-ua-platform": '"Linux"',
    },
  };
}

/** Run `fn(page)` in a throwaway context. Images/media/fonts never load. */
async function withPage(fn) {
  const browser = await getBrowser();
  const { userAgent, headers } = await identity();
  const context = await browser.newContext({
    userAgent,
    extraHTTPHeaders: headers,
    viewport: { width: 1280, height: 2000 },
    locale: "en-US",
    javaScriptEnabled: true,
  });
  context.setDefaultNavigationTimeout(NAV_TIMEOUT_MS);
  context.setDefaultTimeout(NAV_TIMEOUT_MS);
  // We only ever read the DOM, so bytes spent on pixels are bytes wasted.
  await context.route("**/*", (route) => {
    const t = route.request().resourceType();
    if (t === "image" || t === "media" || t === "font") return route.abort();
    return route.continue();
  });
  try {
    const page = await context.newPage();
    return await fn(page);
  } finally {
    await context.close().catch(() => {});
  }
}

/** Navigate and give client-rendered pages a moment to settle. */
async function goto(page, url) {
  const resp = await page.goto(url, { waitUntil: "domcontentloaded" });
  await page
    .waitForLoadState("networkidle", { timeout: SETTLE_TIMEOUT_MS })
    .catch(() => {});
  return resp ? resp.status() : 0;
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async function discover({ url }) {
  const target = await assertPublicUrl(url);
  return withPage(async (page) => {
    const status = await goto(page, target.href);
    const data = await page.evaluate(() => {
      const abs = (href) => {
        try {
          return new URL(href, document.baseURI).href;
        } catch {
          return null;
        }
      };
      const feeds = [...document.querySelectorAll('link[rel~="alternate"][href]')]
        .filter((l) => /rss|atom|xml|json/i.test(l.getAttribute("type") || ""))
        .map((l) => abs(l.getAttribute("href")))
        .filter(Boolean)
        // A comments feed is a feed, but never the one we want.
        .filter((href) => !/\/comments\/|comment/i.test(href));
      const links = [];
      for (const a of document.querySelectorAll("a[href]")) {
        const href = abs(a.getAttribute("href"));
        if (!href) continue;
        links.push({ href, text: (a.textContent || "").trim().slice(0, 300) });
      }
      return { title: document.title || "", feeds, links };
    });
    return { finalUrl: page.url(), status, ...data };
  });
}

async function article({ url }) {
  const target = await assertPublicUrl(url);
  const { html, finalUrl, status, pageTitle } = await withPage(async (page) => {
    const status = await goto(page, target.href);
    return {
      html: await page.content(),
      finalUrl: page.url(),
      status,
      pageTitle: await page.title(),
    };
  });

  // A "page not found" page is still a page: Blogger's is 11k characters of
  // sidebar, plenty for Readability to hand back as an article. Believe the
  // status code instead.
  if (status >= 400) {
    throw new HttpError(422, `${finalUrl} returned HTTP ${status}`);
  }

  // Readability mutates the document it is handed, so give it a private one.
  // `url` makes it resolve relative <img src> and <a href> against the page.
  const dom = new JSDOM(html, { url: finalUrl });
  const doc = dom.window.document;

  const ogTitle = attrOf(doc, 'meta[property="og:title"]', "content");
  const ogSiteName = attrOf(doc, 'meta[property="og:site_name"]', "content");
  const headingTitle = extractHeading(doc);
  const publishedTime = extractPublished(doc);

  let parsed = null;
  try {
    parsed = new Readability(doc, { charThreshold: 250 }).parse();
  } catch (e) {
    log(`readability failed for ${finalUrl}: ${e.message}`);
  }
  dom.window.close();

  if (!parsed || !parsed.content) {
    throw new HttpError(422, `no article content extracted from ${finalUrl}`);
  }
  const content = unwrapReadability(parsed.content);
  const text = (parsed.textContent || "").replace(/\s+/g, " ").trim();
  const siteName = parsed.siteName?.trim() || null;
  const bareTitle = stripSiteChrome(pageTitle, finalUrl, siteName, ogSiteName);
  return {
    finalUrl,
    status,
    // og:title is what the author told social networks to show. Then the post
    // heading, but only if the <title> vouches for it. Then the <title> with the
    // blog's name stripped off — which beats a heading we could not corroborate,
    // and is empty when the <title> is just the blog's name. The caller may
    // still override all of this with a feed title.
    title: (
      ogTitle ||
      corroboratedHeading(headingTitle, pageTitle) ||
      bareTitle ||
      headingTitle ||
      pageTitle ||
      ""
    ).trim(),
    byline: parsed.byline?.trim() || null,
    siteName,
    // NOT parsed.excerpt: that prefers the page's meta description, which on
    // Blogger is the *blog's* tagline — identical on every post it publishes.
    excerpt: excerptOf(content, text),
    publishedTime: parsed.publishedTime || publishedTime,
    html: content,
    text,
    wordCount: text ? text.split(" ").length : 0,
  };
}

/**
 * Readability returns its content inside `<div id="readability-page-1">`, and
 * the site's own post-body div usually sits inside that. Neither says anything.
 * Peeling them off lets the reader style `.prose > p:first-of-type`.
 */
function unwrapReadability(html) {
  const doc = new JSDOM(`<body>${html}</body>`).window.document;
  let node = doc.body;
  for (let i = 0; i < 4; i++) {
    const only = node.children.length === 1 ? node.firstElementChild : null;
    if (!only || only.tagName !== "DIV") break;
    // Bail if unwrapping would drop text that lives beside the div.
    if (node.textContent.trim() !== only.textContent.trim()) break;
    node = only;
  }
  return node.innerHTML.trim();
}

const EXCERPT_MAX = 280;

function clip(s) {
  if (s.length <= EXCERPT_MAX) return s;
  const cut = s.slice(0, EXCERPT_MAX);
  const sp = cut.lastIndexOf(" ");
  return `${(sp > EXCERPT_MAX * 0.6 ? cut.slice(0, sp) : cut).trimEnd()}…`;
}

/** The article's own opening words — a preview, not the blog's boilerplate. */
function excerptOf(html, fallbackText) {
  const doc = new JSDOM(`<body>${html}</body>`).window.document;
  for (const p of doc.querySelectorAll("p")) {
    const t = (p.textContent || "").replace(/\s+/g, " ").trim();
    if (t.length >= 80) return clip(t);
  }
  return fallbackText ? clip(fallbackText) : null;
}

const attrOf = (doc, sel, attr) =>
  doc.querySelector(sel)?.getAttribute(attr)?.trim() || null;

/** Pull a datePublished out of JSON-LD, however deeply it's nested. */
function publishedFromJsonLd(doc) {
  for (const node of doc.querySelectorAll('script[type="application/ld+json"]')) {
    let data;
    try {
      data = JSON.parse(node.textContent || "");
    } catch {
      continue;
    }
    const stack = [data];
    while (stack.length) {
      const cur = stack.pop();
      if (Array.isArray(cur)) {
        stack.push(...cur);
      } else if (cur && typeof cur === "object") {
        const hit = cur.datePublished || cur.dateCreated;
        if (typeof hit === "string" && hit.trim()) return hit.trim();
        stack.push(...Object.values(cur));
      }
    }
  }
  return null;
}

const HEADING_SELECTORS = [
  "h1.post-title",
  "h1.entry-title",
  ".post-title",
  ".entry-title",
  "article h1",
  "h1",
];

/** The visible post heading, for blogs that publish no metadata at all. */
function extractHeading(doc) {
  for (const sel of HEADING_SELECTORS) {
    const t = doc.querySelector(sel)?.textContent?.replace(/\s+/g, " ").trim();
    if (t && t.length > 2 && t.length < 300) return t;
  }
  return null;
}

/** Compare titles the way a reader would: ignoring case, space and dashes. */
const normalizeTitle = (s) =>
  (s || "")
    .toLowerCase()
    .replace(/\s+/g, " ")
    .replace(/[“”"'’‘–—-]/g, "")
    .trim();

/**
 * A heading we can believe. `article h1` matches the <h1> of *any* <article> on
 * the page, and analog-antiquarian.net renders a listing card whose heading is
 * the series name — so all thirteen chapters of one series came back with the
 * same title. The <title> is the one per-post string themes reliably get right,
 * so make it a witness: trust the heading only when the title agrees with it.
 */
function corroboratedHeading(heading, pageTitle) {
  const [h, t] = [normalizeTitle(heading), normalizeTitle(pageTitle)];
  return h && t && t.includes(h) ? heading : null;
}

/** "The Analog Antiquarian" and "analog-antiquarian" both reduce to one word. */
const siteKey = (s) =>
  (s || "")
    .toLowerCase()
    .replace(/^\s*the\s+/, "")
    .replace(/[^a-z0-9]/g, "");

/** " – ", " | ", " » ", " - " separate a title from its chrome. A colon does not. */
const TITLE_SEPARATOR = /\s+[|»«–—·]+\s+|\s+-\s+/;

/**
 * The page title with the blog's own name taken off either end. Readability
 * can't do this: given "Chapter 9: A Late Bloomer – The Analog Antiquarian" it
 * splits on the colon and hands back "A Late Bloomer – The Analog Antiquarian".
 *
 * Returns "" when the title is nothing *but* the site's name, as Blogger's is
 * ("The CRPG Addict") — that is a title telling us nothing, and the caller
 * should prefer even an uncorroborated heading to it.
 */
function stripSiteChrome(pageTitle, finalUrl, parsedSiteName, ogSiteName) {
  // The domain's own label names the site on blogs that never say so in markup.
  let label = "";
  try {
    label = new URL(finalUrl).hostname.replace(/^www\./, "").split(".")[0];
  } catch {
    /* a title is not worth throwing over */
  }
  const aliases = new Set([parsedSiteName, ogSiteName, label].map(siteKey).filter(Boolean));
  const parts = (pageTitle || "")
    .split(TITLE_SEPARATOR)
    .map((p) => p.trim())
    .filter(Boolean);
  while (parts.length && aliases.has(siteKey(parts[parts.length - 1]))) parts.pop();
  while (parts.length && aliases.has(siteKey(parts[0]))) parts.shift();
  return parts.join(" – ");
}

const DATE_TEXT_SELECTORS = [
  "h2.date-header", // Blogger: "Saturday, July 5, 2026"
  ".date-header",
  ".entry-date",
  ".post-date",
  ".published",
  ".timestamp",
  "time",
  ".meta", // filfre: "Posted by Jimmy Maher on July 3, 2026 in ..."
  ".entry-meta",
  ".post-meta",
  ".byline",
];

const MONTH =
  "jan(?:uary)?|feb(?:ruary)?|mar(?:ch)?|apr(?:il)?|may|jun(?:e)?|jul(?:y)?|" +
  "aug(?:ust)?|sep(?:t(?:ember)?)?|oct(?:ober)?|nov(?:ember)?|dec(?:ember)?";

// A date must carry its year. "03 Jul" and "3 min read" are not dates, and
// handing either to Date.parse invents a year. So look for a whole date
// *inside* the element's text rather than parsing the text wholesale.
const DATE_PATTERNS = [
  new RegExp(`\\b(?:${MONTH})\\.?\\s+\\d{1,2},?\\s+(?:19|20)\\d{2}\\b`, "i"),
  new RegExp(`\\b\\d{1,2}\\s+(?:${MONTH})\\.?,?\\s+(?:19|20)\\d{2}\\b`, "i"),
  /\b(?:19|20)\d{2}-\d{1,2}-\d{1,2}\b/,
  /\b\d{1,2}\/\d{1,2}\/(?:19|20)\d{2}\b/,
];

function findDateIn(text) {
  const thisYear = new Date().getUTCFullYear();
  for (const re of DATE_PATTERNS) {
    const m = text.match(re);
    if (!m) continue;
    const ms = Date.parse(m[0]);
    if (!Number.isFinite(ms)) continue;
    const d = new Date(ms);
    const y = d.getUTCFullYear();
    if (y >= 1990 && y <= thisYear + 1) return d.toISOString();
  }
  return null;
}

/** Last resort: read the date a human would read off the rendered page. */
function publishedFromText(doc) {
  for (const sel of DATE_TEXT_SELECTORS) {
    for (const el of doc.querySelectorAll(sel)) {
      const t = (el.textContent || "").replace(/\s+/g, " ").trim();
      if (!t || t.length > 400) continue;
      const hit = findDateIn(t);
      if (hit) return hit;
    }
  }
  return null;
}

/**
 * Publication dates hide in a different place on every blog engine. Try the
 * standards first, then the WordPress and Blogger conventions, then give up
 * and read the date the humans see.
 */
function extractPublished(doc) {
  return (
    attrOf(doc, 'meta[property="article:published_time"]', "content") ||
    attrOf(doc, 'meta[itemprop="datePublished"]', "content") ||
    attrOf(doc, 'meta[name="date"]', "content") ||
    attrOf(doc, 'meta[name="parsely-pub-date"]', "content") ||
    publishedFromJsonLd(doc) ||
    attrOf(doc, "[itemprop=datePublished][datetime]", "datetime") ||
    attrOf(doc, "[itemprop=datePublished][content]", "content") ||
    attrOf(doc, "abbr.published[title]", "title") || // Blogger
    attrOf(doc, ".published[title]", "title") ||
    attrOf(doc, "time.entry-date[datetime]", "datetime") || // WordPress
    attrOf(doc, "time[pubdate][datetime]", "datetime") ||
    attrOf(doc, "time[datetime]", "datetime") ||
    publishedFromText(doc) ||
    null
  );
}

// --- feeds ---

const xml = new XMLParser({
  ignoreAttributes: false,
  attributeNamePrefix: "@_",
  trimValues: true,
  // Without this, numeric character references survive verbatim and a title
  // reads "Planescape: Torment, Part 2: &#8230;to the Desktop".
  htmlEntities: true,
});

const arr = (v) => (v == null ? [] : Array.isArray(v) ? v : [v]);
const textOf = (v) => {
  if (v == null) return null;
  if (typeof v === "string") return v.trim() || null;
  if (typeof v === "number") return String(v);
  if (typeof v === "object") return textOf(v["#text"]);
  return null;
};

/** Atom <link> is an element with attributes; RSS <link> is a text node. */
function atomLink(entry) {
  const links = arr(entry.link);
  const alt = links.find(
    (l) =>
      typeof l === "object" &&
      (l["@_rel"] === "alternate" || l["@_rel"] == null) &&
      (l["@_type"] == null || l["@_type"] === "text/html"),
  );
  const chosen = alt || links.find((l) => typeof l === "object");
  if (chosen) return chosen["@_href"] || null;
  return textOf(links[0]);
}

async function fetchFeedBody(target) {
  const { userAgent } = await identity();
  // Plain fetch first — feeds are static XML and this avoids a browser tab.
  try {
    const res = await fetch(target.href, {
      headers: {
        "user-agent": userAgent,
        accept: "application/rss+xml, application/atom+xml, application/xml, text/xml, */*",
      },
      redirect: "follow",
      signal: AbortSignal.timeout(NAV_TIMEOUT_MS),
    });
    if (res.ok) return await res.text();
    log(`feed fetch ${target.href} -> ${res.status}, retrying in browser`);
  } catch (e) {
    log(`feed fetch ${target.href} failed (${e.message}), retrying in browser`);
  }
  // Some hosts only answer a real browser. Chromium wraps XML in a viewer, so
  // read the raw text rather than the rendered DOM.
  return withPage(async (page) => {
    const status = await goto(page, target.href);
    if (status >= 400) throw new HttpError(502, `feed ${target.href} -> ${status}`);
    return page.evaluate(() => document.documentElement.textContent || "");
  });
}

async function feed({ url }) {
  const target = await assertPublicUrl(url);
  const body = await fetchFeedBody(target);

  let parsed;
  try {
    parsed = xml.parse(body);
  } catch (e) {
    throw new HttpError(422, `feed ${target.href} is not valid XML: ${e.message}`);
  }

  let entries = [];
  if (parsed.rss?.channel) {
    entries = arr(parsed.rss.channel.item).map((it) => ({
      url: textOf(it.link) || it.guid?.["#text"] || textOf(it.guid),
      title: textOf(it.title),
      published: textOf(it.pubDate) || textOf(it["dc:date"]),
    }));
  } else if (parsed.feed) {
    entries = arr(parsed.feed.entry).map((e) => ({
      url: atomLink(e),
      title: textOf(e.title),
      published: textOf(e.published) || textOf(e.updated),
    }));
  } else if (parsed["rdf:RDF"]) {
    entries = arr(parsed["rdf:RDF"].item).map((it) => ({
      url: textOf(it.link),
      title: textOf(it.title),
      published: textOf(it["dc:date"]),
    }));
  } else {
    throw new HttpError(422, `feed ${target.href}: unrecognized feed format`);
  }

  entries = entries.filter((e) => e.url && /^https?:\/\//i.test(e.url));
  return { finalUrl: target.href, entries };
}

// ---------------------------------------------------------------------------
// HTTP plumbing
// ---------------------------------------------------------------------------

const ROUTES = { "/discover": discover, "/feed": feed, "/article": article };

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    req.on("data", (c) => {
      size += c.length;
      if (size > MAX_BODY_BYTES) {
        reject(new HttpError(413, "body too large"));
        req.destroy();
        return;
      }
      chunks.push(c);
    });
    req.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
    req.on("error", reject);
  });
}

function send(res, status, obj) {
  const body = JSON.stringify(obj);
  res.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "content-length": Buffer.byteLength(body),
  });
  res.end(body);
}

const server = http.createServer(async (req, res) => {
  const started = Date.now();
  const path = (req.url || "").split("?")[0];

  if (req.method === "GET" && path === "/healthz") return send(res, 200, { ok: true });

  const handler = ROUTES[path];
  if (!handler) return send(res, 404, { ok: false, error: `no route ${path}` });
  if (req.method !== "POST") return send(res, 405, { ok: false, error: "POST only" });

  try {
    const raw = await readBody(req);
    let payload;
    try {
      payload = JSON.parse(raw || "{}");
    } catch (e) {
      throw new HttpError(400, `bad JSON: ${e.message}`);
    }
    if (!payload.url) throw new HttpError(400, "missing url");
    const out = await handler(payload);
    log(`${path} ${payload.url} ok in ${Date.now() - started}ms`);
    send(res, 200, { ok: true, ...out });
  } catch (e) {
    const status = e instanceof HttpError ? e.status : 502;
    log(`${path} failed in ${Date.now() - started}ms: ${e.message}`);
    send(res, status, { ok: false, error: e.message });
  }
});

// Article renders can legitimately take a while; don't let node time them out.
server.requestTimeout = 0;
server.headersTimeout = 65_000;

for (const sig of ["SIGINT", "SIGTERM"]) {
  process.on(sig, async () => {
    log(`${sig}, shutting down`);
    server.close();
    if (browserPromise) await (await browserPromise).close().catch(() => {});
    process.exit(0);
  });
}

server.listen(PORT, HOST, () => log(`fetcher listening on http://${HOST}:${PORT}`));
