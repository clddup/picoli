//! C allocator callbacks backed by Rust, with checked layout and null handling.
//! A sixteen-byte header keeps returned storage sixteen-byte aligned on both
//! native and wasm32. The original Apache-2.0 package license is retained.
use libdeflate_sys::libdeflate_options;
use std::alloc::{alloc, dealloc, Layout};
use std::ffi::c_void;
use std::mem::{align_of, size_of};

#[repr(C, align(16))]
struct Header {
    size: usize,
}
fn layout_for(size: usize) -> Option<Layout> {
    Layout::from_size_align(size.checked_add(size_of::<Header>())?, align_of::<Header>()).ok()
}
unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
    let Some(layout) = layout_for(size) else {
        return std::ptr::null_mut();
    };
    // SAFETY: layout is checked. Never write through an allocation failure.
    let base = unsafe { alloc(layout) };
    if base.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        base.cast::<Header>().write(Header { size });
        base.add(size_of::<Header>()).cast()
    }
}
unsafe extern "C" fn free(data: *mut c_void) {
    if data.is_null() {
        return;
    }
    // SAFETY: libdeflate returns only pointers from the paired callback; the
    // complete checked allocation is reconstructed before deallocation.
    let base = unsafe { data.cast::<u8>().sub(size_of::<Header>()) };
    let size = unsafe { (*base.cast::<Header>()).size };
    let Some(layout) = layout_for(size) else {
        std::process::abort();
    };
    unsafe {
        dealloc(base, layout);
    }
}
pub static OPTIONS: libdeflate_options = libdeflate_options {
    sizeof_options: size_of::<libdeflate_options>(),
    malloc_func: Some(malloc),
    free_func: Some(free),
};
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn allocations_are_aligned_writable_and_freed_with_the_same_layout() {
        for size in [0usize, 1, 7, 8, 15, 16, 17, 4096] {
            let p = unsafe { (OPTIONS.malloc_func.unwrap())(size) };
            assert!(!p.is_null());
            assert_eq!(p as usize % 16, 0);
            unsafe {
                std::ptr::write_bytes(p.cast::<u8>(), 0xa5, size);
                (OPTIONS.free_func.unwrap())(p);
            }
        }
    }
    #[test]
    fn overflow_returns_null_and_free_null_is_a_noop() {
        assert!(layout_for(usize::MAX).is_none());
        assert!(unsafe { malloc(usize::MAX) }.is_null());
        unsafe {
            free(std::ptr::null_mut());
        }
    }
}
