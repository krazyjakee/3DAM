import { readFile, readdir, stat } from "node:fs/promises";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const webRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const distRoot = join(webRoot, "dist");
const manifestPath = join(distRoot, ".vite", "manifest.json");
const budgets = JSON.parse(await readFile(join(webRoot, "bundle-budgets.json"), "utf8"));
const manifest = JSON.parse(await readFile(manifestPath, "utf8"));

const posix = (path) => path.split(sep).join("/");
const manifestEntries = Object.entries(manifest);
const initialKeys = new Set();

function addSynchronousImports(key) {
  if (initialKeys.has(key)) return;
  const entry = manifest[key];
  if (!entry) throw new Error(`bundle manifest refers to missing module ${key}`);
  initialKeys.add(key);
  for (const dependency of entry.imports ?? []) addSynchronousImports(dependency);
}

for (const [key, entry] of manifestEntries) {
  if (entry.isEntry) addSynchronousImports(key);
}

async function filesBelow(directory) {
  const files = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) files.push(...await filesBelow(path));
    else files.push(path);
  }
  return files;
}

async function sizedArtifact(file) {
  const brotli = `${file}.br`;
  const bytes = (await stat(file)).size;
  // The production compressor deliberately skips bodies below 256 bytes because a sidecar and an
  // extra cache variant cost more than they save. Those assets are transferred raw, so their raw
  // size is also the effective compressed-transfer size for budget accounting.
  const brotliBytes = await stat(brotli)
    .then((metadata) => metadata.size)
    .catch((error) => {
      if (error?.code === "ENOENT" && bytes < 256) return bytes;
      throw error;
    });
  return {
    file: posix(relative(distRoot, file)),
    bytes,
    brotliBytes,
  };
}

const emittedFiles = await filesBelow(distRoot);
const javascriptFiles = emittedFiles.filter((file) => file.endsWith(".js"));
const wasmFiles = emittedFiles.filter((file) => file.endsWith(".wasm"));
const initialJsFiles = new Set(
  [...initialKeys]
    .map((key) => manifest[key]?.file)
    .filter((file) => typeof file === "string" && file.endsWith(".js")),
);
const initialJavaScript = await Promise.all(
  javascriptFiles.filter((file) => initialJsFiles.has(posix(relative(distRoot, file)))).map(sizedArtifact),
);
const lazyJavaScript = await Promise.all(
  javascriptFiles.filter((file) => !initialJsFiles.has(posix(relative(distRoot, file)))).map(sizedArtifact),
);
const lazyWasm = await Promise.all(wasmFiles.map(sizedArtifact));

if (initialJavaScript.length === 0) throw new Error("bundle budget: no initial JavaScript found");
if (lazyJavaScript.length === 0) throw new Error("bundle budget: no lazy JavaScript found");
if (lazyWasm.length === 0) throw new Error("bundle budget: no lazy WASM found");

// An asset reachable through only static manifest imports is fetched with the application shell.
// The model WASM must remain below a dynamic-import boundary so browsing and audio stay GPU-free.
for (const key of initialKeys) {
  const eagerWasm = (manifest[key].assets ?? []).find((asset) => asset.endsWith(".wasm"));
  if (eagerWasm) throw new Error(`bundle budget: WASM became an initial asset (${eagerWasm})`);
}

const failures = [];
function checkArtifacts(label, artifacts, budget) {
  for (const artifact of artifacts) {
    if (artifact.bytes > budget.perArtifactBytes) {
      failures.push(`${label} ${artifact.file}: ${artifact.bytes} > ${budget.perArtifactBytes} bytes`);
    }
    if (artifact.brotliBytes > budget.perArtifactBrotliBytes) {
      failures.push(
        `${label} ${artifact.file} (br): ${artifact.brotliBytes} > ${budget.perArtifactBrotliBytes} bytes`,
      );
    }
  }
}

checkArtifacts("initial JS", initialJavaScript, budgets.initialJavaScript);
checkArtifacts("lazy JS", lazyJavaScript, budgets.lazyJavaScript);
checkArtifacts("lazy WASM", lazyWasm, budgets.lazyWasm);

const initialBytes = initialJavaScript.reduce((total, artifact) => total + artifact.bytes, 0);
const initialBrotliBytes = initialJavaScript.reduce(
  (total, artifact) => total + artifact.brotliBytes,
  0,
);
if (initialBytes > budgets.initialJavaScript.totalBytes) {
  failures.push(`initial JS total: ${initialBytes} > ${budgets.initialJavaScript.totalBytes} bytes`);
}
if (initialBrotliBytes > budgets.initialJavaScript.totalBrotliBytes) {
  failures.push(
    `initial JS total (br): ${initialBrotliBytes} > ${budgets.initialJavaScript.totalBrotliBytes} bytes`,
  );
}

for (const [label, artifacts] of [
  ["initial JS", initialJavaScript],
  ["lazy JS", lazyJavaScript],
  ["lazy WASM", lazyWasm],
]) {
  for (const artifact of artifacts.sort((left, right) => left.file.localeCompare(right.file))) {
    console.log(
      `${label.padEnd(12)} ${artifact.file.padEnd(52)} ${String(artifact.bytes).padStart(9)} raw ${String(artifact.brotliBytes).padStart(9)} br`,
    );
  }
}

if (failures.length > 0) {
  console.error("\nBundle budget exceeded:");
  for (const failure of failures) console.error(`- ${failure}`);
  process.exitCode = 1;
} else {
  console.log(`\nBundle budgets passed (${initialBytes} initial JS bytes).`);
}
