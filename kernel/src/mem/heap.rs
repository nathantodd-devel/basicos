//! Kernel heap allocator, wired up as `#[global_allocator]`.
//!
//! With `frame-alloc` on, the heap claims most of physical RAM — real
//! "full heap access" rather than a small fixed pool — by asking
//! `mem::frame` for the biggest contiguous run of frames it can get
//! (see [`init`] for exactly how). Physical == virtual here (the
//! boot-time identity map, not per-process paging — see `todo.md`), so
//! whatever frames come back are immediately usable as-is.
//!
//! Without `frame-alloc` there's no way to ask how much RAM exists or
//! claim any of it, so the heap falls back to the original fixed-size
//! `.bss` region — the same trick `lib.rs` uses for `USER_STACK`.
//!
//! Either way it's backed by a hand-rolled intrusive free-list
//! allocator (first-fit, no coalescing on free): enough to unblock
//! `alloc::{boxed::Box, vec::Vec, ...}` before real memory management
//! exists. Non-coalescing does mean a workload that keeps reallocating
//! ever-larger buffers (a `Vec` growing without bound, say) can exhaust
//! the heap well before every byte of it is technically "in use" —
//! that's expected, not a bug, and it fails cleanly: `GlobalAlloc::alloc`
//! returning null routes through Rust's normal `handle_alloc_error` ->
//! panic path (`panic.rs`), not silent corruption.

use crate::sync::Spinlock;
use core::alloc::{GlobalAlloc, Layout};
use core::mem;
use core::ptr;

#[cfg(not(feature = "frame-alloc"))]
mod fixed {
    /// No frame allocator to ask "how much RAM is there" — 1 MiB,
    /// arbitrary but comfortably more than this kernel's own structures
    /// need (FAT32 buffers, directory listings, syscall file
    /// descriptors, ...) without frame-backed sizing to lean on.
    pub const HEAP_SIZE: usize = 1024 * 1024;

    #[repr(align(16))]
    pub struct HeapMemory(pub [u8; HEAP_SIZE]);

    pub static mut HEAP_MEMORY: HeapMemory = HeapMemory([0; HEAP_SIZE]);
}

#[global_allocator]
static ALLOCATOR: Spinlock<LinkedListAllocator> = Spinlock::new(LinkedListAllocator::new());

/// Hand the allocator its backing region and return its size in bytes.
///
/// Call once from `kernel_main`, before any code uses `alloc::{Box, Vec,
/// ...}` — allocations before this point just get a null pointer back.
///
/// # Panics
/// With `frame-alloc` on, if even a minimal (1 MiB) contiguous claim
/// fails — meaning there's essentially no usable RAM at all, a boot
/// environment nothing else here would work in either.
pub fn init() -> usize {
    #[cfg(feature = "frame-alloc")]
    {
        init_dynamic()
    }
    #[cfg(not(feature = "frame-alloc"))]
    {
        unsafe {
            let heap_start = core::ptr::addr_of!(fixed::HEAP_MEMORY.0) as usize;
            ALLOCATOR.lock().init(heap_start, fixed::HEAP_SIZE);
        }
        fixed::HEAP_SIZE
    }
}

#[cfg(feature = "frame-alloc")]
fn init_dynamic() -> usize {
    use crate::mem::frame;

    // Free space is essentially never one perfectly contiguous run by
    // this point: the low 1 MiB, the kernel image, and (if `elf-loader`
    // is on) the reserved ELF load zone have all already carved chunks
    // out of it (see `lib.rs`). `allocate_contiguous` needs one run, so
    // ask for "all of it" and back off geometrically until a claim
    // actually lands — that finds the largest surviving contiguous
    // block without needing a "what's my biggest run" query this
    // allocator doesn't have.
    //
    // Start from half of free memory, not all of it: every process's pages
    // and page tables come from what's left. Claiming everything meant the
    // split depended on where the largest run happened to fall, so a
    // change in the kernel's size could leave processes a few MiB, and
    // `exec` failing for want of frames.
    const MIN_HEAP_FRAMES: usize = 256; // 1 MiB — the old fixed size, as a floor

    let mut want = frame::free_count() / 2;
    let frame = loop {
        assert!(
            want >= MIN_HEAP_FRAMES,
            "mem::heap: couldn't claim even a {} KiB contiguous heap ({} frames free total)",
            MIN_HEAP_FRAMES * frame::FRAME_SIZE / 1024,
            frame::free_count(),
        );
        match frame::allocate_contiguous(want) {
            Some(frame) => break frame,
            None => want = want * 3 / 4, // back off 25% and retry
        }
    };

    let heap_start = frame.start_address();
    let heap_size = want * frame::FRAME_SIZE;
    unsafe {
        ALLOCATOR.lock().init(heap_start, heap_size);
    }
    heap_size
}

/// A free block, stored inline at the start of its own memory.
struct ListNode {
    size: usize,
    next: Option<&'static mut ListNode>,
}

impl ListNode {
    const fn new(size: usize) -> Self {
        ListNode { size, next: None }
    }

    fn start_addr(&self) -> usize {
        self as *const Self as usize
    }

    fn end_addr(&self) -> usize {
        self.start_addr() + self.size
    }
}

/// First-fit free-list allocator. `head` is a dummy node whose `next`
/// points at the first real free block. The list is kept in address order
/// so that a freed block merges with its neighbours: without that, every
/// split is permanent, and enough mixed-size traffic (a network soak does
/// it in about 65,000 packets) shreds the heap until a small allocation
/// fails with most of it free.
struct LinkedListAllocator {
    head: ListNode,
}

impl LinkedListAllocator {
    const fn new() -> Self {
        Self {
            head: ListNode::new(0),
        }
    }

    /// Seed the free list with one region covering the whole heap.
    ///
    /// # Safety
    /// `heap_start..heap_start + heap_size` must be valid, unused memory,
    /// and this must be called at most once.
    unsafe fn init(&mut self, heap_start: usize, heap_size: usize) {
        self.add_free_region(heap_start, heap_size);
    }

    /// Return a region of memory to the free list, merging it with the
    /// free blocks on either side if they touch.
    ///
    /// # Safety
    /// The region must be valid, unused, correctly aligned for
    /// `ListNode`, and at least `size_of::<ListNode>()` bytes.
    unsafe fn add_free_region(&mut self, addr: usize, size: usize) {
        assert_eq!(align_up(addr, mem::align_of::<ListNode>()), addr);
        assert!(size >= mem::size_of::<ListNode>());

        let mut prev: *mut ListNode = &mut self.head;
        while let Some(node) = (*prev).next.as_deref_mut() {
            if node.start_addr() > addr {
                break;
            }
            prev = node;
        }

        let mut size = size;
        let mut next = (*prev).next.take();
        if let Some(node) = next.take_if(|n| n.start_addr() == addr + size) {
            size += node.size;
            next = node.next.take();
        }

        // `head`, the only node of size 0, is never a neighbour.
        if (*prev).size != 0 && (*prev).end_addr() == addr {
            (*prev).size += size;
            (*prev).next = next;
            return;
        }

        let node_ptr = addr as *mut ListNode;
        node_ptr.write(ListNode { size, next });
        (*prev).next = Some(&mut *node_ptr);
    }

    /// Find a free region big enough for `size`/`align`, unlink it from
    /// the free list, and return it along with the aligned allocation
    /// start address within it.
    fn find_region(&mut self, size: usize, align: usize) -> Option<(&'static mut ListNode, usize)> {
        let mut current = &mut self.head;
        while let Some(ref mut region) = current.next {
            if let Ok(alloc_start) = Self::alloc_from_region(region, size, align) {
                let next = region.next.take();
                let ret = Some((current.next.take().unwrap(), alloc_start));
                current.next = next;
                return ret;
            } else {
                current = current.next.as_mut().unwrap();
            }
        }
        None
    }

    /// Check whether `region` can satisfy an allocation of `size`/`align`,
    /// rejecting it if the leftover space would be nonzero but too small
    /// to host a `ListNode` (we'd otherwise leak that tail forever). The
    /// gap alignment leaves in front has to be able to host one too.
    fn alloc_from_region(region: &ListNode, size: usize, align: usize) -> Result<usize, ()> {
        let start = region.start_addr();
        let mut alloc_start = align_up(start, align);
        if alloc_start != start && alloc_start - start < mem::size_of::<ListNode>() {
            alloc_start = align_up(start + mem::size_of::<ListNode>(), align);
        }
        let alloc_end = alloc_start.checked_add(size).ok_or(())?;

        if alloc_end > region.end_addr() {
            return Err(());
        }

        let excess_size = region.end_addr() - alloc_end;
        if excess_size > 0 && excess_size < mem::size_of::<ListNode>() {
            return Err(());
        }

        Ok(alloc_start)
    }

    /// Adjust a `Layout` so the eventual allocation is also big enough
    /// and aligned enough to host a `ListNode` once it's freed again.
    fn size_align(layout: Layout) -> (usize, usize) {
        let layout = layout
            .align_to(mem::align_of::<ListNode>())
            .expect("adjusting alignment failed")
            .pad_to_align();
        let size = layout.size().max(mem::size_of::<ListNode>());
        (size, layout.align())
    }
}

unsafe impl GlobalAlloc for Spinlock<LinkedListAllocator> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let (size, align) = LinkedListAllocator::size_align(layout);
        let mut allocator = self.lock();

        if let Some((region, alloc_start)) = allocator.find_region(size, align) {
            let alloc_end = alloc_start.checked_add(size).expect("overflow");
            let (region_start, region_end) = (region.start_addr(), region.end_addr());
            if region_end > alloc_end {
                allocator.add_free_region(alloc_end, region_end - alloc_end);
            }
            if alloc_start > region_start {
                allocator.add_free_region(region_start, alloc_start - region_start);
            }
            alloc_start as *mut u8
        } else {
            ptr::null_mut()
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let (size, _) = LinkedListAllocator::size_align(layout);
        self.lock().add_free_region(ptr as usize, size);
    }
}

/// Round `addr` up to the next multiple of `align` (`align` must be a
/// power of two).
fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}
