use super::*;
fn put(out: &mut Vec<u8>, kind: &[u8; 4], bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(bytes);
    let mut crc = crc32fast::Hasher::new();
    crc.update(kind);
    crc.update(bytes);
    out.extend_from_slice(&crc.finalize().to_be_bytes());
}
fn fixture(width: u32, height: u32, depth: u8, color: u8) -> Vec<u8> {
    let samples = match color {
        0 | 3 => 1usize,
        2 => 3,
        4 => 2,
        6 => 4,
        _ => unreachable!(),
    };
    let stride = (width as usize * samples * usize::from(depth)).div_ceil(8);
    let mut filtered = Vec::new();
    for y in 0..height as usize {
        filtered.push(0);
        for x in 0..stride {
            filtered.push(((x * 13 + y * 7) % 256) as u8);
        }
    }
    let idat = miniz_oxide::deflate::compress_to_vec_zlib(&filtered, 0);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = width.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[depth, color, 0, 0, 0]);
    put(&mut out, b"IHDR", &ihdr);
    if color == 3 {
        let n = 1usize << depth;
        let palette: Vec<_> = (0..n)
            .flat_map(|i| [i as u8, (i * 7) as u8, (i * 13) as u8])
            .collect();
        put(&mut out, b"PLTE", &palette);
        let alpha: Vec<_> = (0..n).map(|i| (i * 255 / (n - 1)) as u8).collect();
        put(&mut out, b"tRNS", &alpha);
    }
    let mut dims = 3780u32.to_be_bytes().to_vec();
    dims.extend_from_slice(&3780u32.to_be_bytes());
    dims.push(1);
    put(&mut out, b"pHYs", &dims);
    put(&mut out, b"IDAT", &idat);
    put(&mut out, b"IEND", &[]);
    out
}
#[test]
fn exact_expanded_pixels_filters_and_metadata_for_all_supported_types() {
    for (width, height) in [(17, 19), (1, 257), (257, 1)] {
        for (depth, color) in [
            (1, 0),
            (2, 0),
            (4, 0),
            (8, 0),
            (1, 3),
            (2, 3),
            (4, 3),
            (8, 3),
            (8, 2),
            (8, 4),
            (8, 6),
        ] {
            let input = fixture(width, height, depth, color);
            let before = crate::decode_baseline(&input).unwrap();
            let trial = candidate(&input, 128 * 1024 * 1024).unwrap().unwrap();
            let after = crate::decode_baseline(&trial.data).unwrap();
            assert_eq!(before.pixels, after.pixels);
            assert_eq!(before.alpha, after.alpha);
            crate::png_rate::verify_current(&before, &trial.data, 128 * 1024 * 1024).unwrap();
            let old = parse(&input).unwrap().unwrap();
            let new = parse(&trial.data).unwrap().unwrap();
            assert_eq!(
                miniz_oxide::inflate::decompress_to_vec_zlib(&old.idat).unwrap(),
                miniz_oxide::inflate::decompress_to_vec_zlib(&new.idat).unwrap()
            );
            let a: Vec<_> = old
                .chunks
                .iter()
                .filter(|c| &c.0 != b"IDAT")
                .map(|c| &input[c.1.clone()])
                .collect();
            let b: Vec<_> = new
                .chunks
                .iter()
                .filter(|c| &c.0 != b"IDAT")
                .map(|c| &trial.data[c.1.clone()])
                .collect();
            assert_eq!(a, b);
        }
    }
}
#[test]
fn fixed_backend_context_fits_allowance_and_null_allocation_is_rejected() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static REQUESTED: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn reject(size: usize) -> *mut std::ffi::c_void {
        REQUESTED.store(size, Ordering::SeqCst);
        std::ptr::null_mut()
    }
    unsafe extern "C" fn free_unused(_: *mut std::ffi::c_void) {}
    let options = libdeflate_sys::libdeflate_options {
        sizeof_options: std::mem::size_of::<libdeflate_sys::libdeflate_options>(),
        malloc_func: Some(reject),
        free_func: Some(free_unused),
    };
    // SAFETY: options are valid for this synchronous call; the callback always
    // returns NULL, so no owned C object or allocation escapes the function.
    let context = unsafe { libdeflate_sys::libdeflate_alloc_compressor_ex(12, &options) };
    assert!(context.is_null());
    let requested = REQUESTED.load(Ordering::SeqCst);
    assert!(requested > 0 && requested as u64 <= BACKEND_ALLOWANCE);
    println!("fixed level12 context allocation request: {requested} bytes");
}

#[test]
fn stage_appends_one_strict_lossless_winner_and_is_idempotent() {
    let input = fixture(79, 83, 8, 6);
    let reference = crate::decode_baseline(&input).unwrap();
    let analysis = Analysis::for_reference(&reference, 128 * 1024 * 1024).unwrap();
    let old = quality::original(&reference, &analysis, &input).unwrap();
    let mut candidates = vec![old];
    refine(
        &input,
        &reference,
        &analysis,
        &mut candidates,
        Strength::default(),
    )
    .unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].data, input);
    let best = quality::best(candidates.iter(), Strength::default()).unwrap();
    assert!(best.bytes < input.len());
    assert_eq!(best.score.loss, 0.0);
    assert!(matches!(
        best.parameters,
        Parameters::PngDeflatePacked {
            policy: "libdeflate12_v1",
            ..
        }
    ));
    refine(
        &input,
        &reference,
        &analysis,
        &mut candidates,
        Strength::default(),
    )
    .unwrap();
    assert_eq!(candidates.len(), 2);
}
#[test]
fn zero_strength_low_budget_and_jpeg_leave_whole_control_unchanged() {
    let input = fixture(79, 83, 8, 6);
    let reference = crate::decode_baseline(&input).unwrap();
    let mut analysis = Analysis::for_reference(&reference, 128 * 1024 * 1024).unwrap();
    let mut candidates = vec![quality::original(&reference, &analysis, &input).unwrap()];
    refine(
        &input,
        &reference,
        &analysis,
        &mut candidates,
        Strength::new(0).unwrap(),
    )
    .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].data, input);
    analysis.memory_budget_bytes = 1;
    refine(
        &input,
        &reference,
        &analysis,
        &mut candidates,
        Strength::default(),
    )
    .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].data, input);
    assert!(candidate(&input, 1).unwrap().is_none());
    let mut opaque = reference;
    opaque.alpha = None;
    let jpeg = crate::encode_jpeg_baseline(&opaque, 90).unwrap();
    let reference = crate::decode_baseline(&jpeg).unwrap();
    let analysis = Analysis::for_reference(&reference, 128 * 1024 * 1024).unwrap();
    let mut candidates = vec![quality::original(&reference, &analysis, &jpeg).unwrap()];
    refine(
        &jpeg,
        &reference,
        &analysis,
        &mut candidates,
        Strength::default(),
    )
    .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].data, jpeg);
}
#[test]
fn admission_uses_capacities_and_checks_exact_threshold_and_overflow() {
    let peak = estimate(1000, 200, 300, 400).unwrap();
    assert!(admitted(Some(peak), peak));
    assert!(!admitted(Some(peak), peak - 1));
    assert!(!admitted(None, u64::MAX));
    assert_eq!(estimate(1000, 200, 300, 401), Some(peak + 1));
    assert!(estimate(u64::MAX, 0, 0, 0).is_none());
    assert!(estimate(1, 1, u64::MAX, 0).is_none());
    assert!(estimate(1, 1, 0, u64::MAX).is_none());
    let input = fixture(79, 83, 8, 2);
    let reference = crate::decode_baseline(&input).unwrap();
    let mut analysis = Analysis::for_reference(&reference, 128 * 1024 * 1024).unwrap();
    let mut candidates = vec![quality::original(&reference, &analysis, &input).unwrap()];
    let exact = estimate(
        79 * 83,
        input.len() as u64,
        input.len() as u64,
        candidates[0].data.capacity() as u64,
    )
    .unwrap();
    analysis.memory_budget_bytes = exact - 1;
    refine(
        &input,
        &reference,
        &analysis,
        &mut candidates,
        Strength::default(),
    )
    .unwrap();
    assert_eq!(candidates.len(), 1);
    analysis.memory_budget_bytes = exact;
    refine(
        &input,
        &reference,
        &analysis,
        &mut candidates,
        Strength::default(),
    )
    .unwrap();
    assert_eq!(candidates.len(), 2);
}
#[test]
fn corrupt_tail_crc_and_unsupported_chunks_are_not_silently_rewritten() {
    let input = fixture(17, 19, 8, 6);
    assert!(candidate(&input[..input.len() - 1], 128 * 1024 * 1024).is_err());
    let mut corrupt = input.clone();
    corrupt[40] ^= 1;
    assert!(candidate(&corrupt, 128 * 1024 * 1024).is_err());
    let mut unknown = input[..input.len() - 12].to_vec();
    put(&mut unknown, b"vpAg", &[1, 2, 3]);
    put(&mut unknown, b"IEND", &[]);
    assert!(candidate(&unknown, 128 * 1024 * 1024).unwrap().is_none());
    let mut apng = input[..33].to_vec();
    put(&mut apng, b"acTL", &[0, 0, 0, 1, 0, 0, 0, 0]);
    apng.extend_from_slice(&input[33..]);
    assert!(candidate(&apng, 128 * 1024 * 1024).unwrap().is_none());
}
