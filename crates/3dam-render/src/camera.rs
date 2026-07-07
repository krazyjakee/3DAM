//! Turntable camera framing (tech-spec 06 §3.2).
//!
//! Fits the model's bounding sphere in view from a canonical 3/4 angle, so every thumbnail is
//! framed consistently regardless of the model's authored scale or origin.

use crate::model::Aabb;
use glam::{Mat4, Vec3};

pub struct Camera {
    pub view_proj: Mat4,
    /// World-space eye position — the PBR shader needs it for the specular view vector.
    pub eye: Vec3,
}

/// Frame the bounds from a fixed 3/4 view for a target aspect ratio (width / height).
pub fn frame(bounds: &Aabb, aspect: f32) -> Camera {
    let center = bounds.center();
    let radius = bounds.radius();

    // Canonical viewing direction: upper-front-right, looking down slightly.
    let dir = Vec3::new(0.9, 0.65, 1.0).normalize();
    let fov_y = 40f32.to_radians();

    // Distance that fits the bounding sphere, accounting for the narrower of the two FOVs when the
    // viewport is not square, plus a small margin so the model doesn't touch the edges.
    let fov_x = 2.0 * ((fov_y * 0.5).tan() * aspect.max(0.0001)).atan();
    let limiting = fov_y.min(fov_x);
    let dist = (radius / (limiting * 0.5).sin()) * 1.12;

    let eye = center + dir * dist;
    let view = Mat4::look_at_rh(eye, center, Vec3::Y);

    let near = (dist - radius).max(radius * 0.01);
    let far = dist + radius * 2.0;
    let proj = Mat4::perspective_rh(fov_y, aspect.max(0.0001), near, far);

    Camera {
        view_proj: proj * view,
        eye,
    }
}
