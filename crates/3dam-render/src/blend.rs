//! Headless Blender bridge for **modern** `.blend` files.
//!
//! Assimp's built-in Blender importer only understands the *legacy* Blender file DNA (≤ 2.7x). A
//! 2.8+/3.x/4.x `.blend` fails to decode — Assimp bails with errors like *"Number of vertices is
//! larger than the corresponding array"* because the on-disk structures have moved on. The only
//! reliable reader of a modern `.blend` is Blender itself, so when a Blender binary is available we
//! run it headless (`--background`) to export the scene to a temporary GLB, which the normal Assimp
//! path ([`crate::model::load`]) then decodes with full geometry + materials + textures.
//!
//! ## Off by default, fail-soft
//! The bridge only fires for a `.blend` the fast in-process Assimp path could not decode, and only
//! when a Blender binary is resolvable — `DAM_BLENDER_BIN` (an explicit path), else `blender` on
//! `PATH`. A host with no Blender provisioned (a typical server) simply never spawns it and the
//! preview degrades to the honest typed tile, exactly as an unsupported format does. There is no
//! `server.db` feature flag for this because model decode runs in the engine (`dam-core`), which
//! also serves the embedded CLI/GUI path where `server.db` does not exist — provisioning the binary
//! *is* the enable switch.
//!
//! ## Safety
//! Blender runs with `--factory-startup --disable-autoexec`, so a hostile `.blend`'s embedded
//! drivers/Python scripts never execute, and no user add-ons/preferences perturb the export. The
//! output path is passed via an environment variable (not string-interpolated into the Python
//! expression), so there is nothing to escape or inject. A hard timeout bounds a wedged Blender.

use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Extract Blender's **own saved preview image** from a `.blend` — the thumbnail a `.blend` carries
/// so OS file managers (and Blender's own file browser) can show it — as PNG bytes.
///
/// This is the cheap, dependency-free path for `.blend` previews: a modern `.blend` can't be decoded
/// to geometry by Assimp (see [`convert_to_glb`]), but most artist-saved files embed a small preview,
/// and using it is instant — no Blender process, no GPU, no full-scene render (which is unreasonable
/// on a large `.blend`). Pure parsing of the file-block stream; scanning is bounded so the file size
/// doesn't matter (the preview sits near the start).
///
/// `None` when the file is compressed (zstd/gzip `.blend` — the plain header is absent), carries no
/// preview block, or is malformed — the caller then falls through to the geometry path / typed tile.
pub fn embedded_thumbnail_png(path: &Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let mut r = BufReader::new(file);

    // Header: "BLENDER" (7) · pointer-size ('_'=4, '-'=8) · endianness ('v'=little, 'V'=big) · ver(3).
    let mut hdr = [0u8; 12];
    r.read_exact(&mut hdr).ok()?;
    if &hdr[0..7] != b"BLENDER" {
        return None; // compressed or not a .blend — no readable block stream
    }
    let ptr_size: i64 = match hdr[7] {
        b'_' => 4,
        b'-' => 8,
        _ => return None,
    };
    let little = match hdr[8] {
        b'v' => true,
        b'V' => false,
        _ => return None,
    };
    let u32d = |b: [u8; 4]| {
        if little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        }
    };
    let i32d = |b: [u8; 4]| {
        if little {
            i32::from_le_bytes(b)
        } else {
            i32::from_be_bytes(b)
        }
    };

    // Walk file-blocks: code[4] · size[u32] · old_ptr[ptr] · sdna[u32] · count[u32] · data[size].
    // Bounded so a pathological/huge file is never fully scanned — the `TEST` preview block, when
    // present, sits among the first handful of blocks.
    for _ in 0..4096 {
        let mut code = [0u8; 4];
        if r.read_exact(&mut code).is_err() || &code == b"ENDB" {
            return None;
        }
        let mut sz = [0u8; 4];
        r.read_exact(&mut sz).ok()?;
        let size = u32d(sz) as u64;
        r.seek(SeekFrom::Current(ptr_size + 8)).ok()?; // skip old_ptr + sdna + count

        if &code == b"TEST" {
            // Preview block payload: width i32, height i32, then width*height RGBA8 texels.
            let mut wh = [0u8; 8];
            r.read_exact(&mut wh).ok()?;
            let w = i32d(wh[0..4].try_into().ok()?);
            let h = i32d(wh[4..8].try_into().ok()?);
            if w <= 0 || h <= 0 || w > 4096 || h > 4096 {
                return None;
            }
            let (w, h) = (w as u32, h as u32);
            if 8 + (w as u64) * (h as u64) * 4 != size {
                return None; // not the layout we expect — don't trust it
            }
            let mut rgba = vec![0u8; (w * h * 4) as usize];
            r.read_exact(&mut rgba).ok()?;
            return encode_rgba_png(w, h, rgba);
        }
        r.seek(SeekFrom::Current(size as i64)).ok()?;
    }
    None
}

fn encode_rgba_png(w: u32, h: u32, rgba: Vec<u8>) -> Option<Vec<u8>> {
    use image::ImageEncoder;
    let img = image::RgbaImage::from_raw(w, h, rgba)?;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgba8)
        .ok()?;
    Some(out)
}

/// Explicit override pointing at a Blender executable. When unset the bridge looks for `blender` on
/// `PATH`. Set it to pin a specific Blender (or to enable the bridge on a host where Blender is not
/// on the service `PATH`).
const BLENDER_BIN_ENV: &str = "DAM_BLENDER_BIN";

/// Env var the export Python reads the output path from — avoids interpolating a path into source.
const OUT_ENV: &str = "DAM_BLEND_GLB_OUT";

/// Hard cap on a single conversion. Blender can wedge on a pathological file; we'd rather fall back
/// to the typed tile than hang a thumbnail/preview worker forever.
const CONVERT_TIMEOUT: Duration = Duration::from_secs(90);

/// A successful conversion: the exported GLB, kept alive by its owning temp dir (removed on drop).
pub struct Converted {
    _dir: tempfile::TempDir,
    glb: PathBuf,
}

impl Converted {
    /// Path to the exported GLB — feed it to the normal Assimp decode.
    pub fn path(&self) -> &Path {
        &self.glb
    }
}

/// Resolve the Blender executable: the `DAM_BLENDER_BIN` override, else the bare `blender` command
/// (found on `PATH` at spawn time). `None` only when the override is set to an empty string.
fn blender_bin() -> Option<PathBuf> {
    match std::env::var_os(BLENDER_BIN_ENV) {
        Some(p) if !p.is_empty() => Some(PathBuf::from(p)),
        Some(_) => None,
        None => Some(PathBuf::from("blender")),
    }
}

/// Convert a modern `.blend` to a temporary GLB via headless Blender. Returns `None` — the caller
/// then fails soft to the typed tile — when Blender is not provisioned, the spawn fails, it times
/// out, exits non-zero, or writes no output. Blocking: spawns a subprocess and waits, so call from
/// `spawn_blocking` (both render entry points already run there).
pub fn convert_to_glb(src: &Path) -> Option<Converted> {
    let bin = blender_bin()?;
    let dir = tempfile::Builder::new()
        .prefix("dam-blend-")
        .tempdir()
        .ok()?;
    let glb = dir.path().join("scene.glb");

    // The GLB path travels via the environment (see `OUT_ENV`), so the Python expression is a fixed
    // literal with nothing to escape.
    let export = "import bpy, os; \
         bpy.ops.export_scene.gltf(filepath=os.environ['DAM_BLEND_GLB_OUT'], export_format='GLB')";

    let child = Command::new(&bin)
        .arg("--background")
        .arg("--factory-startup")
        .arg("--disable-autoexec")
        .arg(src)
        .arg("--python-expr")
        .arg(export)
        .env(OUT_ENV, &glb)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let status = wait_with_timeout(child, CONVERT_TIMEOUT)?;
    if !status.success() {
        return None;
    }
    // Blender can exit 0 yet write nothing on a scene it couldn't export; require a real, non-empty
    // file before we hand it to the decoder.
    let nonempty = std::fs::metadata(&glb)
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    nonempty.then_some(Converted { _dir: dir, glb })
}

/// Wait for `child`, killing it (and reaping it) if it outlives `timeout`. `None` on timeout or a
/// wait error. Polls rather than blocking so the timeout is actually enforced (`std` has no
/// wait-with-timeout); a coarse interval is fine for a seconds-scale subprocess.
fn wait_with_timeout(mut child: Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal little-endian, 8-byte-pointer `.blend` byte stream: the header, the given
    /// `(code, payload)` blocks (their `size` and the skipped pointer/DNA fields synthesised), then a
    /// terminating `ENDB`. No Blender needed — exercises the pure block-walking parser directly.
    fn synth_blend(blocks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"BLENDER-v403"); // 8-byte ptr ('-'), little-endian ('v')
        for (code, payload) in blocks {
            b.extend_from_slice(*code);
            b.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            b.extend_from_slice(&[0u8; 8]); // old_ptr (8 bytes)
            b.extend_from_slice(&0u32.to_le_bytes()); // sdna index
            b.extend_from_slice(&1u32.to_le_bytes()); // count
            b.extend_from_slice(payload);
        }
        b.extend_from_slice(b"ENDB");
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&[0u8; 8]);
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b
    }

    fn test_block(w: i32, h: i32, rgba: &[u8]) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&w.to_le_bytes());
        p.extend_from_slice(&h.to_le_bytes());
        p.extend_from_slice(rgba);
        p
    }

    #[test]
    fn extracts_embedded_preview() {
        // A 2×2 preview, preceded by an unrelated block to prove we walk past non-TEST blocks.
        let px = [
            10, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 100, 110, 120, 255,
        ];
        let bytes = synth_blend(&[(b"REND", vec![0u8; 72]), (b"TEST", test_block(2, 2, &px))]);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mini.blend");
        std::fs::write(&p, &bytes).unwrap();

        let png = embedded_thumbnail_png(&p).expect("should extract the TEST preview");
        let img = image::load_from_memory(&png).expect("valid PNG");
        assert_eq!((img.width(), img.height()), (2, 2));
    }

    #[test]
    fn no_preview_block_is_none() {
        // A .blend with no TEST block → None, so the caller falls through to geometry / typed tile.
        let bytes = synth_blend(&[(b"REND", vec![0u8; 72])]);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nopreview.blend");
        std::fs::write(&p, &bytes).unwrap();
        assert!(embedded_thumbnail_png(&p).is_none());
    }

    #[test]
    fn non_blend_bytes_are_none() {
        // A compressed/foreign file (no "BLENDER" magic) is rejected rather than mis-parsed.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("notablend.blend");
        std::fs::write(&p, b"\x28\xb5\x2f\xfd not really a blend").unwrap();
        assert!(embedded_thumbnail_png(&p).is_none());
    }
}
