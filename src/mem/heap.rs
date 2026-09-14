//! Kernel heap allocator, wired up as `#[global_allocator]`.
//!
//! There's no physical frame allocator or paging management yet (see
//! `todo.md`), so the heap is just a fixed-size region reserved in
//! `.bss` — the same trick `lib.rs` uses for `USER_STACK`. It's backed by
//! a hand-rolled intrusive free-list allocator (first-fit, no
//! coalescing on free): enough to unblock `alloc::{boxed::Box,
//! vec::Vec, ...}` before real memory management exists.

use crate::sync::Spinlock;
use core::alloc::{GlobalAlloc, Layout};
use core::mem;
use core::ptr;

/// Heap size in bytes. Arbitrary — we can't yet ask multiboot2 how much
/// RAM is actually available, so this is just "comfortably small".
pub const HEAP_SIZE: usize = 256 * 1024; // 256 KiB

#[repr(align(16))]
struct HeapMemory([u8; HEAP_SIZE]);

static mut HEAP_MEMORY: HeapMemory = HeapMemory([0; HEAP_SIZE]);

#[global_allocator]
static ALLOCATOR: Spinlock<LinkedListAllocator> = Spinlock::new(LinkedListAllocator::new());

/// Hand the allocator its backing region.
///
/// Call once from `kernel_main`, before any code uses `alloc::{Box, Vec,
/// ...}` — allocations before this point just get a null pointer back.
pub fn init() {
    unsafe {
        let heap_start = core::ptr::addr_of!(HEAP_MEMORY.0) as usize;
        ALLOCATOR.lock().init(heap_start, HEAP_SIZE);
    }
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
/// points at the first real free block; the list is otherwise unordered.
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

    /// Push a region of memory onto the front of the free list.
    ///
    /// # Safety
    /// The region must be valid, unused, correctly aligned for
    /// `ListNode`, and at least `size_of::<ListNode>()` bytes.
    unsafe fn add_free_region(&mut self, addr: usize, size: usize) {
        assert_eq!(align_up(addr, mem::align_of::<ListNode>()), addr);
        assert!(size >= mem::size_of::<ListNode>());

        let mut node = ListNode::new(size);
        node.next = self.head.next.take();
        let node_ptr = addr as *mut ListNode;
        node_ptr.write(node);
        self.head.next = Some(&mut *node_ptr);
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
    /// to host a `ListNode` (we'd otherwise leak that tail forever).
    fn alloc_from_region(region: &ListNode, size: usize, align: usize) -> Result<usize, ()> {
        let alloc_start = align_up(region.start_addr(), align);
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
            let excess_size = region.end_addr() - alloc_end;
            if excess_size > 0 {
                allocator.add_free_region(alloc_end, excess_size);
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
