//! Pure render-target quality negotiation, kept native-testable even though the GPU context is WASM.

pub fn common_sample_count(
    color_supports_4: bool,
    color_supports_2: bool,
    depth_supports_4: bool,
    depth_supports_2: bool,
) -> u32 {
    if color_supports_4 && depth_supports_4 {
        4
    } else if color_supports_2 && depth_supports_2 {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multisampling_uses_the_highest_count_shared_by_color_and_depth() {
        assert_eq!(common_sample_count(true, true, true, true), 4);
        assert_eq!(common_sample_count(true, true, false, true), 2);
        assert_eq!(common_sample_count(false, true, true, true), 2);
        assert_eq!(common_sample_count(true, false, false, true), 1);
        assert_eq!(common_sample_count(false, false, false, false), 1);
    }
}
