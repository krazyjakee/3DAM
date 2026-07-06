import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { fileURLToPath, URL } from "node:url";

// The dev server proxies the engine surface to a running `3dam serve` (default :7333),
// so `pnpm dev` gives HMR against a real backend on one origin (tech-spec 09 §A.4 dev loop).
// Override the target with VITE_API_TARGET when serve runs on another port.
const apiTarget = process.env.VITE_API_TARGET ?? "http://127.0.0.1:7333";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) },
  },
  // Built assets are content-hashed and embedded into the `3dam` binary via rust-embed
  // (tech-spec 09 §A.4). The SPA is root-mounted by `3dam serve`, so absolute `/assets/*` URLs
  // resolve correctly even for a directly-loaded deep client route (SPA fallback).
  base: "/",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: false,
  },
  server: {
    port: 5173,
    proxy: {
      "/api": {
        target: apiTarget,
        changeOrigin: true,
        ws: true,
      },
    },
  },
});
