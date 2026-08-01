//! CPU-only contract fixtures for browser/headless camera and PBR parity. GPU screenshots remain a
//! final-validation step because CI does not promise either a WebGPU adapter or WebGL2 browser.

#[path = "../../3dam-viewer/src/framing.rs"]
mod viewer_framing;

#[test]
fn framing_convention_v1_is_mirrored_by_wasm_and_dom_controls() {
    let convention = dam_render::framing_convention();
    assert_eq!(convention.version, 1);
    assert_eq!(convention.yaw, 0.732_815_1);
    assert_eq!(convention.pitch, 0.450_712_98);
    assert_eq!(convention.fov_y_degrees, 40.0);
    assert_eq!(convention.fit_margin, 1.12);

    let dom = include_str!("../../../web/src/lib/viewer-gestures.ts");
    for token in ["0.7328151", "0.45071298"] {
        assert!(dom.contains(token), "DOM reset pose lost `{token}`");
    }

    assert_eq!(viewer_framing::FRAMING_VERSION, convention.version);
    assert_eq!(viewer_framing::DEFAULT_YAW, convention.yaw);
    assert_eq!(viewer_framing::DEFAULT_PITCH, convention.pitch);
    assert_eq!(viewer_framing::FOV_Y_DEGREES, convention.fov_y_degrees);
    assert_eq!(viewer_framing::FIT_MARGIN, convention.fit_margin);

    let native_direction = dam_render::framing_direction();
    let viewer_direction = viewer_framing::canonical_direction();
    for axis in 0..3 {
        assert!((native_direction[axis] - viewer_direction[axis]).abs() < f32::EPSILON);
    }
    for (radius, aspect) in [(0.001, 0.5), (1.0, 1.0), (23.5, 16.0 / 9.0)] {
        let native = dam_render::framing_fit_distance(radius, aspect);
        let viewer = viewer_framing::fit_distance(radius, aspect, 1.0);
        assert!((native - viewer).abs() < f32::EPSILON);
        let distance = viewer_framing::fit_distance(radius, aspect, 0.35);
        let native_clip = dam_render::framing_clip_planes(radius, distance);
        let (near, far) = viewer_framing::clip_planes(radius, distance);
        assert!((native_clip.0 - near).abs() < f32::EPSILON);
        assert!((native_clip.1 - far).abs() < f32::EPSILON);
        assert!(near > 0.0 && far > near);
    }
}

#[test]
fn studio_pbr_contract_keeps_the_same_material_and_light_conventions() {
    let headless = include_str!("../src/shaders/pbr.wgsl");
    let browser = include_str!("../../3dam-viewer/src/shaders/pbr.wgsl");
    let required = [
        "3DAM_PBR_CONVENTION_V1",
        "let f0 = mix(vec3<f32>(0.04), albedo, metallic)",
        "vec3<f32>(0.5, 0.8, 0.6)",
        "vec3<f32>(-0.6, 0.3, 0.4)",
        "vec3<f32>(-0.2, 0.5, -0.9)",
        "let sky = vec3<f32>(0.40, 0.44, 0.52)",
        "let ground = vec3<f32>(0.11, 0.10, 0.10)",
        "lo / (lo + vec3<f32>(1.0))",
    ];
    for token in required {
        assert!(headless.contains(token), "headless PBR lost `{token}`");
        assert!(browser.contains(token), "browser PBR lost `{token}`");
    }
}

#[test]
fn visual_fixture_manifest_covers_both_browser_backends_and_material_modes() {
    let manifest = include_str!("fixtures/viewer-visual-cases.json");
    for token in [
        "webgpu",
        "webgl2",
        "textured_cube.gltf",
        "glass_cube.gltf",
        "multi_material_grid.obj",
        "framing",
        "base-color",
        "alpha-blend",
        "material-batching",
    ] {
        assert!(manifest.contains(token), "visual fixture manifest lost `{token}`");
    }
}
