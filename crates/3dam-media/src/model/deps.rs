//! External model dependency and texture-sidecar resolution.

use serde_json::Value;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

// ── dependency size (external textures / buffers) ──────────────────────────────
//
// The cheap tier statting a model's *referenced* companion files so the catalog can report the
// whole asset's footprint, not just the mesh container. It reads text headers / a bounded byte
// scan and stats siblings — it never decodes a texture or a geometry buffer.

/// Recognised texture extensions for the sibling scan (lower-case, no dot).
const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "tga", "tif", "tiff", "bmp", "webp", "dds", "exr", "psd", "gif",
];

/// Cap on how much of a (potentially huge) FBX to scan for texture-path strings.
const FBX_SCAN_CAP: u64 = 64 * 1024 * 1024;

/// Total on-disk bytes of the files a model references beyond its own container — external textures,
/// glTF `.bin` buffers, an OBJ's `.mtl` and its maps, plus the sibling maps that belong to the same
/// texture set by naming convention. `None` when the format is self-contained (GLB/STL/PLY embed or
/// carry no external data) or nothing external resolves.
pub(super) fn dependency_bytes(path: &Path, format: &str) -> Option<i64> {
    let files = match format {
        "gltf" => gltf_deps(path),
        "obj" => obj_deps(path),
        "fbx" => fbx_deps(path),
        _ => Vec::new(), // glb (embedded), stl, ply carry no external references
    };
    let self_canon = std::fs::canonicalize(path).ok();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut total: i64 = 0;
    for f in files {
        let canon = std::fs::canonicalize(&f).unwrap_or(f);
        if Some(&canon) == self_canon.as_ref() || !seen.insert(canon.clone()) {
            continue; // never count the model file itself, and dedup shared maps
        }
        if let Ok(md) = std::fs::metadata(&canon) {
            if md.is_file() {
                total += md.len() as i64;
            }
        }
    }
    (total > 0).then_some(total)
}

/// External `buffers[].uri` / `images[].uri` a loose `.gltf` points at (data-URIs are inline).
fn gltf_deps(path: &Path) -> Vec<PathBuf> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    if file
        .metadata()
        .map(|meta| meta.len() > 16 * 1024 * 1024)
        .unwrap_or(true)
    {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.take(16 * 1024 * 1024).read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    let Ok(root) = serde_json::from_slice::<Value>(&bytes) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for key in ["buffers", "images"] {
        let Some(arr) = root.get(key).and_then(Value::as_array) else {
            continue;
        };
        for item in arr {
            if let Some(uri) = item.get("uri").and_then(Value::as_str) {
                if uri.starts_with("data:") {
                    continue;
                }
                out.push(dir.join(uri.replace("%20", " ")));
            }
        }
    }
    out
}

/// The `.mtl` an OBJ names in `mtllib`, plus every texture map it references (`map_Kd`, `bump`, …).
fn obj_deps(path: &Path) -> Vec<PathBuf> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let Ok(f) = File::open(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        if let Some(rest) = line.trim_start().strip_prefix("mtllib ") {
            for name in rest.split_whitespace() {
                let mtl = dir.join(name);
                out.extend(mtl_maps(&mtl));
                out.push(mtl);
            }
        }
    }
    out
}

/// Texture maps referenced by a `.mtl` file. Map statements may carry options
/// (`map_Kd -bm 0.2 tex.png`), so the filename is the last whitespace token.
fn mtl_maps(mtl: &Path) -> Vec<PathBuf> {
    let dir = mtl.parent().unwrap_or_else(|| Path::new("."));
    let Ok(f) = File::open(mtl) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let line = line.trim_start();
        let is_map = line.starts_with("map_")
            || line.starts_with("bump ")
            || line.starts_with("disp ")
            || line.starts_with("decal ")
            || line.starts_with("norm ")
            || line.starts_with("refl ");
        if is_map {
            if let Some(file) = line.split_whitespace().next_back() {
                out.push(dir.join(file.replace('\\', "/")));
            }
        }
    }
    out
}

/// FBX texture paths (baked as `RelativeFilename`/`Filename` strings), resolved against the model
/// dir + sibling texture folders, plus the rest of each texture's naming-convention set — many FBX
/// exports reference only a subset (e.g. mask + normal) of maps that ship alongside.
fn fbx_deps(path: &Path) -> Vec<PathBuf> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let Ok(mut f) = File::open(path) else {
        return Vec::new();
    };
    let cap = f
        .metadata()
        .map(|m| m.len().min(FBX_SCAN_CAP))
        .unwrap_or(FBX_SCAN_CAP);
    let mut buf = vec![0u8; cap as usize];
    let Ok(n) = f.read(&mut buf) else {
        return Vec::new();
    };
    buf.truncate(n);

    let mut out = Vec::new();
    for reference in extract_texture_paths(&buf) {
        if let Some(resolved) = resolve_sibling(dir, &reference) {
            out.extend(companion_siblings(&resolved));
            out.push(resolved);
        }
    }
    out
}

/// Pull filename-like ASCII runs ending in an image extension out of a byte blob (FBX stores texture
/// paths as plain strings, binary or ASCII).
fn extract_texture_paths(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut run = String::new();
    let mut flush = |run: &mut String| {
        if is_image_name(run) {
            out.push(std::mem::take(run));
        } else {
            run.clear();
        }
    };
    for &b in bytes {
        // Printable ASCII plus the separators that appear inside paths.
        if b.is_ascii_graphic() || b == b' ' {
            run.push(b as char);
        } else {
            flush(&mut run);
        }
    }
    flush(&mut run);
    out
}

/// Whether a string looks like a path ending in a known image extension.
fn is_image_name(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    IMAGE_EXTS.iter().any(|e| lower.ends_with(&format!(".{e}")))
}

/// Sibling texture folders game/DCC exports conventionally use, mirroring the render crate's
/// resolver so the counted set matches what the thumbnail actually draws.
const TEXTURE_DIRS: &[&str] = &[
    "Textures",
    "textures",
    "Texture",
    "texture",
    "../Textures",
    "../textures",
    "Maps",
    "maps",
    "tex",
];

/// Resolve an FBX/DCC texture reference to an existing file: the path as-given (relative to the
/// model dir), then the bare filename in the model dir and the conventional sibling folders.
fn resolve_sibling(dir: &Path, reference: &str) -> Option<PathBuf> {
    let cleaned = reference.replace('\\', "/");
    let cleaned = cleaned.trim_start_matches("./");
    let mut cands = vec![dir.join(cleaned)];
    if let Some(name) = Path::new(cleaned).file_name() {
        cands.push(dir.join(name));
        for sib in TEXTURE_DIRS {
            cands.push(dir.join(sib).join(name));
        }
    }
    cands.into_iter().find(|c| c.is_file())
}

/// Every image sibling that shares a resolved texture's asset base name (`T_Table_Mask.png` →
/// `T_Table_*`), so a whole PBR set is counted even when the FBX names only part of it.
fn companion_siblings(file: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(stem)) = (file.parent(), file.file_stem().and_then(|s| s.to_str())) else {
        return Vec::new();
    };
    let Some(base) = asset_base(stem) else {
        return vec![file.to_path_buf()];
    };
    let base = base.to_ascii_lowercase();
    let prefix = format!("{base}_");
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![file.to_path_buf()];
    };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| has_image_ext(p))
        .filter(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_ascii_lowercase())
                .is_some_and(|s| s == base || s.starts_with(&prefix))
        })
        .collect()
}

/// Strip a trailing `_<channel>` token to recover the shared asset base name. A token counts if it's
/// a known channel word or a short alphabetic code (`_B`, `_ORM`, `_Mask`); `None` otherwise.
pub(super) fn asset_base(stem: &str) -> Option<String> {
    let (base, tok) = stem.rsplit_once('_')?;
    if base.is_empty() {
        return None;
    }
    let t = tok.to_ascii_lowercase();
    let known = matches!(
        t.as_str(),
        "basecolor"
            | "albedo"
            | "diffuse"
            | "color"
            | "colour"
            | "normal"
            | "normalmap"
            | "roughness"
            | "metallic"
            | "metalness"
            | "emissive"
            | "emission"
            | "occlusion"
            | "orm"
            | "rma"
            | "arm"
            | "mask"
            | "height"
            | "ao"
            | "specular"
            | "gloss"
            | "opacity"
    );
    let short_alpha = (1..=4).contains(&t.len()) && t.chars().all(|c| c.is_ascii_alphabetic());
    (known || short_alpha).then(|| base.to_string())
}

fn has_image_ext(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| IMAGE_EXTS.contains(&e.as_str()))
}

/// Reuse the geometry document rather than reopening the primary model.
pub(super) fn ingest_gltf_deps(
    path: &Path,
    root: &Value,
    session: &mut crate::ingest::Session<'_, '_>,
) -> Option<i64> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut files = Vec::new();
    for key in ["buffers", "images"] {
        if let Some(items) = root.get(key).and_then(Value::as_array) {
            for item in items {
                if let Some(uri) = item.get("uri").and_then(Value::as_str) {
                    if !uri.starts_with("data:") {
                        files.push(dir.join(uri.replace("%20", " ")));
                    }
                }
            }
        }
    }
    ingest_total(path, files, session)
}

/// Geometry and mtllib discovery share one bounded OBJ snapshot. Companion text
/// consumes the same byte budget, and an incomplete companion leaves the total unknown.
pub(super) fn ingest_obj_deps(
    path: &Path,
    bytes: &[u8],
    session: &mut crate::ingest::Session<'_, '_>,
) -> Option<i64> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut files = BTreeSet::new();
    let mut materials = BTreeSet::new();
    for line in std::str::from_utf8(bytes).ok()?.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("mtllib ") {
            for name in rest.split_whitespace() {
                materials.insert(name.to_owned());
            }
        }
    }
    for name in materials {
        let Some((mtl, mut file)) = open_contained_material(dir, &name, session) else {
            session.deferred = true;
            return None;
        };
        if let Some(bytes) = session.complete_open_file(&mut file) {
            let dir = mtl.parent().unwrap_or_else(|| Path::new("."));
            for line in std::str::from_utf8(&bytes).ok()?.lines() {
                let line = line.trim_start();
                if line.starts_with("map_")
                    || ["bump ", "disp ", "decal ", "norm ", "refl "]
                        .iter()
                        .any(|prefix| line.starts_with(prefix))
                {
                    if let Some(name) = line.split_whitespace().next_back() {
                        files.insert(dir.join(name.replace('\\', "/")));
                    }
                }
            }
        } else if session.deferred {
            return None;
        }
        files.insert(mtl);
    }
    ingest_total(path, files.into_iter().collect(), session)
}

/// Resolve every material component through held directory descriptors. Nofollow
/// opens close path replacement races; nonblocking opens let us reject FIFOs.
fn open_contained_material(
    dir: &Path,
    name: &str,
    session: &mut crate::ingest::Session<'_, '_>,
) -> Option<(PathBuf, File)> {
    use std::path::Component;
    let name = name.replace('\\', "/");
    let relative = Path::new(&name);
    if relative.is_absolute()
        || name.contains(':')
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return None;
    }
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::OpenOptionsExt;
        if !session.admit_metadata() {
            return None;
        }
        let mut candidate = std::fs::canonicalize(dir).ok()?;
        if !session.admit_metadata() {
            return None;
        }
        let mut parent = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&candidate)
            .ok()?;
        let mut components = relative
            .components()
            .filter_map(|component| {
                if let Component::Normal(part) = component {
                    Some(part)
                } else {
                    None
                }
            })
            .peekable();
        while let Some(part) = components.next() {
            candidate.push(part);
            if !session.admit_metadata() {
                return None;
            }
            let part = CString::new(part.as_bytes()).ok()?;
            let final_component = components.peek().is_none();
            let flags = libc::O_RDONLY
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | if final_component {
                    0
                } else {
                    libc::O_DIRECTORY
                };
            // SAFETY: parent owns a live directory fd, part is NUL-terminated,
            // and no creation flag requires a mode argument.
            let descriptor = unsafe { libc::openat(parent.as_raw_fd(), part.as_ptr(), flags) };
            if descriptor < 0 {
                return None;
            }
            // SAFETY: openat returned a fresh descriptor transferred once to File.
            let file = unsafe { File::from_raw_fd(descriptor) };
            if final_component {
                return Some((candidate, file));
            }
            parent = file;
        }
        None
    }
    #[cfg(not(unix))]
    {
        // Preserve confinement until a descriptor-relative Windows opener exists.
        let _ = dir;
        session.deferred = true;
        None
    }
}

fn ingest_total(
    path: &Path,
    files: Vec<PathBuf>,
    session: &mut crate::ingest::Session<'_, '_>,
) -> Option<i64> {
    if !session.admit_metadata() {
        return None;
    }
    let own = std::fs::canonicalize(path).ok();
    let mut seen = BTreeSet::new();
    let mut total = 0i64;
    for file in files {
        if !session.admit_metadata() {
            return None;
        }
        let canon = std::fs::canonicalize(&file).unwrap_or(file);
        if Some(&canon) == own.as_ref() || !seen.insert(canon.clone()) {
            continue;
        }
        if !session.admit_metadata() {
            return None;
        }
        if let Ok(meta) = std::fs::metadata(canon) {
            if meta.is_file() {
                total = total.checked_add(i64::try_from(meta.len()).ok()?)?;
            }
        }
    }
    (total > 0).then_some(total)
}

/// The FBX byte snapshot used for geometry is also the texture-string scan.
pub(super) fn ingest_fbx_deps(
    path: &Path,
    bytes: &[u8],
    session: &mut crate::ingest::Session<'_, '_>,
) -> Option<i64> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut files = BTreeSet::new();
    for reference in extract_texture_paths(bytes) {
        if !session.active() {
            session.deferred = true;
            return None;
        }
        let Some(resolved) = ingest_resolve_sibling(dir, &reference, session) else {
            if session.deferred {
                return None;
            }
            continue;
        };
        files.insert(resolved.clone());
        if let (Some(parent), Some(stem)) = (
            resolved.parent(),
            resolved.file_stem().and_then(|stem| stem.to_str()),
        ) {
            if let Some(base) = asset_base(stem) {
                let base = base.to_ascii_lowercase();
                let prefix = format!("{base}_");
                if !session.admit_metadata() {
                    return None;
                }
                if let Ok(mut entries) = std::fs::read_dir(parent) {
                    loop {
                        if !session.admit_metadata() {
                            return None;
                        }
                        let Some(entry) = entries.next() else { break };
                        let Ok(entry) = entry else {
                            continue;
                        };
                        let file = entry.path();
                        if has_image_ext(&file)
                            && file
                                .file_stem()
                                .and_then(|stem| stem.to_str())
                                .map(|stem| {
                                    let stem = stem.to_ascii_lowercase();
                                    stem == base || stem.starts_with(&prefix)
                                })
                                .unwrap_or(false)
                        {
                            files.insert(file);
                        }
                    }
                }
            }
        }
        if files.len() >= 4096 {
            session.deferred = true;
            return None;
        }
    }
    ingest_total(path, files.into_iter().collect(), session)
}

fn ingest_resolve_sibling(
    dir: &Path,
    reference: &str,
    session: &mut crate::ingest::Session<'_, '_>,
) -> Option<PathBuf> {
    let cleaned = reference.replace('\\', "/");
    let cleaned = cleaned.trim_start_matches("./");
    let mut candidates = vec![dir.join(cleaned)];
    if let Some(name) = Path::new(cleaned).file_name() {
        candidates.push(dir.join(name));
        for sibling in TEXTURE_DIRS {
            candidates.push(dir.join(sibling).join(name));
        }
    }
    for candidate in candidates {
        if !session.admit_metadata() {
            return None;
        }
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
