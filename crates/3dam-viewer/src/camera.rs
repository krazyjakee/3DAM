//! Orbit camera + bounds auto-fit — the browser copy of tech-spec 06 §5's *versioned, reproducible*
//! framing. Pure `glam` math (no GPU). `dam-render/tests/viewer_parity.rs` numerically pins this
//! WASM mirror to the headless renderer's versioned constants, direction, bounds fit and clip-plane
//! formula (ADR 0002 "same pose by construction").
//!
//! Framing is a pure function of the mesh bounds plus a small constant set: `fit_distance` derives
//! from the bounding sphere; user `zoom`/orbit apply *on top* without changing the stored default.
//! Framing convention v1 uses a 40° vertical FOV, yaw `0.7328151`, pitch `0.450713`, and an
//! aspect-aware bounding-sphere fit with a 1.12 margin — the exact headless thumbnail pose.

use glam::{Mat4, Vec2, Vec3};

use crate::framing::{
    clip_planes, fit_distance, orbit_direction, DEFAULT_PITCH, DEFAULT_YAW, FIT_MARGIN,
    FOV_Y_DEGREES,
};

/// Axis-aligned bounds of the loaded scene, in model space. Feeds the auto-fit distance.
#[derive(Clone, Copy)]
pub struct Aabb {
    pub min: Vec3,
    pub max: Vec3,
}

impl Aabb {
    /// An empty box that "grows" to the first point (min = +inf, max = -inf).
    pub fn empty() -> Self {
        Self {
            min: Vec3::splat(f32::INFINITY),
            max: Vec3::splat(f32::NEG_INFINITY),
        }
    }

    pub fn expand(&mut self, p: Vec3) {
        self.min = self.min.min(p);
        self.max = self.max.max(p);
    }

    /// A safe unit box if no geometry was seen (empty mesh) — keeps framing math finite.
    pub fn or_unit(self) -> Self {
        if self.min.cmple(self.max).all() && self.min.is_finite() && self.max.is_finite() {
            self
        } else {
            Self {
                min: Vec3::splat(-0.5),
                max: Vec3::splat(0.5),
            }
        }
    }

    pub fn center(&self) -> Vec3 {
        (self.min + self.max) * 0.5
    }

    /// Bounding-sphere radius (half the diagonal), floored so a degenerate/flat mesh still frames.
    pub fn radius(&self) -> f32 {
        ((self.max - self.min) * 0.5).length().max(1e-4)
    }
}

/// The versioned framing constants (tech-spec 06 §5). Kept as a struct so a future `FramingVersion`
/// bump (which would invalidate cached thumbnails/embeddings on the server) is a one-line change
/// here and the web viewer tracks it.
#[derive(Clone, Copy)]
pub struct Framing {
    pub yaw: f32,
    pub pitch: f32,
    pub fov_deg: f32,
    pub fit_margin: f32,
}

impl Default for Framing {
    fn default() -> Self {
        Self {
            yaw: DEFAULT_YAW,
            pitch: DEFAULT_PITCH,
            fov_deg: FOV_Y_DEGREES,
            fit_margin: FIT_MARGIN,
        }
    }
}

/// Live orbit state the DOM drives via `set_camera(yaw, pitch, zoom)`. `zoom` is a *separate*
/// multiplier on the bounds-derived fit distance (spec §5: user zoom never rewrites the default).
pub struct OrbitCamera {
    framing: Framing,
    pub yaw: f32,
    pub pitch: f32,
    pub zoom: f32,
    /// View-plane pan in viewport fractions. Kept bounds-relative, so interaction speed is stable
    /// for authored units ranging from millimetres to kilometres.
    pub pan: Vec2,
    bounds: Aabb,
}

impl OrbitCamera {
    pub fn new(bounds: Aabb) -> Self {
        let framing = Framing::default();
        Self {
            yaw: framing.yaw,
            pitch: framing.pitch,
            zoom: 1.0,
            pan: Vec2::ZERO,
            framing,
            bounds,
        }
    }

    /// Re-fit to freshly loaded geometry, resetting to the default pose.
    pub fn set_bounds(&mut self, bounds: Aabb) {
        self.bounds = bounds;
        self.yaw = self.framing.yaw;
        self.pitch = self.framing.pitch;
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;
    }

    /// Apply DOM-driven orbit/zoom. Pitch is clamped just shy of the poles to avoid a gimbal flip;
    /// zoom is clamped to a sane range so the model can't be lost behind the near plane or shrink to
    /// a dot.
    pub fn update(&mut self, yaw: f32, pitch: f32, zoom: f32) {
        self.update_pose(yaw, pitch, zoom, self.pan.x, self.pan.y);
    }

    pub fn update_pose(&mut self, yaw: f32, pitch: f32, zoom: f32, pan_x: f32, pan_y: f32) {
        const LIMIT: f32 = std::f32::consts::FRAC_PI_2 - 0.05;
        self.yaw = if yaw.is_finite() { yaw } else { self.framing.yaw };
        self.pitch = if pitch.is_finite() {
            pitch.clamp(-LIMIT, LIMIT)
        } else {
            self.framing.pitch
        };
        self.zoom = if zoom.is_finite() {
            // At 0.35 the closest square-aspect fit remains just outside the bounds sphere. A
            // smaller multiplier would put the eye inside the model and invert/clamp geometry.
            zoom.clamp(0.35, 10.0)
        } else {
            1.0
        };
        self.pan = Vec2::new(
            if pan_x.is_finite() { pan_x } else { 0.0 },
            if pan_y.is_finite() { pan_y } else { 0.0 },
        )
        .clamp(Vec2::splat(-2.0), Vec2::splat(2.0));
    }

    fn direction(&self) -> Vec3 {
        Vec3::from_array(orbit_direction(self.yaw, self.pitch))
    }

    fn target(&self) -> Vec3 {
        let dir = self.direction();
        let forward = -dir;
        let right = forward.cross(Vec3::Y).normalize_or_zero();
        let up = right.cross(forward).normalize_or_zero();
        let scale = self.bounds.radius() * self.zoom * 2.0;
        self.bounds.center() + right * (self.pan.x * scale) + up * (self.pan.y * scale)
    }

    fn fit_distance(&self, aspect: f32) -> f32 {
        debug_assert_eq!(self.framing.fov_deg, FOV_Y_DEGREES);
        debug_assert_eq!(self.framing.fit_margin, FIT_MARGIN);
        fit_distance(self.bounds.radius(), aspect, self.zoom)
    }

    fn eye(&self, aspect: f32) -> Vec3 {
        self.target() + self.direction() * self.fit_distance(aspect)
    }

    /// Camera world position — the shader uses it for the specular view direction.
    pub fn eye_pos(&self, aspect: f32) -> Vec3 {
        self.eye(aspect)
    }

    /// Combined view-projection for the given aspect ratio. `perspective_rh` gives the wgpu/DX
    /// `0..1` depth range (not GL's `-1..1`), matching our depth buffer config.
    pub fn view_proj(&self, aspect: f32) -> Mat4 {
        let center = self.target();
        let radius = self.bounds.radius();
        let eye = self.eye(aspect);
        let view = Mat4::look_at_rh(eye, center, Vec3::Y);
        let dist = self.fit_distance(aspect);
        let (near, far) = clip_planes(radius, dist);
        let proj = Mat4::perspective_rh(
            self.framing.fov_deg.to_radians(),
            aspect.max(1e-3),
            near,
            far,
        );
        proj * view
    }
}
