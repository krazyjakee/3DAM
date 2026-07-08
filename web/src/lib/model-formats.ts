// 3D format coverage for preview (issue #18) — the single web-side source of truth for which model
// formats get what kind of preview. Mirrors the server renderer's `supports_format`
// (crates/3dam-render/src/lib.rs): one Assimp decode feeds both the turntable thumbnail and the
// interactive island, so the two clients agree by construction.
//
// Coverage tiers:
//  • Interactive island (orbit/zoom + lighting/wireframe controls) — every Assimp-decodable mesh
//    format below. The DOM fetches the server-decoded `DMSH` preview blob, so the browser never
//    resolves external buffers/textures.
//  • Thumbnail-only — `.blend`: Assimp can't decode a modern .blend's geometry, so there's no
//    interactive mesh; the server thumbnail surfaces Blender's own embedded preview image when present.
//  • No preview (metadata only) — the USD family (`usd`/`usda`/`usdc`/`usdz`): no Assimp USD importer
//    yet, so these show a clear "preview not available" state (never a silent blank), pending a
//    dedicated USD path (tech-spec 06 follow-up).

/** Model formats that get the interactive 3D island. Assimp's professional-interchange range minus
 *  `.blend` (thumbnail-only). Keep in lockstep with `dam-render`'s `supports_format`. */
export const INTERACTIVE_3D_FORMATS: ReadonlySet<string> = new Set([
  "gltf",
  "glb",
  "fbx",
  "obj",
  "stl",
  "ply",
  "dae",
  "3ds",
  "x",
  "lwo",
  "lws",
  "ase",
  "ms3d",
  "off",
  "dxf",
]);

/** True when a model format has an interactive 3D preview (vs thumbnail-only / metadata-only). */
export function hasInteractive3D(format: string): boolean {
  return INTERACTIVE_3D_FORMATS.has(format.toLowerCase());
}
