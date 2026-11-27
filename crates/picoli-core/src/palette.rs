//! Content-weighted palette training and spatially coherent pixel assignment.
use crate::{
    Error, Reference,
    analysis::{Analysis, color, distance},
};
use image::RgbImage;

#[derive(Clone)]
struct Bin {
    rgb: [u8; 3],
    value: [i32; 3],
    weight: u64,
}

fn key(rgb: [u8; 3]) -> usize {
    ((usize::from(rgb[0]) >> 3) << 10)
        | ((usize::from(rgb[1]) >> 3) << 5)
        | (usize::from(rgb[2]) >> 3)
}

fn histogram(reference: &Reference, analysis: &Analysis) -> Vec<Bin> {
    let mut buckets = vec![[0u64; 4]; 32768];
    for (i, (pixel, &weight)) in reference.pixels.pixels().zip(&analysis.weights).enumerate() {
        let weight = u64::from(weight)
            * reference
                .alpha
                .as_ref()
                .map_or(1, |a| u64::from(a.as_raw()[i]));
        let bucket = &mut buckets[key(pixel.0)];
        bucket[3] += weight;
        for c in 0..3 {
            bucket[c] += u64::from(pixel[c]) * weight;
        }
    }
    buckets
        .into_iter()
        .filter(|b| b[3] != 0)
        .map(|b| {
            let rgb = std::array::from_fn(|c| ((b[c] + b[3] / 2) / b[3]) as u8);
            Bin {
                rgb,
                value: color(rgb),
                weight: b[3],
            }
        })
        .collect()
}

pub(crate) fn train(reference: &Reference, analysis: &Analysis, count: usize) -> Vec<[u8; 3]> {
    let bins = histogram(reference, analysis);
    if bins.is_empty() {
        return vec![[0, 0, 0]];
    }
    // Deterministic farthest-point seeds with tempered population weighting.
    // Pure population medians can spend all small-palette slots on neutrals and
    // merge rare saturated colors (e.g. a red stamp) into the background.
    let first = bins
        .iter()
        .enumerate()
        .max_by_key(|&(i, bin)| (bin.weight, std::cmp::Reverse(i)))
        .map_or(0, |(i, _)| i);
    let mut palette = vec![bins[first].rgb];
    let mut nearest = vec![u64::MAX; bins.len()];
    while palette.len() < count.min(bins.len()) {
        let last = color(palette[palette.len() - 1]);
        for (bin, error) in bins.iter().zip(&mut nearest) {
            *error = (*error).min(distance(bin.value, last));
        }
        let next = bins
            .iter()
            .enumerate()
            .max_by_key(|&(i, bin)| (nearest[i] * bin.weight.isqrt().max(1), std::cmp::Reverse(i)))
            .map_or(0, |(i, _)| i);
        if nearest[next] == 0 {
            break;
        }
        palette.push(bins[next].rgb);
    }
    // Refine on the weighted histogram, not on every pixel for every iteration.
    for _ in 0..5 {
        let values: Vec<_> = palette.iter().copied().map(color).collect();
        let mut sums = vec![[0u64; 4]; palette.len()];
        for bin in &bins {
            let chosen = values
                .iter()
                .enumerate()
                .min_by_key(|&(i, &v)| (distance(v, bin.value), i))
                .map_or(0, |(i, _)| i);
            let sum = &mut sums[chosen];
            for (c, channel) in sum[..3].iter_mut().enumerate() {
                *channel += u64::from(bin.rgb[c]) * bin.weight;
            }
            sum[3] += bin.weight;
        }
        let mut changed = false;
        for (center, sum) in palette.iter_mut().zip(sums) {
            if sum[3] > 0 {
                let new = std::array::from_fn(|c| ((sum[c] + sum[3] / 2) / sum[3]) as u8);
                changed |= *center != new;
                *center = new;
            }
        }
        if !changed {
            break;
        }
    }
    palette.sort_unstable();
    palette.dedup();
    palette
}

/// Bounded per-cell candidate masks, reused for every coherence trial.
/// Exact mode retains every color that can be nearest anywhere in the cell.
/// These integer interval bounds are independent of upstream VP-tree code.
pub(crate) struct Mapper<'a> {
    palette: &'a [[u8; 3]],
    values: Vec<[i32; 3]>,
    masks: Vec<[u64; 4]>,
    // Direct-mapped, bounded cache of exact RGB nearest colors. Transparent
    // inputs keep the original all-candidate path to preserve alpha-rounding ties.
    nearest_cache: Vec<(u32, u8)>,
}

fn cache_has_reuse(reference: &Reference) -> bool {
    let count = reference.pixels.width() as usize * reference.pixels.height() as usize;
    let stride = count.div_ceil(4096).max(1);
    let mut keys = vec![u32::MAX; 65536];
    let (mut hits, mut samples) = (0usize, 0usize);
    for p in reference.pixels.pixels().step_by(stride) {
        let key = u32::from(p[0]) << 16 | u32::from(p[1]) << 8 | u32::from(p[2]);
        let slot = (key.wrapping_mul(0x9e3779b1) >> 16) as usize;
        hits += usize::from(keys[slot] == key);
        samples += 1;
        keys[slot] = key;
    }
    samples > 0 && hits * 4 >= samples
}

impl<'a> Mapper<'a> {
    pub(crate) fn new(reference: &Reference, palette: &'a [[u8; 3]], exact: bool) -> Self {
        let values: Vec<_> = palette.iter().copied().map(color).collect();
        let mut masks = vec![[0u64; 4]; 32768];
        let mut seen = vec![false; 32768];
        for pixel in reference.pixels.pixels() {
            let k = key(pixel.0);
            if seen[k] {
                continue;
            }
            seen[k] = true;
            let low = pixel.0.map(|c| i32::from(c & !7));
            if exact {
                // An RGB cube maps into this enclosing [2Y, Co, Cg] box.
                // A color whose minimum distance exceeds another color's
                // maximum distance cannot win for any pixel in the cube.
                let [r, g, b] = low;
                let minimum = [
                    2 * ((77 * r + 150 * g + 29 * b) >> 8),
                    r - b - 7,
                    g - ((r + b + 14) >> 1),
                ];
                let maximum = [minimum[0] + 14, r - b + 7, g + 7 - ((r + b) >> 1)];
                let mut bounds = [(0u64, 0u64); 256];
                let mut upper = u64::MAX;
                for (i, &[y, co, cg]) in values.iter().enumerate() {
                    let mut near = 0u64;
                    let mut far = 0u64;
                    for (c, lo, hi) in [2 * y, co, cg]
                        .into_iter()
                        .zip(minimum)
                        .zip(maximum)
                        .map(|((c, lo), hi)| (c, lo, hi))
                    {
                        let d = if c < lo {
                            lo - c
                        } else if c > hi {
                            c - hi
                        } else {
                            0
                        };
                        near += (i64::from(d) * i64::from(d)) as u64;
                        let d = (c - lo).abs().max((c - hi).abs());
                        far += (i64::from(d) * i64::from(d)) as u64;
                    }
                    bounds[i] = (near, far);
                    upper = upper.min(far);
                }
                for (i, &(lower, _)) in bounds[..values.len()].iter().enumerate() {
                    if lower <= upper {
                        masks[k][i / 64] |= 1 << (i % 64);
                    }
                }
            } else {
                let representative = color(low.map(|c| (c + 4) as u8));
                let mut best = [(u64::MAX, 0usize); 2];
                for (i, &v) in values.iter().enumerate() {
                    let entry = (distance(v, representative), i);
                    if entry < best[0] {
                        best[1] = best[0];
                        best[0] = entry;
                    } else if entry < best[1] {
                        best[1] = entry;
                    }
                }
                for (_, index) in best {
                    masks[k][index / 64] |= 1 << (index % 64);
                }
            }
        }
        Self {
            palette,
            values,
            masks,
            nearest_cache: if exact && reference.alpha.is_none() && cache_has_reuse(reference) {
                vec![(u32::MAX, 0); 65536]
            } else {
                Vec::new()
            },
        }
    }

    fn nearest(&mut self, rgb: [u8; 3], value: [i32; 3], mask: [u64; 4]) -> u8 {
        let key = u32::from(rgb[0]) << 16 | u32::from(rgb[1]) << 8 | u32::from(rgb[2]);
        let slot = (key.wrapping_mul(0x9e3779b1) >> 16) as usize;
        let cached = self.nearest_cache[slot];
        if cached.0 == key {
            return cached.1;
        }
        let mut best = (u64::MAX, 0u8);
        for (word, mut bits) in mask.into_iter().enumerate() {
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let index = (word * 64 + bit) as u8;
                best = best.min((distance(self.values[usize::from(index)], value), index));
            }
        }
        self.nearest_cache[slot] = (key, best.1);
        best.1
    }

    pub(crate) fn map(
        &mut self,
        reference: &Reference,
        analysis: &Analysis,
        coherence: u32,
    ) -> (Vec<u8>, RgbImage) {
        self.map_impl(reference, analysis, coherence, |_, _| true)
            .expect("unbounded mapping cannot reject a pixel")
    }

    pub(crate) fn map_scored(
        &mut self,
        reference: &Reference,
        analysis: &Analysis,
        coherence: u32,
        context: &crate::analysis::opaque::OpaqueContext<'_>,
    ) -> Option<(Vec<u8>, RgbImage, crate::analysis::Score)> {
        debug_assert!(reference.alpha.is_none());
        let mut scorer = context.start();
        let (indices, rgb) = self.map_impl(reference, analysis, coherence, |i, value| {
            scorer.push(i, value)
        })?;
        Some((indices, rgb, scorer.finish()))
    }

    fn map_impl(
        &mut self,
        reference: &Reference,
        analysis: &Analysis,
        coherence: u32,
        mut accept: impl FnMut(usize, [i32; 3]) -> bool,
    ) -> Option<(Vec<u8>, RgbImage)> {
        let width = reference.pixels.width() as usize;
        let mut indices = Vec::with_capacity(analysis.colors.len());
        let mut rgb = RgbImage::new(reference.pixels.width(), reference.pixels.height());
        for (i, pixel) in reference.pixels.pixels().enumerate() {
            let left = if i % width > 0 { indices[i - 1] } else { 0 };
            let up = if i >= width { indices[i - width] } else { 0 };
            let weight = u64::from(analysis.weights[i]);
            let mut mask = self.masks[key(pixel.0)];
            if !self.nearest_cache.is_empty() {
                let index = self.nearest(pixel.0, analysis.colors.at(i), mask);
                mask = [0; 4];
                mask[usize::from(index) / 64] = 1 << (index % 64);
            }
            if i % width > 0 {
                mask[usize::from(left) / 64] |= 1 << (left % 64);
            }
            if i >= width {
                mask[usize::from(up) / 64] |= 1 << (up % 64);
            }
            let mut best = (u64::MAX, 0u8);
            for (word, mut bits) in mask.into_iter().enumerate() {
                while bits != 0 {
                    let bit = bits.trailing_zeros() as usize;
                    let index = (word * 64 + bit) as u8;
                    bits &= bits - 1;
                    let mut error =
                        distance(self.values[usize::from(index)], analysis.colors.at(i)) * weight;
                    if let Some(alpha) = &reference.alpha {
                        let a = u64::from(alpha.as_raw()[i]);
                        error = error * a * a / (255 * 255);
                    }
                    let transitions = u64::from(i % width > 0 && index != left)
                        + u64::from(i >= width && index != up);
                    best = best.min((error + u64::from(coherence) * transitions, index));
                }
            }
            let chosen = best.1;
            indices.push(chosen);
            let mapped = if reference.alpha.as_ref().is_some_and(|a| a.as_raw()[i] == 0) {
                *pixel // Preserve invisible RGB, independent of the mapped index.
            } else {
                image::Rgb(self.palette[usize::from(chosen)])
            };
            rgb.put_pixel((i % width) as u32, (i / width) as u32, mapped);
            if !accept(i, self.values[usize::from(chosen)]) {
                return None;
            }
        }
        Some((indices, rgb))
    }
}

pub(crate) fn map(
    reference: &Reference,
    analysis: &Analysis,
    palette: &[[u8; 3]],
    coherence: u32,
) -> (Vec<u8>, RgbImage) {
    Mapper::new(reference, palette, false).map(reference, analysis, coherence)
}

pub(crate) fn encode(
    indices: &[u8],
    palette: &[[u8; 3]],
    width: u32,
    height: u32,
) -> Result<(Vec<u8>, &'static str), Error> {
    encode_indexed(indices, palette, &[], width, height)
}

pub(crate) fn encode_for_reference(
    reference: &Reference,
    indices: &[u8],
    palette: &[[u8; 3]],
    mapped: &RgbImage,
) -> Result<(Vec<u8>, &'static str), Error> {
    let Some(alpha) = &reference.alpha else {
        return encode(indices, palette, mapped.width(), mapped.height());
    };
    let rgba = mapped_rgba(reference, mapped, alpha)?;
    let mut best = encode_data(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
        png::BitDepth::Eight,
        png::ColorType::Rgba,
        None,
    )?;
    let mut lookup = std::collections::BTreeMap::new();
    let mut colors = Vec::new();
    let mut transparencies = Vec::new();
    let mut remapped = Vec::with_capacity(indices.len());
    for p in rgba.pixels() {
        let index = if let Some(&index) = lookup.get(&p.0) {
            index
        } else {
            if colors.len() == 256 {
                return Ok(best);
            } // RGBA fallback, never coarsen alpha.
            let index = colors.len() as u8;
            lookup.insert(p.0, index);
            colors.push([p[0], p[1], p[2]]);
            transparencies.push(p[3]);
            index
        };
        remapped.push(index);
    }
    while transparencies.last() == Some(&255) {
        transparencies.pop();
    }
    let indexed = encode_indexed(
        &remapped,
        &colors,
        &transparencies,
        rgba.width(),
        rgba.height(),
    )?;
    if indexed.0.len() < best.0.len() {
        best = indexed;
    }
    Ok(best)
}

fn mapped_rgba(
    reference: &Reference,
    mapped: &RgbImage,
    alpha: &image::GrayImage,
) -> Result<image::RgbaImage, Error> {
    if alpha.dimensions() != mapped.dimensions()
        || mapped.dimensions() != reference.pixels.dimensions()
    {
        return Err(Error::Argument("alpha dimensions differ from mapped RGB"));
    }
    Ok(image::RgbaImage::from_fn(
        mapped.width(),
        mapped.height(),
        |x, y| {
            let a = alpha.get_pixel(x, y)[0];
            let rgb = if a == 0 {
                reference.pixels.get_pixel(x, y)
            } else {
                mapped.get_pixel(x, y)
            };
            image::Rgba([rgb[0], rgb[1], rgb[2], a])
        },
    ))
}

pub(crate) fn encode_truecolor(
    reference: &Reference,
    mapped: &RgbImage,
) -> Result<(Vec<u8>, &'static str), Error> {
    if mapped.dimensions() != reference.pixels.dimensions() {
        return Err(Error::Argument("mapped dimensions differ from reference"));
    }
    if let Some(alpha) = &reference.alpha {
        let rgba = mapped_rgba(reference, mapped, alpha)?;
        encode_data(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            png::BitDepth::Eight,
            png::ColorType::Rgba,
            None,
        )
    } else {
        encode_data(
            mapped.as_raw(),
            mapped.width(),
            mapped.height(),
            png::BitDepth::Eight,
            png::ColorType::Rgb,
            None,
        )
    }
}

pub(crate) fn encode_indexed(
    indices: &[u8],
    palette: &[[u8; 3]],
    transparencies: &[u8],
    width: u32,
    height: u32,
) -> Result<(Vec<u8>, &'static str), Error> {
    let (bits, depth) = match palette.len() {
        1..=2 => (1, png::BitDepth::One),
        3..=4 => (2, png::BitDepth::Two),
        5..=16 => (4, png::BitDepth::Four),
        17..=256 => (8, png::BitDepth::Eight),
        _ => return Err(Error::Argument("palette must have 1..=256 entries")),
    };
    let stride = (width as usize * bits).div_ceil(8);
    let mut packed = vec![0u8; stride * height as usize];
    for (i, &index) in indices.iter().enumerate() {
        let (x, y) = (i % width as usize, i / width as usize);
        packed[y * stride + x * bits / 8] |= index << (8 - bits - (x * bits) % 8);
    }
    let colors: Vec<u8> = palette.iter().flatten().copied().collect();
    encode_data(
        &packed,
        width,
        height,
        depth,
        png::ColorType::Indexed,
        Some((&colors, transparencies)),
    )
}

/// Exact representation changes only; do not train a coarse histogram here.
pub(crate) fn encode_exact(reference: &Reference) -> Result<Option<Vec<u8>>, Error> {
    let (width, height) = reference.pixels.dimensions();
    if reference
        .alpha
        .as_ref()
        .is_some_and(|a| a.dimensions() != (width, height))
    {
        return Err(Error::Argument("alpha dimensions differ from RGB"));
    }
    let mut best = None;
    if reference
        .pixels
        .pixels()
        .all(|p| p[0] == p[1] && p[1] == p[2])
    {
        let mut data = Vec::with_capacity(
            reference.pixels.len() / 3 * if reference.alpha.is_some() { 2 } else { 1 },
        );
        for (i, p) in reference.pixels.pixels().enumerate() {
            data.push(p[0]);
            if let Some(alpha) = &reference.alpha {
                data.push(alpha.as_raw()[i]);
            }
        }
        let color_type = if reference.alpha.is_some() {
            png::ColorType::GrayscaleAlpha
        } else {
            png::ColorType::Grayscale
        };
        best = Some(encode_data(&data, width, height, png::BitDepth::Eight, color_type, None)?.0);
    }
    let mut lookup = std::collections::BTreeMap::new();
    let mut colors = Vec::new();
    let mut transparencies = Vec::new();
    let mut indices = Vec::new();
    for (i, p) in reference.pixels.pixels().enumerate() {
        let a = reference
            .alpha
            .as_ref()
            .map_or(255, |alpha| alpha.as_raw()[i]);
        let key = [p[0], p[1], p[2], a];
        let index = if let Some(&index) = lookup.get(&key) {
            index
        } else {
            if colors.len() == 256 {
                return Ok(best);
            }
            let index = colors.len() as u8;
            lookup.insert(key, index);
            colors.push(p.0);
            transparencies.push(a);
            index
        };
        indices.push(index);
    }
    while transparencies.last() == Some(&255) {
        transparencies.pop();
    }
    let indexed = encode_indexed(&indices, &colors, &transparencies, width, height)?.0;
    if best
        .as_ref()
        .is_none_or(|bytes| indexed.len() < bytes.len())
    {
        best = Some(indexed);
    }
    Ok(best)
}

type IndexedData<'a> = Option<(&'a [u8], &'a [u8])>;
const FILTERS: [(png::Filter, &str); 5] = [
    (png::Filter::NoFilter, "none"),
    (png::Filter::Up, "up"),
    (png::Filter::Sub, "sub"),
    (png::Filter::Paeth, "paeth"),
    (png::Filter::Adaptive, "adaptive"),
];

fn encode_with_filter(
    data: &[u8],
    width: u32,
    height: u32,
    depth: png::BitDepth,
    color_type: png::ColorType,
    indexed: IndexedData<'_>,
    filter: png::Filter,
) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_depth(depth);
        encoder.set_color(color_type);
        if let Some((colors, transparencies)) = indexed {
            encoder.set_palette(colors);
            if !transparencies.is_empty() {
                encoder.set_trns(transparencies);
            }
        }
        encoder.set_deflate_compression(png::DeflateCompression::Level(9));
        encoder.set_filter(filter);
        let mut writer = encoder
            .write_header()
            .map_err(|e| Error::Encoding(e.to_string()))?;
        writer
            .write_image_data(data)
            .map_err(|e| Error::Encoding(e.to_string()))?;
        writer
            .finish()
            .map_err(|e| Error::Encoding(e.to_string()))?;
    }
    Ok(bytes)
}

fn encode_data(
    data: &[u8],
    width: u32,
    height: u32,
    depth: png::BitDepth,
    color_type: png::ColorType,
    indexed: IndexedData<'_>,
) -> Result<(Vec<u8>, &'static str), Error> {
    let mut selected = [true; 5];
    if color_type == png::ColorType::Rgb && height >= 192 && data.len() >= 65536 {
        // Rank RGB filters on first/middle/last row strips. Palette, alpha and
        // grayscale retain exhaustive search: sample rankings can miss their
        // full-image winner. This heuristic changes no reconstructed samples.
        let stride = data.len() / height as usize;
        let mut preview = Vec::with_capacity(stride * 48);
        for start in [0, (height as usize - 16) / 2, height as usize - 16] {
            preview.extend_from_slice(&data[start * stride..(start + 16) * stride]);
        }
        let mut ranked = Vec::with_capacity(FILTERS.len());
        for (index, &(filter, _)) in FILTERS.iter().enumerate() {
            let bytes =
                encode_with_filter(&preview, width, 48, depth, color_type, indexed, filter)?;
            ranked.push((bytes.len(), index));
        }
        ranked.sort_unstable();
        selected.fill(false);
        for (rank, &(bytes, index)) in ranked.iter().enumerate() {
            // Retain at least two, and all filters within 1% of the best
            // preview so near-ties do not become arbitrary exclusions.
            selected[index] = rank < 2 || bytes <= ranked[0].0 + ranked[0].0 / 100;
        }
    }
    let mut best: Option<(Vec<u8>, &'static str)> = None;
    for (index, &(filter, name)) in FILTERS.iter().enumerate() {
        if !selected[index] {
            continue;
        }
        let bytes = encode_with_filter(data, width, height, depth, color_type, indexed, filter)?;
        if best.as_ref().is_none_or(|b| bytes.len() < b.0.len()) {
            best = Some((bytes, name));
        }
    }
    best.ok_or(Error::Encoding("no PNG filter candidates".into()))
}

#[cfg(test)]
mod mapping_tests {
    use super::*;

    fn random_rgb(n: u32) -> [u8; 3] {
        let mut n = n.wrapping_mul(747796405).wrapping_add(2891336453);
        n ^= n >> 16;
        [n as u8, (n >> 8) as u8, (n >> 16) as u8]
    }

    #[test]
    fn cache_admission_follows_color_reuse_not_image_identity_or_pixel_limit() {
        let mut reference = Reference {
            pixels: RgbImage::from_pixel(129, 65, image::Rgb([70, 110, 190])),
            alpha: None,
            source_format: "png",
            jpeg_metadata: Default::default(),
        };
        assert!(cache_has_reuse(&reference));
        reference.pixels = RgbImage::from_fn(129, 65, |x, y| image::Rgb(random_rgb(x + y * 129)));
        assert!(!cache_has_reuse(&reference));
    }

    #[test]
    fn nearest_cache_hash_collisions_do_not_reuse_another_colors_index() {
        let second = (1..1u32 << 24)
            .find(|key| key.wrapping_mul(0x9e3779b1) >> 16 == 0)
            .unwrap();
        let colors = [
            [0, 0, 0],
            [(second >> 16) as u8, (second >> 8) as u8, second as u8],
        ];
        let reference = Reference {
            pixels: RgbImage::from_fn(257, 17, |x, y| image::Rgb(colors[((x + y) % 2) as usize])),
            alpha: None,
            source_format: "png",
            jpeg_metadata: Default::default(),
        };
        let analysis = Analysis::new(&reference.pixels).unwrap();
        for palette in [colors, [colors[1], colors[0]]] {
            let mut mapper = Mapper::new(&reference, &palette, true);
            mapper.nearest_cache = vec![(u32::MAX, 0); 65536];
            for _ in 0..2 {
                let (_, mapped) = mapper.map(&reference, &analysis, 0);
                assert_eq!(mapped, reference.pixels);
            }
        }
    }

    #[test]
    fn exact_cell_search_keeps_a_third_color_missed_by_center_guesses() {
        let reference = Reference {
            pixels: RgbImage::from_pixel(1, 1, image::Rgb([128; 3])),
            alpha: None,
            source_format: "png",
            jpeg_metadata: Default::default(),
        };
        let analysis = Analysis::new(&reference.pixels).unwrap();
        let palette = [[128; 3], [132; 3], [134, 132, 132]];
        let (_, approximate) =
            Mapper::new(&reference, &palette, false).map(&reference, &analysis, 0);
        assert_ne!(approximate, reference.pixels);
        let (_, exact) = Mapper::new(&reference, &palette, true).map(&reference, &analysis, 0);
        assert_eq!(exact, reference.pixels);
    }

    #[test]
    fn interval_pruning_matches_exhaustive_search_including_coherence_and_ties() {
        let reference = Reference {
            pixels: RgbImage::from_fn(37, 19, |x, y| image::Rgb(random_rgb(x + y * 37))),
            alpha: None,
            source_format: "png",
            jpeg_metadata: Default::default(),
        };
        let analysis = Analysis::new(&reference.pixels).unwrap();
        for count in [1, 2, 3, 16, 64, 128, 256] {
            let palette: Vec<_> = (0..count).map(|n| random_rgb(n + 1111)).collect();
            let values: Vec<_> = palette.iter().copied().map(color).collect();
            let mut mapper = Mapper::new(&reference, &palette, true);
            mapper.nearest_cache.clear();
            let mut cached_mapper = Mapper::new(&reference, &palette, true);
            cached_mapper.nearest_cache = vec![(u32::MAX, 0); 65536];
            for coherence in [0u32, 64, 8192] {
                let (indices, _) = mapper.map(&reference, &analysis, coherence);
                let mut expected: Vec<u8> = Vec::new();
                for (i, original) in analysis.colors.iter().enumerate() {
                    let left = if i % 37 > 0 { expected[i - 1] } else { 0 };
                    let up = if i >= 37 { expected[i - 37] } else { 0 };
                    let chosen = values
                        .iter()
                        .enumerate()
                        .min_by_key(|&(index, &v)| {
                            let transitions = u64::from(i % 37 > 0 && index != usize::from(left))
                                + u64::from(i >= 37 && index != usize::from(up));
                            (
                                distance(v, original) * u64::from(analysis.weights[i])
                                    + u64::from(coherence) * transitions,
                                index,
                            )
                        })
                        .unwrap()
                        .0 as u8;
                    expected.push(chosen);
                }
                assert_eq!(indices, expected, "palette={count}, coherence={coherence}");
                let (cached_indices, _) = cached_mapper.map(&reference, &analysis, coherence);
                assert_eq!(
                    cached_indices, expected,
                    "cached palette={count}, coherence={coherence}"
                );
            }
        }
    }

    #[test]
    fn exact_mapping_preserves_alpha_and_hidden_rgb_after_encoding() {
        let reference = Reference {
            pixels: RgbImage::from_fn(37, 19, |x, y| image::Rgb(random_rgb(x + y * 37))),
            alpha: Some(image::GrayImage::from_fn(37, 19, |x, y| {
                image::Luma([((x + y * 37) % 256) as u8])
            })),
            source_format: "png",
            jpeg_metadata: Default::default(),
        };
        let analysis =
            Analysis::for_reference(&reference, crate::resources::DEFAULT_MEMORY_BYTES).unwrap();
        let palette: Vec<_> = (0..256).map(|n| random_rgb(n + 1111)).collect();
        let (indices, mapped) =
            Mapper::new(&reference, &palette, true).map(&reference, &analysis, 64);
        let (data, _) = encode_for_reference(&reference, &indices, &palette, &mapped).unwrap();
        let decoded = crate::decode_baseline(&data).unwrap();
        assert_eq!(decoded.alpha, reference.alpha);
        assert_eq!(decoded.pixels, mapped);
        crate::quality::verify_transparency(
            &reference,
            &data,
            crate::resources::DEFAULT_MEMORY_BYTES,
        )
        .unwrap();
    }
}

#[cfg(test)]
mod exact_tests {
    use super::*;

    #[test]
    fn rgb_filter_preview_keeps_full_dimensions_and_exact_pixels() {
        let reference = Reference {
            pixels: RgbImage::from_fn(273, 197, |x, y| {
                image::Rgb([(x * 7) as u8, (y * 11) as u8, (x + y * 13) as u8])
            }),
            alpha: None,
            source_format: "png",
            jpeg_metadata: Default::default(),
        };
        let (data, _) = encode_truecolor(&reference, &reference.pixels).unwrap();
        let decoded = crate::decode_baseline(&data).unwrap();
        assert_eq!(decoded.pixels, reference.pixels);
        assert!(decoded.alpha.is_none());
    }

    #[test]
    fn exhaustive_indexed_filters_keep_odd_width_packed_rows_exact() {
        let palette = [[11, 33, 77], [220, 180, 60], [40, 210, 90]];
        let (width, height) = (1025, 257);
        let indices: Vec<_> = (0..width * height)
            .map(|i| ((i * 71 + i / width) % 3) as u8)
            .collect();
        let (data, _) = encode(&indices, &palette, width, height).unwrap();
        let decoded = crate::decode_baseline(&data).unwrap();
        assert_eq!(decoded.pixels.dimensions(), (width, height));
        for (p, index) in decoded.pixels.pixels().zip(indices) {
            assert_eq!(p.0, palette[usize::from(index)]);
        }
    }

    #[test]
    fn exhaustive_rgba_filters_keep_dimensions_and_every_sample_exact() {
        let reference = Reference {
            pixels: RgbImage::from_fn(273, 197, |x, y| {
                image::Rgb([(x * 7) as u8, (y * 11) as u8, (x + y * 13) as u8])
            }),
            alpha: Some(image::GrayImage::from_fn(273, 197, |x, y| {
                image::Luma([(x + y * 273) as u8])
            })),
            source_format: "png",
            jpeg_metadata: Default::default(),
        };
        let (data, _) = encode_truecolor(&reference, &reference.pixels).unwrap();
        assert_eq!(
            crate::decode_baseline(&data).unwrap().rgba().unwrap(),
            reference.rgba().unwrap()
        );
    }

    #[test]
    fn all_gray_levels_survive_exact_representation_optimization() {
        let reference = Reference {
            pixels: RgbImage::from_fn(256, 64, |x, y| {
                let mut n = (x + y * 256)
                    .wrapping_mul(747796405)
                    .wrapping_add(2891336453);
                n ^= n >> 16;
                image::Rgb([n as u8; 3])
            }),
            alpha: None,
            source_format: "png",
            jpeg_metadata: Default::default(),
        };
        let data = crate::encode_png_lossless(&reference).unwrap();
        let decoded = crate::decode_baseline(&data).unwrap();
        assert_eq!(decoded.pixels, reference.pixels);
        assert!(decoded.alpha.is_none());
        assert!(
            data.len()
                < encode_truecolor(&reference, &reference.pixels)
                    .unwrap()
                    .0
                    .len()
        );
        assert_eq!(
            data[25], 0,
            "full-range gray data needs no RGB or palette overhead"
        );
    }

    #[test]
    fn close_sparse_colors_and_alpha_are_not_merged_by_histogram_cells() {
        let colors = [[11, 12, 13], [12, 13, 14], [13, 14, 15], [14, 15, 16]];
        let reference = Reference {
            pixels: RgbImage::from_fn(65, 33, |x, y| {
                image::Rgb(colors[((x + y * 65) % 4) as usize])
            }),
            alpha: Some(image::GrayImage::from_fn(65, 33, |x, y| {
                image::Luma([((x + y) % 7 * 40) as u8])
            })),
            source_format: "png",
            jpeg_metadata: Default::default(),
        };
        let data = crate::encode_png_lossless(&reference).unwrap();
        let decoded = crate::decode_baseline(&data).unwrap();
        assert_eq!(decoded.rgba().unwrap(), reference.rgba().unwrap());
    }
}
