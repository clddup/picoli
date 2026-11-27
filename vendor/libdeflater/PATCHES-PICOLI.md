# Isolated Picoli research changes

This directory contains libdeflater 1.25.2, an Apache-2.0 Rust wrapper around the upstream C libdeflate codec. The original notices and license remain intact. The codec's compression algorithms are upstream work, not Picoli inventions. The C library has its own retained COPYING notice in libdeflate-sys.

The local changes add `Compressor::try_new`, preserving the original constructor while allowing a null C allocation to return `None`. The Rust allocator callbacks now use checked layout arithmetic, check allocation failure before writing a header, tolerate `free(NULL)`, and retain a sixteen-byte-aligned allocation header. The header stores the requested size so that freeing reconstructs the original Rust layout. The callback tests cover writable aligned storage, zero-size storage, overflow, and null freeing.

The empty workspace declaration isolates this research copy from parent workspace discovery. It does not change the library API or licensing.

The consuming probe uses Cargo resolver 2 and target-specific dependencies: native builds use the normal C allocator and do not enable `freestanding`; wasm32 builds enable upstream `freestanding` and `use_rust_alloc`. Enabling freestanding for native previously exported scalar C memory functions into an unoptimized native test and caused a severe slowdown. The interrupted test log and CPU sample are retained outside this directory. The same assertions must complete on the corrected build; interruption is not a passing test.

The allocator source is also included directly in the probe's native tests, so the exact callback implementation is tested without changing the normal native library allocator. Actual wasm32 execution remains a separate required check. This research does not claim a browser test, a Rust 1.88 toolchain test, a product promotion, or a compression result before those checks are performed.
