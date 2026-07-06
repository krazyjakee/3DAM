//! Orbit camera + bounds auto-fit — the browser copy of tech-spec 06 §5's *versioned, reproducible*
//! framing. Pure `glam` math (no GPU), so when `dam-render`/`dam-core` land this moves there
//! wholesale and the web viewer, the desktop viewer, and the headless thumbnailer share one framing
//! (ADR 0002 "same pose by construction").
//!
//! Framing is a pure function of the mesh bounds plus a small constant set: `fit_distance` derives
//! from the bounding sphere; user `zoom`/orbit apply *on top* without changing the stored default.
//! The default pose (`fit_mul = 2.8`, `yaw = π/4`, `pitch ≈ 0.5`, `fov = 45°`) is exactly MoGen's
//! `radius * 2.8` auto-fit (3d-handler-notes §2), so a thumbnail and the viewer's initial frame line
//! up.

use glam::{Mat4, Vec3};

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
        if self.min.x <= self.max.x {
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
        ((self.max - self.min) * 0.5).length().max(1e-3)
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
    pub fit_mul: f32,
}

impl Default for Framing {
    fn default() -> Self {
        Self {
            yaw: std::f32::consts::FRAC_PI_4,
            pitch: 0.5,
            fov_deg: 45.0,
            fit_mul: 2.8,
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
    bounds: Aabb,
}

impl OrbitCamera {
    pub fn new(bounds: Aabb) -> Self {
        let framing = Framing::default();
        Self {
            yaw: framing.yaw,
            pitch: framing.pitch,
            zoom: 1.0,
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
    }

    /// Apply DOM-driven orbit/zoom. Pitch is clamped just shy of the poles to avoid a gimbal flip;
    /// zoom is clamped to a sane range so the model can't be lost behind the near plane or shrink to
    /// a dot.
    pub fn update(&mut self, yaw: f32, pitch: f32, zoom: f32) {
        const LIMIT: f32 = std::f32::consts::FRAC_PI_2 - 0.05;
        self.yaw = yaw;
        self.pitch = pitch.clamp(-LIMIT, LIMIT);
        self.zoom = zoom.clamp(0.1, 10.0);
    }

    fn eye(&self) -> Vec3 {
        let center = self.bounds.center();
        let radius = self.bounds.radius();
        let dist = radius * self.framing.fit_mul * self.zoom;
        let dir = Vec3::new(
            self.pitch.cos() * self.yaw.sin(),
            self.pitch.sin(),
            self.pitch.cos() * self.yaw.cos(),
        );
        center + dir * dist
    }

    /// Camera world position — the shader uses it for the specular view direction.
    pub fn eye_pos(&self) -> Vec3 {
        self.eye()
    }

    /// Combined view-projection for the given aspect ratio. `perspective_rh` gives the wgpu/DX
    /// `0..1` depth range (not GL's `-1..1`), matching our depth buffer config.
    pub fn view_proj(&self, aspect: f32) -> Mat4 {
        let center = self.bounds.center();
        let radius = self.bounds.radius();
        let eye = self.eye();
        let view = Mat4::look_at_rh(eye, center, Vec3::Y);
        // Near/far bracket the model generously so orbit + zoom never clip it.
        let near = (radius * 0.01).max(1e-3);
        let far = radius * 100.0;
        let proj = Mat4::perspective_rh(
            self.framing.fov_deg.to_radians(),
            aspect.max(1e-3),
            near,
            far,
        );
        proj * view
    }
}
