//! Memory management.
//!
//! This module provides:
//! - `multiboot2`: Parsing of the multiboot2 boot information (memory map)
//! - `frame`: Physical frame allocator (bitmap-based, 4 KiB frames)
//! - `paging`: Page table manipulation and virtual memory management
//! - `heap`: Kernel heap allocator (`#[global_allocator]`)

#[cfg(feature = "frame-alloc")]
pub mod frame;
#[cfg(feature = "heap")]
pub mod heap;
pub mod multiboot2;
#[cfg(feature = "paging")]
pub mod paging;
