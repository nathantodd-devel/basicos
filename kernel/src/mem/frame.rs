//! Physical frame allocator.
//!
//! Uses a bitmap to track which 4 KiB frames are free. The bitmap itself
//! lives in a static array — we can't use the heap since the heap allocator
//! might eventually need to allocate frames.

use crate::sync::Spinlock;
use core::sync::atomic::{AtomicBool, Ordering};

use super::multiboot2::MemoryMap;

/// Page/frame size: 4 KiB.
pub const FRAME_SIZE: usize = 4096;

/// Maximum physical memory we support (1 GiB for now — matches the identity map).
/// This determines the size of our bitmap.
const MAX_PHYS_MEMORY: usize = 1024 * 1024 * 1024; // 1 GiB

/// Number of frames we can track.
const MAX_FRAMES: usize = MAX_PHYS_MEMORY / FRAME_SIZE;

/// Bits per bitmap entry.
const BITS_PER_ENTRY: usize = 64;

/// Number of u64s needed for the bitmap.
const BITMAP_SIZE: usize = MAX_FRAMES / BITS_PER_ENTRY;

/// A physical frame number (PFN). Frame N covers addresses [N*4096, (N+1)*4096).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Frame(usize);

impl Frame {
    /// Create a frame from a frame number.
    pub const fn from_number(n: usize) -> Self {
        Frame(n)
    }

    /// Create a frame from a physical address (must be frame-aligned).
    pub const fn containing_address(addr: usize) -> Self {
        Frame(addr / FRAME_SIZE)
    }

    /// The frame number.
    pub const fn number(&self) -> usize {
        self.0
    }

    /// Start physical address of this frame.
    pub const fn start_address(&self) -> usize {
        self.0 * FRAME_SIZE
    }

    /// End physical address (exclusive) of this frame.
    pub const fn end_address(&self) -> usize {
        (self.0 + 1) * FRAME_SIZE
    }
}

/// Bitmap-based frame allocator.
///
/// Each bit represents one 4 KiB frame: 0 = free, 1 = allocated.
struct BitmapAllocator {
    /// The bitmap. A 1 bit means the frame is in use.
    bitmap: [u64; BITMAP_SIZE],
    /// Number of free frames.
    free_count: usize,
    /// Total frames in the system (based on physical memory size).
    total_frames: usize,
    /// Next frame to check when allocating (simple optimization).
    next_free_hint: usize,
}

impl BitmapAllocator {
    const fn new() -> Self {
        Self {
            // Start with all frames marked as used (safe default).
            bitmap: [!0u64; BITMAP_SIZE],
            free_count: 0,
            total_frames: 0,
            next_free_hint: 0,
        }
    }

    /// Initialize the allocator from the memory map.
    ///
    /// Marks all frames as used initially, then frees frames in available regions,
    /// then re-marks the kernel region as used.
    fn init(&mut self, memory_map: &MemoryMap, kernel_start: usize, kernel_end: usize) {
        // Find the highest available address to determine total frames
        let max_addr = memory_map.max_available_addr() as usize;
        self.total_frames = (max_addr / FRAME_SIZE).min(MAX_FRAMES);

        // Mark everything as used initially
        for entry in &mut self.bitmap {
            *entry = !0u64;
        }
        self.free_count = 0;

        // Free frames in available regions
        for region in memory_map.available_regions() {
            let start_frame = (region.base_addr as usize + FRAME_SIZE - 1) / FRAME_SIZE;
            let end_frame = (region.end_addr() as usize / FRAME_SIZE).min(MAX_FRAMES);

            for frame_num in start_frame..end_frame {
                self.mark_free(frame_num);
            }
        }

        // Re-mark kernel region as used
        let kernel_start_frame = kernel_start / FRAME_SIZE;
        let kernel_end_frame = (kernel_end + FRAME_SIZE - 1) / FRAME_SIZE;
        for frame_num in kernel_start_frame..kernel_end_frame {
            self.mark_used(frame_num);
        }

        // Also reserve the first 1 MiB (low memory, BIOS, VGA, etc.)
        let low_mem_frames = (1024 * 1024) / FRAME_SIZE;
        for frame_num in 0..low_mem_frames {
            self.mark_used(frame_num);
        }

        self.next_free_hint = low_mem_frames;
    }

    /// Mark a frame as free (set bit to 0).
    fn mark_free(&mut self, frame_num: usize) {
        if frame_num >= MAX_FRAMES {
            return;
        }
        let idx = frame_num / BITS_PER_ENTRY;
        let bit = frame_num % BITS_PER_ENTRY;

        if self.bitmap[idx] & (1 << bit) != 0 {
            self.bitmap[idx] &= !(1 << bit);
            self.free_count += 1;
        }
    }

    /// Mark a frame as used (set bit to 1).
    fn mark_used(&mut self, frame_num: usize) {
        if frame_num >= MAX_FRAMES {
            return;
        }
        let idx = frame_num / BITS_PER_ENTRY;
        let bit = frame_num % BITS_PER_ENTRY;

        if self.bitmap[idx] & (1 << bit) == 0 {
            self.bitmap[idx] |= 1 << bit;
            self.free_count -= 1;
        }
    }

    /// Check if a frame is free.
    fn is_free(&self, frame_num: usize) -> bool {
        if frame_num >= MAX_FRAMES {
            return false;
        }
        let idx = frame_num / BITS_PER_ENTRY;
        let bit = frame_num % BITS_PER_ENTRY;
        self.bitmap[idx] & (1 << bit) == 0
    }

    /// Allocate a single frame. Returns `None` if no frames available.
    fn allocate(&mut self) -> Option<Frame> {
        if self.free_count == 0 {
            return None;
        }

        // Start searching from the hint
        let start = self.next_free_hint / BITS_PER_ENTRY;
        let total_entries = (self.total_frames + BITS_PER_ENTRY - 1) / BITS_PER_ENTRY;

        for i in 0..total_entries {
            let idx = (start + i) % total_entries;
            let entry = self.bitmap[idx];

            // Skip if all bits are set (all frames used)
            if entry == !0u64 {
                continue;
            }

            // Find first zero bit
            let bit = (!entry).trailing_zeros() as usize;
            let frame_num = idx * BITS_PER_ENTRY + bit;

            if frame_num < self.total_frames {
                self.mark_used(frame_num);
                self.next_free_hint = frame_num + 1;
                return Some(Frame::from_number(frame_num));
            }
        }

        None
    }

    /// Free a previously allocated frame.
    fn deallocate(&mut self, frame: Frame) {
        let frame_num = frame.number();
        if frame_num < self.total_frames {
            self.mark_free(frame_num);
            // Update hint if this frame is earlier
            if frame_num < self.next_free_hint {
                self.next_free_hint = frame_num;
            }
        }
    }

    /// Allocate a contiguous range of frames.
    fn allocate_contiguous(&mut self, count: usize) -> Option<Frame> {
        if count == 0 || self.free_count < count {
            return None;
        }

        // Linear scan for contiguous free frames
        let mut run_start = 0;
        let mut run_length = 0;

        for frame_num in 0..self.total_frames {
            if self.is_free(frame_num) {
                if run_length == 0 {
                    run_start = frame_num;
                }
                run_length += 1;

                if run_length == count {
                    // Found a run! Mark all frames as used.
                    for i in 0..count {
                        self.mark_used(run_start + i);
                    }
                    return Some(Frame::from_number(run_start));
                }
            } else {
                run_length = 0;
            }
        }

        None
    }

    /// Number of free frames.
    fn free_count(&self) -> usize {
        self.free_count
    }

    /// Total frames being tracked.
    fn total_frames(&self) -> usize {
        self.total_frames
    }
}

/// Global frame allocator instance.
static FRAME_ALLOCATOR: Spinlock<BitmapAllocator> = Spinlock::new(BitmapAllocator::new());

/// Whether the frame allocator has been initialized.
static INITIALIZED: AtomicBool = AtomicBool::new(false);

// Linker symbols for kernel boundaries
extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

/// Initialize the frame allocator from the multiboot2 memory map.
///
/// Must be called once, early in boot, after parsing the memory map.
pub fn init(memory_map: &MemoryMap) {
    // Get kernel boundaries from linker symbols
    let kernel_start = unsafe { &__kernel_start as *const u8 as usize };
    let kernel_end = unsafe { &__kernel_end as *const u8 as usize };

    FRAME_ALLOCATOR.lock().init(memory_map, kernel_start, kernel_end);
    INITIALIZED.store(true, Ordering::Release);
}

/// Initialize with explicit kernel boundaries (for when linker symbols aren't set up).
pub fn init_with_kernel_bounds(memory_map: &MemoryMap, kernel_start: usize, kernel_end: usize) {
    FRAME_ALLOCATOR
        .lock()
        .init(memory_map, kernel_start, kernel_end);
    INITIALIZED.store(true, Ordering::Release);
}

/// Reserve an arbitrary physical address range, marking its frames used
/// so the allocator never hands them out.
///
/// For anything the allocator doesn't already know to protect — e.g. a
/// multiboot2 module (the initrd) sitting in otherwise-"available" RAM.
/// Call this before anything else has a chance to allocate frames that
/// might overlap it.
pub fn reserve_range(start_addr: usize, end_addr: usize) {
    let mut allocator = FRAME_ALLOCATOR.lock();
    let start_frame = start_addr / FRAME_SIZE;
    let end_frame = (end_addr + FRAME_SIZE - 1) / FRAME_SIZE;
    for frame_num in start_frame..end_frame {
        allocator.mark_used(frame_num);
    }
}

/// Allocate a single physical frame.
pub fn allocate() -> Option<Frame> {
    debug_assert!(
        INITIALIZED.load(Ordering::Acquire),
        "frame allocator not initialized"
    );
    FRAME_ALLOCATOR.lock().allocate()
}

/// Free a previously allocated frame.
pub fn deallocate(frame: Frame) {
    FRAME_ALLOCATOR.lock().deallocate(frame);
}

/// Allocate `count` contiguous physical frames.
pub fn allocate_contiguous(count: usize) -> Option<Frame> {
    debug_assert!(
        INITIALIZED.load(Ordering::Acquire),
        "frame allocator not initialized"
    );
    FRAME_ALLOCATOR.lock().allocate_contiguous(count)
}

/// Number of free frames available.
pub fn free_count() -> usize {
    FRAME_ALLOCATOR.lock().free_count()
}

/// Total frames being managed.
pub fn total_frames() -> usize {
    FRAME_ALLOCATOR.lock().total_frames()
}

/// Check if the frame allocator has been initialized.
pub fn is_initialized() -> bool {
    INITIALIZED.load(Ordering::Acquire)
}
