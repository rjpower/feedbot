import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

// The Rust server serves `dist/` directly, so build output is flat and
// self-contained: fonts are bundled from node_modules, nothing is fetched from
// a CDN at runtime.
export default defineConfig({
  plugins: [vue()],
  build: {
    // The device this whole app exists for is a Kindle, whose browser is Blink
    // 74–79 wearing a spoofed 2009 WebKit user agent. It has ES modules, Proxy
    // and CSS variables — everything Vue 3 needs — but not optional chaining,
    // which shipped in Chrome 80. Vite's default target assumes ~Chrome 107, so
    // a single `?.` anywhere in the bundle is a SyntaxError there: the module
    // never executes, nothing mounts, and the page is blank with no error to
    // find. `npm run check` fails the build if this is ever raised.
    //
    // cssTarget follows target, which is what keeps esbuild from minifying
    // colours into `rgb(0 0 0/.5)` syntax that Blink 74 cannot parse either.
    target: "chrome73",
    outDir: "dist",
    emptyOutDir: true,
    assetsInlineLimit: 0,
  },
  server: {
    port: 5173,
    proxy: {
      "/api": { target: "http://127.0.0.1:8099", changeOrigin: true },
    },
  },
});
