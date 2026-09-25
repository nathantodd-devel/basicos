//! The process heap — a `#[global_allocator]`, so `Vec`, `String`,
//! `Box`, `format!` and the rest of `alloc` just work.
//!
//! A first-fit free list, kept sorted by address so adjacent free blocks
//! coalesce on `dealloc` (freed memory gets reused rather than leaking —
//! a long-running program that allocates and frees in a loop stays
//! flat). Grows on demand: first via `brk`, and if the kernel refuses
//! (the heap window is fixed-size), from the `mmap` arena.
//!
//! Single-threaded by construction — basicos processes have exactly one
//! thread — so the allocator state isn't locked. That stops being true
//! the day threads exist; this needs a lock then.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::mem::size_of;
use core::ptr;

use crate::mem;

/// A free region: this header lives in the first bytes of the region
/// itself, which is why every block is at least this big.
#[repr(C)]
struct Node {
    size: usize,
    next: *mut Node,
}

/// Block granularity. Every block size and address the heap deals in is a
/// multiple of this, which is also `Node`'s size — so any leftover piece
/// of a split is always big enough to hold its own header, with no
/// "sliver too small to track" case to handle.
const GRANULE: usize = 16;
const _: () = assert!(size_of::<Node>() == GRANULE);

/// How much to ask the kernel for at a time.
const GROW_CHUNK: usize = 64 * 1024;

fn align_up(v: usize, a: usize) -> usize {
    (v + a - 1) & !(a - 1)
}

struct Heap {
    /// Free list, ascending by address.
    head: *mut Node,
}

impl Heap {
    /// Insert `[addr, addr+size)` into the free list, merging with
    /// neighbours it touches.
    unsafe fn add_region(&mut self, addr: usize, size: usize) {
        unsafe {
            let mut prev: *mut Node = ptr::null_mut();
            let mut cur = self.head;
            while !cur.is_null() && (cur as usize) < addr {
                prev = cur;
                cur = (*cur).next;
            }

            let node = addr as *mut Node;
            (*node).size = size;
            (*node).next = cur;
            // Merge forward.
            if !cur.is_null() && addr + size == cur as usize {
                (*node).size += (*cur).size;
                (*node).next = (*cur).next;
            }
            // Link in / merge backward.
            if prev.is_null() {
                self.head = node;
            } else if prev as usize + (*prev).size == addr {
                (*prev).size += (*node).size;
                (*prev).next = (*node).next;
            } else {
                (*prev).next = node;
            }
        }
    }

    /// First-fit search; carves `size` bytes at `align` out of a free
    /// region, leaving any front/back remainder on the list.
    unsafe fn take(&mut self, size: usize, align: usize) -> *mut u8 {
        unsafe {
            let mut prev: *mut Node = ptr::null_mut();
            let mut cur = self.head;
            while !cur.is_null() {
                let start = cur as usize;
                let end = start + (*cur).size;
                let a = align_up(start, align);
                if a + size <= end {
                    let front = a - start; // multiple of GRANULE: 0 or >= a header
                    let back = end - (a + size);
                    let next = (*cur).next;

                    let mut tail = next;
                    if back > 0 {
                        let b = (a + size) as *mut Node;
                        (*b).size = back;
                        (*b).next = next;
                        tail = b;
                    }
                    if front > 0 {
                        (*cur).size = front;
                        (*cur).next = tail;
                    } else if prev.is_null() {
                        self.head = tail;
                    } else {
                        (*prev).next = tail;
                    }
                    return a as *mut u8;
                }
                prev = cur;
                cur = (*cur).next;
            }
            ptr::null_mut()
        }
    }

    /// Get more memory from the kernel and add it to the free list.
    unsafe fn grow(&mut self, need: usize) -> bool {
        let chunk = align_up(need.max(GROW_CHUNK), 4096);

        let cur = mem::brk(0);
        let start = align_up(cur, GRANULE);
        let want = start + chunk;
        if mem::brk(want) >= want {
            unsafe { self.add_region(start, chunk) };
            return true;
        }
        // brk window exhausted — fall back to the mmap arena.
        match mem::mmap(chunk, true) {
            Ok(p) => {
                unsafe { self.add_region(p as usize, chunk) };
                true
            }
            Err(_) => false,
        }
    }
}

pub struct Allocator {
    heap: UnsafeCell<Heap>,
}

// Safety: see the module docs — one thread per process.
unsafe impl Sync for Allocator {}

fn block_size(layout: Layout) -> usize {
    align_up(layout.size().max(GRANULE), GRANULE)
}

unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let heap = unsafe { &mut *self.heap.get() };
        let size = block_size(layout);
        let align = layout.align().max(GRANULE);
        loop {
            let p = unsafe { heap.take(size, align) };
            if !p.is_null() {
                return p;
            }
            // `+ align` covers the worst case of alignment padding.
            if !unsafe { heap.grow(size + align) } {
                return ptr::null_mut();
            }
        }
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        let heap = unsafe { &mut *self.heap.get() };
        unsafe { heap.add_region(p as usize, block_size(layout)) };
    }
}

#[global_allocator]
static ALLOCATOR: Allocator = Allocator {
    heap: UnsafeCell::new(Heap {
        head: ptr::null_mut(),
    }),
};
