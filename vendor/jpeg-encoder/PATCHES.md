# Local changes to jpeg-encoder 0.7.1

This directory is a modified copy of `jpeg-encoder` 0.7.1, maintained locally for the Picoli experiment. The upstream project is <https://github.com/vstroebel/jpeg-encoder>. Its license expression is `(MIT OR Apache-2.0) AND IJG`. The original MIT, Apache-2.0 and IJG license files and source notices remain in this directory.

This software is based in part on the work of the Independent JPEG Group. The upstream integer DCT implementation also acknowledges its libjpeg-turbo and MozJPEG provenance. Those implementations are not Picoli inventions.

## Standard progressive EOBRUN support

The local `progressive.rs` module implements JPEG spectral-selection AC end-of-band runs. It is a standard JPEG coding feature, not a new compression algorithm. A shared AC event walker produces the symbols used both for optimized Huffman frequency counting and actual entropy serialization. It flushes pending runs before nonzero coefficients, restart markers and scan completion, and limits runs to 32767 blocks.

`encoder.rs` adds the opt-in `set_progressive_eob_runs` flag, disabled by default. The flag requires progressive scans and optimized Huffman tables. Unsupported combinations return the explicit `InvalidConfiguration` error added in `error.rs`. The existing encoder path remains available with the flag disabled. Optimized DC frequency counting now resets its predictor at restart boundaries, matching entropy serialization.

The product tries this feature only for its final selected JPEG parameters and scan layout. It retains a new file only when the complete output is smaller and the shared decoder reconstructs exactly the same pixels. It does not change transforms, quantization, quality limits, sampling, display metadata or source dimensions.

Tests cover run boundaries, signed nonzero coefficients, ZRL, spectral ranges and restart flushes. An independent decoder compares flag-off and flag-on output pixels across multiple patterns, sampling modes, scan counts and restart intervals. Product-level tests additionally exercise metadata and the final-winner admission rule.

## Opt-in per-scan AC Huffman tables

The `set_progressive_scan_huffman` option is disabled by default and requires progressive EOBRUN coding and optimized Huffman tables. For each component and AC spectral band, it uses the same event walker to build a bounded 257-entry frequency table and emit the entropy stream. It writes the optimized DHT immediately before that scan's SOS, redefining the appropriate table ID. Unused initial global AC tables are omitted. The existing DC tables and all quantized coefficients remain unchanged.

This is a standard JPEG capability, independently implemented here. The research comparator's `encode.rs` documents per-scan AC tables matching C MozJPEG; see <https://github.com/imazen/mozjpeg-rs>. That crate is not a product dependency, and its implementation was not copied. Tests compare exact reconstructed pixels for multiple patterns, sampling modes, scan counts and restart intervals, including repeated restart-marker numbering. Incompatible options are explicit errors.

The product compares the existing two- and four-scan layouts only after selecting its original JPEG quantization and sampling parameters. It retains at most the smallest strictly smaller, pixel-identical alternative. No quality limits are relaxed, no input pixels are filtered or resized, and display metadata is still attached by the core.

## Opt-in integer AC coefficient rate-distortion optimization

The `set_experimental_ac_rdo_lambda` option defaults to zero. Zero bypasses the new coefficient solver entirely and retains the existing quantizer. Nonzero values require progressive encoding and 8-bit DQT entries; incompatible configurations return `InvalidConfiguration` before writing JPEG headers. The bridge uses the actual internal quantization step, scaled by eight to match the integer FDCT, rather than the byte-sized DQT accessor.

The independently implemented shortest-path solver minimizes integer squared coefficient error plus lambda times a fixed standard-Huffman bit estimate within each existing AC spectral band. It considers zero runs, ZRL, amplitude categories and the terminal EOB, while preserving DC coefficients. This is a standard trellis/rate-distortion technique, not a new JPEG invention. The estimate does not model cross-block progressive EOBRUN savings or predict future optimized Huffman tables. The existing encoder still computes and writes the actual optimized entropy stream afterward; the model cost is not claimed to be complete file size or image-domain perceptual quality.

An exact nonnegative-error pruning rule skips a category only when its local squared error already exceeds an available complete prefix cost. A real pruning-disabled solver remains in tests, with complete coefficient, cost, bit-count and tie comparisons. Exhaustive small-band tests, unavailable-symbol cases, extreme lambdas and an independent JPEG decoder cover the solver and actual encoding bridge. A debug-only missing-Huffman-symbol panic was reproduced before fixing symbol lookup; its failure evidence remains in the isolated research artifacts.

The core separately checks the complete encoded file and decoded image. Its bounded policy first preserves both original uniform quantizer-margin/lambda trials, `(95, 256)` and `(90, 1024)`. Only when neither produces a strictly smaller admissible file does it try one additional `(95, 256)` candidate with component and sampling weights. Thus there are at most three extra nonzero-lambda encodings, not two. Every retained file must be strictly smaller and must not regress in any frozen pixel, edge or worst-tile component. A successful original trial is retained exactly, without running the complementary trial. Original-file fallback, PNG and strength zero are not refined. Metadata attachment remains the core's responsibility. These checks are proxy constraints, not human visual acceptance, browser validation or evidence of superiority over another compressor.

The opt-in `set_experimental_ac_rdo_sampling_weights` flag defaults to false. In a three-component encoding, its fixed heuristic Y/Cb/Cr weights are 100/143/99; other component counts use 100. An independent review corrected the original derivation claim: the continuous inverse-color-transform diagonal of the frozen metric is approximately 100/116.33/99.21, not 100/143/99. The already frozen and measured model retains 143 as a heuristic, not as an analytically derived constant; changing it would be a separate experiment requiring new validation. The effective lambda is the rounded base lambda times 100, divided by the component weight and the horizontal and vertical downsampling factors, with nonzero results clamped to the valid integer range. The sampling-area factor accounts approximately for the number of output pixels represented by a subsampled coefficient; the component weights remain heuristic. They do not model cross-channel terms, content weights, resampling, edges or tiles exactly. Zero lambda retains its complete bypass. No full-image buffer or new dependency is introduced. Actual decoded-image checks remain mandatory.

For AC bands longer than 32 positions, an exact prefix-minimum implementation groups predecessors by zero-run length modulo 16. The ZRL rate and all-zero distortion prefix separate into one signed offset per residue. Each query therefore considers at most 16 predecessor groups rather than every earlier state. The selected coefficient, complete cost, bit count and deterministic predecessor ties are unchanged. Short bands retain the direct solver to avoid grouping overhead. When the ZRL symbol is unavailable, each residue is replaced as its predecessor leaves the legal 16-position window; unavailable EOB and amplitude symbols retain their original handling.

The original direct solver remains the test oracle. Differential tests compare complete solutions across 512 deterministic full-range coefficient/quantizer seeds, seven spectral ranges, six lambdas including the maximum value, missing symbols, terminal cases and invalid arguments. Signed offsets and complete costs stay below 2^49 in magnitude under the validated input bounds. This is an arithmetic and state-search optimization of the same model, not a new compression decision or a claim of universally faster end-to-end encoding.

No MozJPEG implementation was copied or added as a product dependency. Research references include the [Mozilla MozJPEG overview](https://github.com/mozilla/mozjpeg/blob/master/README-mozilla.txt), the [Rust MozJPEG comparator](https://github.com/imazen/mozjpeg-rs), and [HB2023 JPEG coefficient optimization research](https://informationsecurity.uibk.ac.at/pdfs/HB2023_IHMMSEC.pdf). Original upstream notices and all three license files remain unchanged.

## Pre-existing high-scan spectral-range fix

The upstream public API accepts 2 through 64 scans per component. Its original `64 / (scans - 1)` partition produced an empty first AC range when the requested count was 34 or greater. The permanent regression first reproduced an independent-decoder failure at 34 scans (`ss=1, se=0`); the failure log is retained.

A shared AC-range iterator now feeds both statistics and serialization. All ranges are nonempty and together cover coefficients 1 through 63 exactly once. The existing boundaries for counts 2 through 33 remain identical, including the product's only allowed layouts, 2 and 4. Independent-decoder tests cover every accepted count, each supported product sampling mode and both EOB flag settings, and compare exact pixels with the two-scan control. No failed assertion is skipped or relaxed.

## Non-behavioral lint maintenance

The quantization loop iterates over the existing 64-entry zigzag table using `enumerate`. Tests no longer clone `Copy` arrays or add redundant borrows, and one test helper accepts a mutable slice instead of a mutable vector. These changes address Clippy diagnostics without removing assertions or suppressing new warnings.

The separate MozJPEG Rust source investigation was a research comparator. Its crate is not a product dependency, and the local EOBRUN implementation does not claim MozJPEG's trellis functionality.

## Experimental reusable FDCT coefficient preparation

Under the existing `experimental-coefficients` feature, `PreparedFdct` precomputes RGB-to-YCbCr color conversion, chroma subsampling, and integer FDCT blocks once per image and sampling factor, reusing the encoder's existing color conversion and `fdct` routines. `PreparedFdct::quantize_into` writes MCU-aligned zigzagged quantized `i16` coefficients into reusable caller buffers using the encoder's existing quantizer layout, so scale/table search can evaluate and encode multiple quantization tables via `encode_quantized` without repeating color conversion or FDCT on every trial.
