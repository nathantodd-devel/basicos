//! Interrupt-safe spinlock for kernel synchronization.
//!
//! Disables interrupts while held to prevent deadlock when interrupt
//! handlers need to acquire the same lock (like the log buffer).

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// A spinlock that disables interrupts while held.
///
/// This mirrors Linux's `spin_lock_irqsave()` / `spin_unlock_irqrestore()` pattern.
pub struct Spinlock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

// Safety: We ensure exclusive access through the spinlock mechanism
unsafe impl<T: Send> Sync for Spinlock<T> {}
unsafe impl<T: Send> Send for Spinlock<T> {}

impl<T> Spinlock<T> {
    /// Create a new spinlock wrapping the given data.
    pub const fn new(data: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    /// Acquire the spinlock, disabling interrupts.
    ///
    /// Returns a guard that releases the lock and restores interrupt state on drop.
    pub fn lock(&self) -> SpinlockGuard<'_, T> {
        // Save current interrupt state and disable interrupts
        let irq_was_enabled = interrupts_enabled();
        disable_interrupts();

        // Spin until we acquire the lock
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // Hint to the CPU that we're spinning
            core::hint::spin_loop();
        }

        SpinlockGuard {
            lock: self,
            irq_was_enabled,
        }
    }

    /// Try to acquire the lock without blocking.
    ///
    /// Returns `Some(guard)` if successful, `None` if the lock is held.
    #[allow(dead_code)]
    pub fn try_lock(&self) -> Option<SpinlockGuard<'_, T>> {
        let irq_was_enabled = interrupts_enabled();
        disable_interrupts();

        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(SpinlockGuard {
                lock: self,
                irq_was_enabled,
            })
        } else {
            // Failed to acquire — restore interrupt state
            if irq_was_enabled {
                enable_interrupts();
            }
            None
        }
    }
}

/// RAII guard that releases the spinlock and restores interrupt state on drop.
pub struct SpinlockGuard<'a, T> {
    lock: &'a Spinlock<T>,
    irq_was_enabled: bool,
}

impl<T> Deref for SpinlockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // Safety: We hold the lock, so we have exclusive access
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinlockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // Safety: We hold the lock, so we have exclusive access
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinlockGuard<'_, T> {
    fn drop(&mut self) {
        // Release the lock
        self.lock.locked.store(false, Ordering::Release);

        // Restore interrupt state
        if self.irq_was_enabled {
            enable_interrupts();
        }
    }
}

/// Check if interrupts are currently enabled (RFLAGS.IF bit).
#[inline]
fn interrupts_enabled() -> bool {
    let rflags: u64;
    unsafe {
        core::arch::asm!(
            "pushfq",
            "pop {0}",
            out(reg) rflags,
            options(nomem, preserves_flags)
        );
    }
    // IF is bit 9
    (rflags & (1 << 9)) != 0
}

/// Disable interrupts (CLI).
#[inline]
fn disable_interrupts() {
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
    }
}

/// Enable interrupts (STI).
#[inline]
fn enable_interrupts() {
    unsafe {
        core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
    }
}
