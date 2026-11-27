//! Approximate encoded-pixel geometry for the optional coefficient model.
//! Fixed heuristic Y/Cb/Cr weights100/143/99 are NOT metric-derived constants.
//! The continuous metric diagonal is nearer100/116/99; this frozen model is heuristic.
//! Sampling multiplies affected pixel area, not an exact perceptual, edge or tile model.
pub(crate) fn lambda(base: u32, index: usize, count: usize, h_scale: usize, v_scale: usize) -> u32 {
    if base == 0 {
        return 0;
    }
    let weight = if count == 3 {
        match index {
            1 => 143u64,
            2 => 99,
            _ => 100,
        }
    } else {
        100
    };
    let divisor = weight * h_scale as u64 * v_scale as u64;
    ((u64::from(base) * 100 + divisor / 2) / divisor).clamp(1, u64::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests {
    use super::lambda;
    #[test]
    fn sampling_area_and_color_weights_are_global_not_image_dependent() {
        assert_eq!(lambda(256, 0, 3, 1, 1), 256);
        for (h, v, cb, cr) in [(1, 1, 179, 259), (2, 1, 90, 129), (2, 2, 45, 65)] {
            assert_eq!(lambda(256, 1, 3, h, v), cb);
            assert_eq!(lambda(256, 2, 3, h, v), cr);
        }
        for base in [0, 1, 16, 256, 1024, u32::MAX] {
            for index in 0..3 {
                assert_eq!(lambda(base, index, 3, 4, 4) == 0, base == 0);
            }
            assert_eq!(lambda(base, 0, 1, 1, 1), base);
            assert_eq!(lambda(base, 2, 4, 1, 1), base);
        }
    }
}
