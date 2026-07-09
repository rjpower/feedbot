import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

// The Rust server serves `dist/` directly, so build output is flat and
// self-contained: fonts are bundled from node_modules, nothing is fetched from
// a CDN at runtime.
export default defineConfig({
  plugins: [vue()],
  build: {
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
