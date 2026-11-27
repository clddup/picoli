# Local decoder patch

This copy comes from the crates.io `zune-jpeg` 0.5.15 package. Its package metadata identifies upstream commit `31d81fed7551c8ccea456d9d8e2b1fd8bebb6995`, path `crates/zune-jpeg`, in <https://github.com/etemesi254/zune-image>. Original source headers and MIT, Apache-2.0 and Zlib license files are retained.

The local change is limited to `src/bitstream.rs`, `src/mcu.rs` and `src/mcu_prog.rs`. The bitstream separately counts real entropy bits and records consumption of synthetic marker/EOF padding. Strict sequential and progressive decode paths reject that consumption; sequential decoding also rejects a marker that terminates an incomplete MCU row. Restart resets discard buffered padding but never erase an already detected violation. Lookahead may still use zeros without consuming them as coefficients, and permissive decoding behavior remains available to non-strict callers.

The workspace patch applies to both Picoli and its transitive image dependency. It does not modify the machine's shared Cargo registry. This is a local patch, not a claim that upstream has published the fix. Integration coverage lives in `picoli-core` and includes the original formerly ignored regression, scan truncation, valid final padding, odd dimensions, grayscale, three chroma sampling modes, progressive scans and restart markers.
