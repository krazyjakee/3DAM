//! Headless render smoke test (Phase 1 gate). Renders the fixture cube through the full public API
//! and asserts a valid, non-blank PNG. Fail-soft: if no adapter is available at all (no GPU and no
//! software rasteriser), the test skips rather than failing — the environment, not the code, is at
//! fault, and the engine degrades to typed tiles in exactly that case.

use std::path::Path;

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn renders_cube_to_nonblank_png() {
    let path = fixture("cube.gltf");
    let size = 128;
    let png = match dam_render::render_model_thumbnail_png(&path, "gltf", size) {
        Ok(png) => png,
        Err(dam_render::RenderError::NoAdapter) => {
            eprintln!("skipping: no GPU or software adapter in this environment");
            return;
        }
        Err(e) => panic!("render failed: {e}"),
    };

    // Optional visual dump for local inspection: DAM_RENDER_DUMP=/path/out.png cargo test ...
    if let Ok(dump) = std::env::var("DAM_RENDER_DUMP") {
        std::fs::write(&dump, &png).expect("write dump");
        eprintln!("dumped render to {dump}");
    }

    // Decodes as a PNG of the requested size.
    let img = image::load_from_memory(&png).expect("valid PNG");
    assert_eq!(img.width(), size);
    assert_eq!(img.height(), size);

    // Not a blank frame: some pixels must differ from the clear colour (i.e. the cube drew). We
    // check that more than one distinct luminance bucket is present.
    let rgba = img.to_rgba8();
    let mut seen = std::collections::HashSet::new();
    for px in rgba.pixels() {
        seen.insert((px[0] / 16, px[1] / 16, px[2] / 16));
    }
    assert!(
        seen.len() > 1,
        "rendered frame is a single flat colour — the cube did not draw"
    );
}

#[test]
fn renders_textured_cube_with_color() {
    let path = fixture("textured_cube.gltf");
    let png = match dam_render::render_model_thumbnail_png(&path, "gltf", 192) {
        Ok(png) => png,
        Err(dam_render::RenderError::NoAdapter) => return,
        Err(e) => panic!("render failed: {e}"),
    };
    if let Ok(dump) = std::env::var("DAM_TEXTURED_DUMP") {
        std::fs::write(&dump, &png).expect("write dump");
        eprintln!("dumped textured render to {dump}");
    }

    let img = image::load_from_memory(&png).expect("valid PNG").to_rgba8();
    // The texture has red/green/blue/yellow quadrants. A grey clay render would have near-zero
    // saturation everywhere; assert we see genuinely saturated pixels of multiple hues.
    let mut saw_red = false;
    let mut saw_green = false;
    let mut saw_blue = false;
    for px in img.pixels() {
        let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
        if r - g > 60 && r - b > 60 {
            saw_red = true;
        }
        if g - r > 40 && g - b > 20 {
            saw_green = true;
        }
        if b - r > 60 && b - g > 40 {
            saw_blue = true;
        }
    }
    assert!(
        saw_red && saw_green && saw_blue,
        "textured render should show saturated red/green/blue from the base-colour map (got r={saw_red} g={saw_green} b={saw_blue}) — textures are not being applied"
    );
}

#[test]
fn renders_obj_with_external_texture() {
    // A non-glTF format (OBJ) whose material references a sibling PNG via MTL map_Kd — exercises
    // the Assimp import path plus external (on-disk) texture resolution, the same shape FBX uses.
    let path = fixture("obj_cube.obj");
    let png = match dam_render::render_model_thumbnail_png(&path, "obj", 192) {
        Ok(png) => png,
        Err(dam_render::RenderError::NoAdapter) => return,
        Err(e) => panic!("render failed: {e}"),
    };
    if let Ok(dump) = std::env::var("DAM_OBJ_DUMP") {
        std::fs::write(&dump, &png).expect("write dump");
    }
    let img = image::load_from_memory(&png).expect("valid PNG").to_rgba8();
    let mut saw_red = false;
    let mut saw_blue = false;
    for px in img.pixels() {
        let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
        if r - g > 60 && r - b > 60 {
            saw_red = true;
        }
        if b - r > 60 && b - g > 40 {
            saw_blue = true;
        }
    }
    assert!(
        saw_red && saw_blue,
        "OBJ with an external base-colour texture should render in colour (map_Kd not applied)"
    );
}

#[test]
fn renders_fbx_with_embedded_texture() {
    // FBX is the dominant game-studio interchange format. This fixture (authored in Blender) embeds
    // its base-colour texture in the .fbx, so it exercises Assimp's FBX importer + embedded-texture
    // extraction end to end.
    let path = fixture("textured_cube.fbx");
    let png = match dam_render::render_model_thumbnail_png(&path, "fbx", 192) {
        Ok(png) => png,
        Err(dam_render::RenderError::NoAdapter) => return,
        Err(e) => panic!("FBX render failed: {e}"),
    };
    if let Ok(dump) = std::env::var("DAM_FBX_DUMP") {
        std::fs::write(&dump, &png).expect("write dump");
    }
    let img = image::load_from_memory(&png).expect("valid PNG").to_rgba8();
    let mut saw_red = false;
    let mut saw_green = false;
    let mut saw_blue = false;
    for px in img.pixels() {
        let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
        if r - g > 60 && r - b > 60 {
            saw_red = true;
        }
        if g - r > 40 && g - b > 20 {
            saw_green = true;
        }
        if b - r > 60 && b - g > 40 {
            saw_blue = true;
        }
    }
    assert!(
        saw_red && saw_green && saw_blue,
        "FBX with an embedded base-colour texture should render in colour — the embedded texture is not being applied"
    );
}

/// Whether a `blender` binary is reachable (honouring the `DAM_BLENDER_BIN` override). The bridge
/// test skips — rather than fails — when it isn't, since CI hosts don't ship Blender.
fn blender_available() -> bool {
    let bin = std::env::var_os("DAM_BLENDER_BIN")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "blender".into());
    std::process::Command::new(bin)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn modern_blend_decodes_via_blender_bridge() {
    // Assimp's built-in Blender importer only reads legacy (≤2.7x) files; this fixture is a modern
    // (2.8+) `.blend` that Assimp alone fails on. The headless Blender bridge exports it to a temp
    // GLB, which then decodes normally — proving the previewer supports modern `.blend`.
    if !blender_available() {
        eprintln!("skipping: no `blender` binary on this host (set DAM_BLENDER_BIN to run)");
        return;
    }
    let blob = dam_render::model_preview_blob(&fixture("modern_cube.blend"), "blend")
        .expect("modern .blend should decode through the Blender bridge");
    let (_n_tex, _n_mat, n_sub) = parse_dmsh(&blob);
    assert!(n_sub >= 1, "expected geometry from the bridged .blend");
}

#[test]
fn unsupported_format_is_soft_error() {
    // USD has no Assimp importer — it's the one model family that degrades to the typed tile.
    let err = dam_render::render_model_thumbnail_png(Path::new("nope.usdz"), "usdz", 64)
        .expect_err("usd is not yet renderable");
    assert!(matches!(err, dam_render::RenderError::UnsupportedFormat(_)));
    assert!(!dam_render::supports_format("usdz"));
    assert!(!dam_render::supports_format("usd"));
    // The Assimp-backed formats are all supported.
    for f in [
        "gltf", "glb", "fbx", "obj", "stl", "ply", "dae", "3ds", "blend",
    ] {
        assert!(dam_render::supports_format(f), "{f} should be supported");
    }
}

/// Walk a `DMSH` blob's structure, asserting the layout is well-formed and the cursor consumes it
/// exactly. Returns `(n_tex, n_mat, n_sub)` for content assertions. Mirrors the WASM parser and
/// `dam-render`'s serializer (a `Vertex` is 16×f32 = 64 bytes; a material is 60 bytes).
fn parse_dmsh(blob: &[u8]) -> (u32, u32, u32) {
    const VERT: usize = 64;
    // base_color(16) + metallic(4) + roughness(4) + emissive(12) + 4 slots(16) + alpha_mode(4) + alpha_cutoff(4)
    const MAT: usize = 60;
    let u32at = |p: &mut usize| {
        let v = u32::from_le_bytes(blob[*p..*p + 4].try_into().unwrap());
        *p += 4;
        v
    };
    assert_eq!(&blob[0..4], b"DMSH", "bad magic");
    let mut p = 4usize; // past the magic
    assert_eq!(u32at(&mut p), 2, "unexpected DMSH version");
    p += 24; // bounds: 6 × f32

    let n_tex = u32at(&mut p);
    for _ in 0..n_tex {
        let len = u32at(&mut p) as usize;
        p += len; // PNG bytes
    }
    let n_mat = u32at(&mut p);
    p += n_mat as usize * MAT;
    let n_sub = u32at(&mut p);
    for _ in 0..n_sub {
        let _material = u32at(&mut p);
        let n_vert = u32at(&mut p) as usize;
        p += n_vert * VERT;
        let n_idx = u32at(&mut p) as usize;
        p += n_idx * 4;
    }
    assert_eq!(p, blob.len(), "cursor did not consume the whole blob");
    (n_tex, n_mat, n_sub)
}

#[test]
fn preview_blob_is_self_contained_and_textured() {
    // The interactive viewer's blob — CPU-only, so it runs with no GPU adapter (unlike the PNG path).
    let blob = dam_render::model_preview_blob(&fixture("textured_cube.gltf"), "gltf")
        .expect("decode textured cube to DMSH");
    let (n_tex, n_mat, n_sub) = parse_dmsh(&blob);
    assert!(n_sub >= 1, "expected geometry");
    assert!(n_mat >= 1, "expected a material");
    assert!(
        n_tex >= 1,
        "a textured fixture must carry its texture in the self-contained blob"
    );
}

#[test]
fn preview_blob_encodes_blend_transparency() {
    // A glTF material declaring `alphaMode:"BLEND"` with a base-colour alpha < 1 (the car-glass case)
    // must reach the blob as a blended material carrying its opacity — not silently forced opaque.
    let blob = dam_render::model_preview_blob(&fixture("glass_cube.gltf"), "gltf")
        .expect("decode glass cube to DMSH");

    let u32at = |p: usize| u32::from_le_bytes(blob[p..p + 4].try_into().unwrap());
    let f32at = |p: usize| f32::from_bits(u32at(p));

    // Walk to the first material record: magic(4) + version(4) + bounds(24), then the texture table.
    let mut p = 4 + 4 + 24;
    let n_tex = u32at(p);
    p += 4;
    for _ in 0..n_tex {
        let len = u32at(p) as usize;
        p += 4 + len;
    }
    let n_mat = u32at(p);
    p += 4;
    assert!(n_mat >= 1, "expected a material");

    // Material layout: base_color(16) metallic(4) roughness(4) emissive(12) 4 slots(16)
    // alpha_mode(4) alpha_cutoff(4).
    let base_alpha = f32at(p + 12);
    let alpha_mode = u32at(p + 52);
    assert_eq!(alpha_mode, 2, "alphaMode:BLEND must serialize as blend (2)");
    assert!(
        (base_alpha - 0.25).abs() < 1e-4,
        "base-colour alpha should survive to the blob, got {base_alpha}"
    );
}

#[test]
fn preview_blob_covers_non_gltf_formats() {
    // The whole point: FBX/OBJ (never previewable client-side before) now decode through the blob.
    for (file, fmt) in [("textured_cube.fbx", "fbx"), ("obj_cube.obj", "obj")] {
        let blob = dam_render::model_preview_blob(&fixture(file), fmt)
            .unwrap_or_else(|e| panic!("decode {file} to DMSH: {e}"));
        let (_n_tex, _n_mat, n_sub) = parse_dmsh(&blob);
        assert!(n_sub >= 1, "{file}: expected geometry in the blob");
    }
    // Unsupported formats fail-soft, same as the thumbnail path.
    assert!(matches!(
        dam_render::model_preview_blob(Path::new("nope.usdz"), "usdz"),
        Err(dam_render::RenderError::UnsupportedFormat(_))
    ));
}
