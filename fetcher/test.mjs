// Live smoke test for the fetch sidecar. Hits the real seed blogs, so it needs
// network and a running `node server.mjs`. Run with: npm test
//
// These assertions encode what the crawler depends on: that we can find a
// site's feed, read entries out of it, and pull a titled, dated article body
// off a page. The three seeds cover Blogger and WordPress.

const BASE = process.env.FETCHER_URL || "http://127.0.0.1:4000";

const siteKey = (s) =>
  (s || "")
    .toLowerCase()
    .replace(/^\s*the\s+/, "")
    .replace(/[^a-z0-9]/g, "");

let failures = 0;
const check = (name, cond, detail = "") => {
  console.log(`${cond ? "  ok  " : "  FAIL"} ${name}${detail ? ` — ${detail}` : ""}`);
  if (!cond) failures++;
};

async function post(path, body) {
  const res = await fetch(`${BASE}${path}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  return res.json();
}

const SEEDS = [
  {
    name: "crpgaddict (Blogger)",
    home: "https://crpgaddict.blogspot.com/",
    feed: "https://crpgaddict.blogspot.com/feeds/posts/default",
    article: "https://crpgaddict.blogspot.com/2026/07/al-qadim-genies-betrayal.html",
  },
  {
    name: "datadrivengamer (Blogger)",
    home: "https://datadrivengamer.blogspot.com/",
    feed: "https://datadrivengamer.blogspot.com/feeds/posts/default",
    article: "https://datadrivengamer.blogspot.com/2026/07/take-a-train-won.html",
  },
  {
    name: "filfre (WordPress)",
    home: "https://www.filfre.net/",
    feed: "https://www.filfre.net/feed/",
    article: "https://www.filfre.net/2026/07/the-life-and-times-of-maxis-part-1-simeverything/",
  },
];

async function main() {
  const health = await fetch(`${BASE}/healthz`).then((r) => r.json());
  check("sidecar is up", health.ok === true);

  for (const seed of SEEDS) {
    console.log(`\n=== ${seed.name}`);

    const d = await post("/discover", { url: seed.home });
    check("discover ok", d.ok === true, d.error);
    if (d.ok) {
      check("finds a feed", d.feeds.length > 0, `feeds=${JSON.stringify(d.feeds)}`);
      check(
        "feed is not a comments feed",
        d.feeds.every((f) => !/comment/i.test(f)),
      );
      check("index page has links", d.links.length > 20, `${d.links.length} links`);
    }

    const f = await post("/feed", { url: seed.feed });
    check("feed parses", f.ok === true, f.error);
    if (f.ok) {
      check("feed has entries", f.entries.length > 0, `${f.entries.length} entries`);
      check(
        "entries have absolute urls",
        f.entries.every((e) => /^https?:\/\//.test(e.url)),
      );
      check(
        "entries have titles",
        f.entries.every((e) => e.title),
      );
      check(
        "entries have dates",
        f.entries.every((e) => e.published && Number.isFinite(Date.parse(e.published))),
      );
      // WordPress writes titles like "Part 2: &#8230;to the Desktop".
      check(
        "titles have no raw entities",
        f.entries.every((e) => !/&(#\d+|#x[0-9a-f]+|[a-z]+);/i.test(e.title)),
        f.entries.map((e) => e.title).find((t) => /&\w+;|&#/.test(t)),
      );
    }

    const a = await post("/article", { url: seed.article });
    check("article extracts", a.ok === true, a.error);
    if (a.ok) {
      check("has a title", !!a.title, JSON.stringify(a.title));
      check("title has no site chrome", !/»|Digital Antiquarian$/.test(a.title), a.title);
      check("has a published date", !!a.publishedTime, String(a.publishedTime));
      check("date parses", Number.isFinite(Date.parse(a.publishedTime || "")));
      check("has body text", a.wordCount > 200, `${a.wordCount} words`);
      check("images are absolute", !/src="(?!https?:)/.test(a.html));
      check("readability wrapper is unwrapped", !/id="readability-page/.test(a.html));
      check("content starts with real markup", /^<(p|div|figure|table|h\d|img|blockquote)/i.test(a.html), a.html.slice(0, 40));
      check("has an excerpt", !!a.excerpt, String(a.excerpt).slice(0, 60));
      check("excerpt is not the blog tagline", !/A blog in which a dedicated addict/.test(a.excerpt || ""), a.excerpt?.slice(0, 50));
      check("excerpt is prose from the article", (a.excerpt || "").length > 60);
      seed.excerpt = a.excerpt;
    }
  }

  // The bug this guards: Readability's own `excerpt` falls back to the page's
  // meta description, which Blogger sets to the blog's tagline — so every post
  // on a blog got the same preview text.
  console.log("\n=== excerpts differ between posts on one blog");
  {
    const two = await Promise.all([
      post("/article", { url: "https://crpgaddict.blogspot.com/2026/07/al-qadim-genies-betrayal.html" }),
      post("/article", { url: "https://crpgaddict.blogspot.com/2026/07/the-search-for-freedom-all-parts-of.html" }),
    ]);
    if (two.every((x) => x.ok)) {
      check("two posts, two excerpts", two[0].excerpt !== two[1].excerpt);
      check("two posts, two titles", two[0].title !== two[1].title);
    } else {
      check("fetched both posts", false, two.map((x) => x.error).join("; "));
    }
  }

  // analog-antiquarian.net renders a listing card whose <h1> is the *series*
  // name, so `article h1` gives every chapter the same title. Only the <title>
  // distinguishes them.
  console.log("\n=== post heading vs. listing-card heading");
  {
    const [a, b] = await Promise.all([
      post("/article", { url: "https://analog-antiquarian.net/2025/12/12/chapter-9-a-late-bloomer/" }),
      post("/article", { url: "https://analog-antiquarian.net/2026/03/13/chapter-15-the-trial-of-galileo/" }),
    ]);
    check("both chapters fetched", a.ok !== false && b.ok !== false, `${a.error || ""} ${b.error || ""}`);
    check("  chapter 9 titled by chapter", /A Late Bloomer/i.test(a.title || ""), a.title);
    check("  chapter 15 titled by chapter", /Trial of Galileo/i.test(b.title || ""), b.title);
    check("  not the series title", a.title !== b.title);
    check("  no site chrome", !/Analog Antiquarian/i.test(a.title || ""), a.title);
    check("  keeps the chapter number", /^Chapter 9\b/.test(a.title || ""), a.title);
  }

  // The same precedence on Blogger, whose <title> carries the blog name as a
  // "Blog: Post" prefix — a colon, which is not a chrome separator.
  console.log("\n=== blogger keeps the post title, not the blog name");
  {
    const r = await post("/article", {
      url: "https://crpgaddict.blogspot.com/2026/07/our-second-greatest-gift.html",
    });
    check("fetched", r.ok !== false, r.error);
    check("  titled by post", /Second-Greatest Gift/i.test(r.title || ""), r.title);
    check("  not the blog name", siteKey(r.title) !== "crpgaddict", r.title);
    check("  no blog-name prefix", !/^The CRPG Addict:/i.test(r.title || ""), r.title);
  }

  // Blogger answers an unknown slug with 404 and eleven thousand characters of
  // sidebar, which Readability will happily call an article.
  console.log("\n=== a 404 is not an article");
  {
    const r = await post("/article", {
      url: "https://crpgaddict.blogspot.com/2026/07/game-472-take-a-train.html",
    });
    check("refuses a 404 page", r.ok === false, r.error || `got title "${r.title}"`);
    check("  says so", /404/.test(r.error || ""), r.error);
  }

  // WordPress.com serves a 403 "Checking your browser..." interstitial to a
  // user agent whose Chrome version disagrees with the Sec-CH-UA headers
  // Chromium sends alongside it. Nothing else on the seed list notices.
  console.log("\n=== bot check (user agent matches its own client hints)");
  {
    const d = await post("/discover", { url: "https://bluerenga.blog/" });
    check("bluerenga discovers", d.ok !== false, d.error);
    check("  not challenged", d.status === 200, `status ${d.status}: ${d.title}`);
    check("  found a feed", (d.feeds || []).length > 0);
    check("  found links", (d.links || []).length > 20, `${(d.links || []).length} links`);
  }

  console.log("\n=== SSRF guard");
  for (const bad of [
    "http://127.0.0.1:4000/healthz",
    "http://169.254.169.254/latest/meta-data/",
    "http://192.168.1.1/",
    "http://[::1]/",
    "file:///etc/passwd",
  ]) {
    const r = await post("/article", { url: bad });
    check(`refuses ${bad}`, r.ok === false, r.error);
  }

  console.log(failures ? `\n${failures} FAILURES` : "\nall green");
  process.exit(failures ? 1 : 0);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
