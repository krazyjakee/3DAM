//! Turntable camera framing (tech-spec 06 §3.2).
//!
//! Fits the model's bounding sphere in view from a canonical 3/4 angle, so every thumbnail is
//! framed consistently regardless of the model's authored scale or origin.

use crate::model::Aabb;
use glam::{Mat4, Vec3};

/// Version of the camera/PBR convention shared with the browser viewer. Bump this together with
/// `RENDER_VERSION` when a framing constant changes; `tests/viewer_parity.rs` pins the WASM mirror.
pub const FRAMING_VERSION: u32 = 1;
pub const DEFAULT_YAW: f32 = 0.732_815_1;
pub const DEFAULT_PITCH: f32 = 0.450_712_98;
pub const FOV_Y_DEGREES: f32 = 40.0;
pub const FIT_MARGIN: f32 = 1.12;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FramingConvention {
    pub version: u32,
    pub yaw: f32,
    pub pitch: f32,
    pub fov_y_degrees: f32,
    pub fit_margin: f32,
}

pub const fn framing_convention() -> FramingConvention {
    FramingConvention {
        version: FRAMING_VERSION,
        yaw: DEFAULT_YAW,
        pitch: DEFAULT_PITCH,
        fov_y_degrees: FOV_Y_DEGREES,
        fit_margin: FIT_MARGIN,
    }
}

pub struct Camera {
    pub view_proj: Mat4,
    /// World-space eye position — the PBR shader needs it for the specular view vector.
    pub eye: Vec3,
}

/// Bounds-sphere distance for the canonical vertical field of view and target aspect ratio.
/// Keeping this separate makes the browser/headless parity fixture compare the actual convention,
/// rather than two coincidentally similar camera matrices.
pub fn fit_distance(radius: f32, aspect: f32) -> f32 {
    let fov_y = FOV_Y_DEGREES.to_radians();
    let fov_x = 2.0 * ((fov_y * 0.5).tan() * aspect.max(0.0001)).atan();
    let limiting = fov_y.min(fov_x);
    (radius / (limiting * 0.5).sin()) * FIT_MARGIN
}

pub fn canonical_direction() -> [f32; 3] {
    [
        DEFAULT_PITCH.cos() * DEFAULT_YAW.sin(),
        DEFAULT_PITCH.sin(),
        DEFAULT_PITCH.cos() * DEFAULT_YAW.cos(),
    ]
}

pub fn clip_planes(radius: f32, distance: f32) -> (f32, f32) {
    (
        (distance - radius).max(radius * 0.01),
        distance + radius * 2.0,
    )
}

/// Frame the bounds from a fixed 3/4 view for a target aspect ratio (width / height).
pub fn frame(bounds: &Aabb, aspect: f32) -> Camera {
    let center = bounds.center();
    let radius = bounds.radius();

    // Canonical viewing direction: upper-front-right, looking down slightly. Yaw/pitch are exposed
    // constants because the interactive viewer starts at this exact pose.
    let dir = Vec3::from_array(canonical_direction());
    let fov_y = FOV_Y_DEGREES.to_radians();

    // Distance that fits the bounding sphere, accounting for the narrower of the two FOVs when the
    // viewport is not square, plus a small margin so the model doesn't touch the edges.
    let dist = fit_distance(radius, aspect);

    let eye = center + dir * dist;
    let view = Mat4::look_at_rh(eye, center, Vec3::Y);

    let (near, far) = clip_planes(radius, dist);
    let proj = Mat4::perspective_rh(fov_y, aspect.max(0.0001), near, far);

    Camera {
        view_proj: proj * view,
        eye,
    }
}
