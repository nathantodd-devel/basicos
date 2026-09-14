//! Fixed-size circular buffers.
//!
//! Provides two types:
//! - `LogRingBuffer`: Overwrites oldest data when full (for kernel log)
//! - `RingBuffer`: Queue-style with try_push/try_pop (for I/O buffers)

use core::sync::atomic::{AtomicUsize, Ordering};

/// Fixed-size ring buffer that overwrites oldest data when full.
///
/// `N` must be a power of 2 for efficient modulo via bitmask.
/// Used for kernel log storage (similar to Linux's dmesg buffer).
pub struct LogRingBuffer<const N: usize> {
    buffer: [u8; N],
    write_pos: usize,
    total_written: usize,
}

impl<const N: usize> LogRingBuffer<N> {
    /// Create a new empty ring buffer.
    pub const fn new() -> Self {
        Self {
            buffer: [0; N],
            write_pos: 0,
            total_written: 0,
        }
    }

    /// Write bytes to the ring buffer, overwriting oldest data if full.
    pub fn write(&mut self, data: &[u8]) {
        for &byte in data {
            self.buffer[self.write_pos] = byte;
            self.write_pos = (self.write_pos + 1) & (N - 1);
            self.total_written = self.total_written.saturating_add(1);
        }
    }

    /// Write a single byte to the ring buffer.
    #[inline]
    #[allow(dead_code)]
    pub fn write_byte(&mut self, byte: u8) {
        self.buffer[self.write_pos] = byte;
        self.write_pos = (self.write_pos + 1) & (N - 1);
        self.total_written = self.total_written.saturating_add(1);
    }

    /// Read all available bytes, calling the callback for each byte.
    ///
    /// Bytes are delivered in order from oldest to newest.
    /// Returns the number of bytes read.
    pub fn read_all<F: FnMut(u8)>(&self, mut f: F) -> usize {
        let available = self.len();

        if available == 0 {
            return 0;
        }

        // Calculate start position (oldest data)
        let start = if self.total_written >= N {
            // Buffer has wrapped — oldest is at write_pos
            self.write_pos
        } else {
            // Buffer hasn't wrapped — oldest is at 0
            0
        };

        for i in 0..available {
            let pos = (start + i) & (N - 1);
            f(self.buffer[pos]);
        }

        available
    }

    /// Returns the number of valid bytes in the buffer.
    #[inline]
    pub fn len(&self) -> usize {
        core::cmp::min(self.total_written, N)
    }

    /// Returns true if the buffer is empty.
    #[inline]
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.total_written == 0
    }

    /// Returns the total number of bytes ever written (for sequence tracking).
    #[inline]
    #[allow(dead_code)]
    pub fn total_written(&self) -> usize {
        self.total_written
    }

    /// Clear the buffer.
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.write_pos = 0;
        self.total_written = 0;
    }
}

impl<const N: usize> Default for LogRingBuffer<N> {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Queue-style RingBuffer for I/O
// =============================================================================

/// Lock-free single-producer single-consumer ring buffer.
///
/// Suitable for interrupt handlers (producer) and main code (consumer).
/// `N` must be a power of 2.
pub struct RingBuffer<T, const N: usize> {
    buffer: [core::mem::MaybeUninit<T>; N],
    head: AtomicUsize, // Next position to read from
    tail: AtomicUsize, // Next position to write to
}

// Safety: The buffer uses atomics for synchronization and is designed for SPSC use.
unsafe impl<T: Send, const N: usize> Sync for RingBuffer<T, N> {}

impl<T: Copy, const N: usize> RingBuffer<T, N> {
    /// Create a new empty ring buffer.
    pub const fn new() -> Self {
        Self {
            buffer: unsafe { core::mem::MaybeUninit::uninit().assume_init() },
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Try to push an item. Returns `Err(item)` if the buffer is full.
    pub fn try_push(&self, item: T) -> Result<(), T> {
        let tail = self.tail.load(Ordering::Relaxed);
        let next_tail = (tail + 1) & (N - 1);

        if next_tail == self.head.load(Ordering::Acquire) {
            return Err(item); // Buffer full
        }

        // Safety: We have exclusive write access to this slot
        unsafe {
            let ptr = self.buffer.as_ptr().add(tail) as *mut T;
            ptr.write(item);
        }

        self.tail.store(next_tail, Ordering::Release);
        Ok(())
    }

    /// Try to pop an item. Returns `None` if the buffer is empty.
    pub fn try_pop(&self) -> Option<T> {
        let head = self.head.load(Ordering::Relaxed);

        if head == self.tail.load(Ordering::Acquire) {
            return None; // Buffer empty
        }

        // Safety: We have exclusive read access to this slot
        let item = unsafe {
            let ptr = self.buffer.as_ptr().add(head) as *const T;
            ptr.read()
        };

        let next_head = (head + 1) & (N - 1);
        self.head.store(next_head, Ordering::Release);

        Some(item)
    }

    /// Check if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.head.load(Ordering::Acquire) == self.tail.load(Ordering::Acquire)
    }

    /// Check if the buffer is full.
    #[allow(dead_code)]
    pub fn is_full(&self) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        let next_tail = (tail + 1) & (N - 1);
        next_tail == self.head.load(Ordering::Acquire)
    }

    /// Returns the number of items in the buffer.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Acquire);
        (tail.wrapping_sub(head)) & (N - 1)
    }
}
