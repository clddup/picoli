use super::*;

#[test]
fn nonnegative_error_pruning_preserves_every_coefficient_cost_bit_and_tie() {
    for seed in 0..256u32 {
        let mut source = [0i16; 64];
        let mut steps = [8u16; 64];
        let mut baseline = [seed as i16; 64];
        for i in 0..64 {
            let x = (seed + i as u32)
                .wrapping_mul(747796405)
                .wrapping_add(2891336453);
            source[i] = (x >> 16) as i16;
            steps[i] = (1 + (x >> 8) % 255) as u16 * 8;
            let magnitude = ((i32::from(source[i]).unsigned_abs() + u32::from(steps[i]) / 2)
                / u32::from(steps[i]))
            .min(1023) as i16;
            baseline[i] = if source[i] < 0 { -magnitude } else { magnitude };
        }
        let table = core::array::from_fn(|symbol| {
            let code = ((seed + symbol as u32 * 13) % 16 + 1) as u8;
            if symbol == 0
                || symbol == 0xf0
                || ((1..=10).contains(&(symbol & 15)) && !(seed + symbol as u32).is_multiple_of(7))
            {
                code
            } else {
                0
            }
        });
        let start = if seed % 4 == 0 {
            1
        } else {
            1 + seed as usize % 63
        };
        let end = if seed % 4 == 0 {
            64
        } else {
            start + 1 + (seed as usize * 7) % (64 - start)
        };
        for lambda in [0, 1, 16, 64, 256, 1024, u32::MAX] {
            assert_eq!(
                solve::<true>(&source, &steps, &baseline, start, end, &table, lambda),
                solve::<false>(&source, &steps, &baseline, start, end, &table, lambda),
                "seed={seed}, band={start}..{end}, lambda={lambda}"
            );
        }
    }
}
