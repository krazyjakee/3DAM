import { spawn } from "node:child_process";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const baseUrl = process.env.RESPONSIVE_BASE_URL ?? "http://127.0.0.1:5173";
const chromium = process.env.CHROMIUM ?? "chromium";
const useFixtureApi = process.env.RESPONSIVE_LIVE_API !== "1";
const scriptDir = dirname(fileURLToPath(import.meta.url));
const outputDir = join(scriptDir, "..", "..", "docs", "images", "responsive-workspace");
const profile = await mkdtemp(join(tmpdir(), "3dam-responsive-"));

const scenarios = [
  { name: "workspace-320", width: 320, height: 720, path: "/?view=grid", touch: true },
  { name: "workspace-360-table", width: 360, height: 760, path: "/?view=table", touch: true },
  { name: "workspace-768", width: 768, height: 800, path: "/?view=grid", touch: true },
  {
    name: "workspace-split-1024-table",
    width: 1024,
    height: 800,
    path: "/?view=table",
    touch: false,
  },
];

await mkdir(outputDir, { recursive: true });
const browser = spawn(
  chromium,
  [
    "--headless=new",
    "--disable-gpu",
    "--no-sandbox",
    "--remote-debugging-port=0",
    `--user-data-dir=${profile}`,
    "about:blank",
  ],
  { stdio: ["ignore", "ignore", "pipe"] },
);

let browserErrors = "";
browser.stderr.setEncoding("utf8");
browser.stderr.on("data", (chunk) => {
  browserErrors += chunk;
});

const delay = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));

async function devtoolsPort() {
  const activePort = join(profile, "DevToolsActivePort");
  for (let attempt = 0; attempt < 100; attempt += 1) {
    try {
      const [port] = (await readFile(activePort, "utf8")).trim().split("\n");
      if (port) return Number(port);
    } catch {
      // Chromium creates this file after its browser process and debugging socket are ready.
    }
    // Some distro wrappers keep Chromium's effective profile elsewhere. Its canonical stderr
    // announcement is an equivalent source for the ephemeral debugging port.
    const announced = browserErrors.match(/DevTools listening on ws:\/\/127\.0\.0\.1:(\d+)\//);
    if (announced) return Number(announced[1]);
    await delay(100);
  }
  throw new Error(`Chromium did not expose DevTools in time.\n${browserErrors}`);
}

function connect(url) {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(url);
    socket.addEventListener("open", () => resolve(socket), { once: true });
    socket.addEventListener("error", reject, { once: true });
  });
}

function cdp(socket) {
  let id = 0;
  const pending = new Map();
  socket.addEventListener("message", (event) => {
    const message = JSON.parse(String(event.data));
    if (!message.id) return;
    const waiter = pending.get(message.id);
    if (!waiter) return;
    pending.delete(message.id);
    if (message.error) waiter.reject(new Error(message.error.message));
    else waiter.resolve(message.result);
  });
  return (method, params = {}) =>
    new Promise((resolve, reject) => {
      const messageId = ++id;
      pending.set(messageId, { resolve, reject });
      socket.send(JSON.stringify({ id: messageId, method, params }));
    });
}

const fixtureAssets = [
  ["fixture-image-1", "worn-metal-albedo.png", "image", "png", 1_482_304, "permissive"],
  ["fixture-audio-1", "industrial-door-loop.wav", "audio", "wav", 8_421_376, "attribution"],
  ["fixture-model-1", "warehouse-crane-highpoly.glb", "model", "glb", 24_219_648, "unknown"],
  ["fixture-image-2", "warning-stripes-reference.jpg", "image", "jpg", 932_864, "restricted"],
].map(([id, name, media, format, size, status]) => ({
  id,
  name,
  media,
  format,
  size,
  license: { id: null, status },
  top_tags: [],
  origin: "local",
  key_attrs: {},
  favorite: false,
  source_id: null,
}));

function fixtureFor(requestUrl) {
  const path = new URL(requestUrl).pathname;
  if (path === "/api/version")
    return { api: "v1", server: "responsive fixture", capabilities: [], auth: "anonymous" };
  if (path === "/api/v1/whoami")
    return { identity: null, scopes: ["read"], anonymous: true, account: null, restricted: false };
  if (path === "/api/v1/query") return { items: fixtureAssets, cursor: null };
  if (path === "/api/v1/stats")
    return {
      total: fixtureAssets.length,
      by_media: { image: 2, audio: 1, model: 1 },
      by_source: {},
      tags: {},
      unanalyzed: 0,
      sources: 0,
    };
  if (path === "/api/v1/jobs/list")
    return {
      items: [
        {
          id: "responsive-scan",
          kind: "scan",
          state: "running",
          progress: { done: 7, total: 20, current: "Textures/worn-metal-albedo.png" },
          error: null,
          sources: [],
        },
      ],
      cursor: null,
    };
  if (path === "/api/v1/sources" || path === "/api/v1/collections" || path === "/api/v1/duplicates")
    return [];
  return {};
}

let socket;
try {
  const port = await devtoolsPort();
  const target = await fetch(
    `http://127.0.0.1:${port}/json/new?${encodeURIComponent(`${baseUrl}/`)}`,
    { method: "PUT" },
  ).then((response) => response.json());
  socket = await connect(target.webSocketDebuggerUrl);
  const send = cdp(socket);
  await send("Page.enable");
  await send("Runtime.enable");
  if (useFixtureApi) {
    socket.addEventListener("message", (event) => {
      const message = JSON.parse(String(event.data));
      if (message.method !== "Fetch.requestPaused") return;
      const { requestId, request } = message.params;
      const body = Buffer.from(JSON.stringify(fixtureFor(request.url))).toString("base64");
      void send("Fetch.fulfillRequest", {
        requestId,
        responseCode: 200,
        responseHeaders: [
          { name: "content-type", value: "application/json" },
          { name: "cache-control", value: "no-store" },
        ],
        body,
      }).catch(() => {
        // A fast navigation can retire an intercepted request before the response is delivered.
      });
    });
    await send("Fetch.enable", {
      patterns: [{ urlPattern: `${new URL(baseUrl).origin}/api/*` }],
    });
  }

  const manifest = [];
  for (const scenario of scenarios) {
    await send("Emulation.setDeviceMetricsOverride", {
      width: scenario.width,
      height: scenario.height,
      deviceScaleFactor: 1,
      mobile: scenario.touch,
      screenWidth: scenario.width,
      screenHeight: scenario.height,
    });
    await send("Emulation.setTouchEmulationEnabled", {
      enabled: scenario.touch,
      ...(scenario.touch ? { maxTouchPoints: 1 } : {}),
    });
    await send("Page.navigate", { url: `${baseUrl}${scenario.path}` });
    await delay(1_500);

    const evaluated = await send("Runtime.evaluate", {
      expression: `(() => {
        const root = document.documentElement;
        const body = document.body;
        const browser = document.querySelector('[data-shortcut-region="browser"]');
        return {
          title: document.title,
          viewportWidth: root.clientWidth,
          pageScrollWidth: Math.max(root.scrollWidth, body?.scrollWidth ?? 0),
          browserWidth: browser ? Math.round(browser.getBoundingClientRect().width) : null,
          toolbarVisible: Boolean(document.querySelector('.browser-toolbar')),
          statusVisible: Boolean(document.querySelector('footer')),
          searchVisible: Boolean(document.querySelector('#asset-search')?.getClientRects().length),
          tableVisible: Boolean(document.querySelector('.asset-table-columns')),
          cancelVisible: Boolean(document.querySelector('button[aria-label^="Cancel "]')?.getClientRects().length),
          connectionVisible: Boolean(document.querySelector('[role="status"]')?.getClientRects().length),
          signInVisible: [...document.querySelectorAll('button')].some(
            (button) => button.textContent?.includes('Sign in') && button.getClientRects().length,
          ),
        };
      })()`,
      returnByValue: true,
    });
    const metrics = evaluated.result.value;
    if (!metrics.toolbarVisible || !metrics.statusVisible)
      throw new Error(`${scenario.name}: workspace chrome did not render (${JSON.stringify(metrics)})`);
    if (!metrics.searchVisible || !metrics.cancelVisible || !metrics.connectionVisible || !metrics.signInVisible)
      throw new Error(`${scenario.name}: a required action is not visible (${JSON.stringify(metrics)})`);
    if (scenario.path.includes("view=table") && !metrics.tableVisible)
      throw new Error(`${scenario.name}: responsive table did not render (${JSON.stringify(metrics)})`);
    if (metrics.pageScrollWidth > metrics.viewportWidth + 1)
      throw new Error(`${scenario.name}: page overflow ${metrics.pageScrollWidth}px > ${metrics.viewportWidth}px`);

    const image = await send("Page.captureScreenshot", {
      format: "png",
      captureBeyondViewport: false,
      fromSurface: true,
    });
    const filename = `${scenario.name}.png`;
    await writeFile(join(outputDir, filename), Buffer.from(image.data, "base64"));
    manifest.push({ ...scenario, ...metrics, file: filename });
  }
  await writeFile(
    join(outputDir, "manifest.json"),
    `${JSON.stringify({ capturedAt: new Date().toISOString(), baseUrl, fixtureApi: useFixtureApi, scenarios: manifest }, null, 2)}\n`,
  );
} finally {
  socket?.close();
  browser.kill("SIGTERM");
  await rm(profile, { recursive: true, force: true });
}
