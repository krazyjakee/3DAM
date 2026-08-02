//! Pure, target-neutral framing convention used by the WASM camera and native parity fixtures.

pub const FRAMING_VERSION: u32 = 1;
pub const DEFAULT_YAW: f32 = 0.732_815_1;
pub const DEFAULT_PITCH: f32 = 0.450_712_98;
pub const FOV_Y_DEGREES: f32 = 40.0;
pub const FIT_MARGIN: f32 = 1.12;

pub fn orbit_direction(yaw: f32, pitch: f32) -> [f32; 3] {
    [
        pitch.cos() * yaw.sin(),
        pitch.sin(),
        pitch.cos() * yaw.cos(),
    ]
}

#[cfg(test)]
pub fn canonical_direction() -> [f32; 3] {
    orbit_direction(DEFAULT_YAW, DEFAULT_PITCH)
}

pub fn fit_distance(radius: f32, aspect: f32, zoom: f32) -> f32 {
    let fov_y = FOV_Y_DEGREES.to_radians();
    let fov_x = 2.0 * ((fov_y * 0.5).tan() * aspect.max(0.0001)).atan();
    let limiting = fov_y.min(fov_x);
    (radius / (limiting * 0.5).sin()) * FIT_MARGIN * zoom
}

pub fn clip_planes(radius: f32, distance: f32) -> (f32, f32) {
    (
        (distance - radius).max(radius * 0.01),
        distance + radius * 2.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_view_uses_the_default_orbit() {
        assert_eq!(
            canonical_direction(),
            orbit_direction(DEFAULT_YAW, DEFAULT_PITCH)
        );
    }
}
