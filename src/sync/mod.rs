//! Synchronization primitives and data structures.

pub mod ringbuf;
pub mod spinlock;

pub use ringbuf::LogRingBuffer;
#[cfg(any(feature = "keyboard", feature = "serial-input"))]
pub use ringbuf::RingBuffer;
#[allow(unused_imports)]
pub use spinlock::{Spinlock, SpinlockGuard};
