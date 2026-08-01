import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { constants as zlibConstants, brotliCompress, gzip } from "node:zlib";
import { readdir, readFile, writeFile } from "node:fs/promises";
import { extname, join } from "node:path";
import { fileURLToPath, URL } from "node:url";

// The dev server proxies the engine surface to a running `3dam serve` (default :7333),
// so `pnpm dev` gives HMR against a real backend on one origin (tech-spec 09 §A.4 dev loop).
// Override the target with VITE_API_TARGET when serve runs on another port.
const apiTarget = process.env.VITE_API_TARGET ?? "http://127.0.0.1:7333";

// Emit sidecars beside the content-hashed production assets. The Rust host embeds these files and
// negotiates them directly, avoiding a fresh compression job on every request for the large JS and
// WASM payloads. Node's built-in codecs keep this part of the release build dependency-free.
function precompressAssets() {
  const compressible = new Set([".css", ".html", ".js", ".json", ".svg", ".wasm"]);

  const encode = (input: Buffer) =>
    Promise.all([
      new Promise<Buffer>((resolve, reject) =>
        brotliCompress(
          input,
          {
            params: {
              [zlibConstants.BROTLI_PARAM_QUALITY]: zlibConstants.BROTLI_MAX_QUALITY,
            },
          },
          (error, output) => (error ? reject(error) : resolve(output)),
        ),
      ),
      new Promise<Buffer>((resolve, reject) =>
        gzip(input, { level: zlibConstants.Z_BEST_COMPRESSION }, (error, output) =>
          error ? reject(error) : resolve(output),
        ),
      ),
    ]);

  const visit = async (directory: string): Promise<void> => {
    await Promise.all(
      (await readdir(directory, { withFileTypes: true })).map(async (entry) => {
        const path = join(directory, entry.name);
        if (entry.isDirectory()) return visit(path);
        if (!compressible.has(extname(entry.name))) return;

        const input = await readFile(path);
        // Below this point sidecar overhead and an extra cache variant cost more than they save;
        // the server applies the same tiny-body policy to dynamic responses.
        if (input.byteLength < 256) return;
        const [brotli, gzipped] = await encode(input);
        await Promise.all([writeFile(`${path}.br`, brotli), writeFile(`${path}.gz`, gzipped)]);
      }),
    );
  };

  return {
    name: "3dam-precompress-assets",
    apply: "build" as const,
    closeBundle: () => visit(fileURLToPath(new URL("./dist", import.meta.url))),
  };
}

export default defineConfig({
  plugins: [react(), tailwindcss(), precompressAssets()],
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
      // `/admin/api` is a *sibling* of `/api`, not a child, so it needs its own entry — without
      // it every Settings call 404s against the dev server itself and the whole admin surface
      // renders as "unsupported on this server", which looks like a server problem rather than a
      // missing proxy rule. Found while verifying the OIDC settings section (issue #41).
      // Scoped to `/admin/api`, not `/admin`, so it cannot shadow a future client-side route.
      // No `ws: true`: the only WebSocket is `/api/v1/ws`, covered by the entry above.
      "/admin/api": {
        target: apiTarget,
        changeOrigin: true,
      },
    },
  },
});
