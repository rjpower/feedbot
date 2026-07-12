// Guard the one browser feedbot actually exists for.
//
// The reading device is a Kindle, and its browser is Blink 74–79 hiding behind
// a spoofed 2009 WebKit user agent. It has ES modules, Proxy and CSS custom
// properties — everything Vue 3 needs — but no optional chaining, which shipped
// in Chrome 80. Vite's default target assumes something far newer, so a single
// `?.` in the bundle is a SyntaxError there: the module never runs, nothing
// mounts, and the page is blank with no error in any log we keep. It took a
// capability probe on the device itself to find that the first time.
//
// So: parse every emitted chunk at the oldest syntax level we promise, and fail
// the build the moment something newer slips in. Run by `npm run build`.

import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { parse } from "acorn";

/** ES2019 is the last edition before optional chaining and `??`. */
const ECMA_VERSION = 2019;

const dist = fileURLToPath(new URL("../dist/assets/", import.meta.url));
const files = readdirSync(dist);
let failed = false;

for (const name of files.filter((f) => f.endsWith(".js"))) {
  const source = readFileSync(join(dist, name), "utf8");
  try {
    parse(source, { ecmaVersion: ECMA_VERSION, sourceType: "module" });
    console.log(`  ok    ${name} parses as ES${ECMA_VERSION}`);
  } catch (e) {
    failed = true;
    // acorn reports a character offset; a line of context beats a number.
    const at = Math.max(0, (e.pos ?? 0) - 60);
    console.error(`  FAIL  ${name}: ${e.message}`);
    console.error(`        ...${source.slice(at, at + 120).replace(/\n/g, " ")}...`);
    console.error(`        Raise of build.target in vite.config.js? See the comment there.`);
  }
}

// These degrade rather than explode, but only because each has a fallback
// declaration ahead of it. Losing the fallback is silent, so count them.
// `max(min(...))` is what esbuild lowers clamp() into — and min()/max() are
// Chrome 79, the same release as clamp(), so the lowering buys nothing on its
// own. The plain rem declaration ahead of it is what actually does the work.
const CSS_NEEDING_FALLBACK = ["100dvh", "color-mix(", ":has(", "clamp(", "max(min("];
for (const name of files.filter((f) => f.endsWith(".css"))) {
  const css = readFileSync(join(dist, name), "utf8");
  const found = CSS_NEEDING_FALLBACK.filter((f) => css.includes(f));
  const fallbacks = { "100dvh": "100vh", "color-mix(": "background:var(--bg)" };
  for (const feature of found) {
    const need = fallbacks[feature];
    if (need && !css.includes(need)) {
      failed = true;
      console.error(`  FAIL  ${name}: uses ${feature} with no ${need} fallback`);
    }
  }
  console.log(`  ok    ${name} — post-Chrome-73 CSS present but guarded: ${found.join(" ") || "none"}`);
}

if (failed) {
  console.error("\nThe Kindle would render a blank page. Build refused.");
  process.exit(1);
}
console.log(`\nBundle is safe for Chrome 73+.`);
