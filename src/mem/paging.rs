//! Page table management for x86_64.
//!
//! Provides utilities for mapping and unmapping virtual addresses,
//! manipulating page tables, and managing the virtual address space.

use super::frame::{allocate, deallocate, Frame, FRAME_SIZE};
use crate::sync::Spinlock;
use core::ptr;

/// Page size (same as frame size): 4 KiB.
pub const PAGE_SIZE: usize = FRAME_SIZE;

/// Number of entries in a page table.
const ENTRIES_PER_TABLE: usize = 512;

// --- the process-private zone --------------------------------------------
//
// `entrypoint.s` identity-maps the first 1 GiB with 2 MiB huge pages — one
// P2 entry per 2 MiB, no P1 (page table) level at all underneath. That's
// fine for a kernel with a single shared address space, but it means
// there's no granularity to give two processes *different* physical
// backing at the same virtual address, which is the whole point of a
// process having its own address space.
//
// `demote_process_zone()` fixes that for one fixed 16 MiB slice, turning
// its huge-page P2 entries into real P1 tables (preserving the exact same
// identity mapping — purely structural, nothing moves). Every process's
// own address space (`PageMapper::new_process_space`/`fork_from`) then
// gets its *own* private P1 tables for just this slice, while everything
// else in the 1 GiB range — kernel code, the heap, page tables themselves
// — stays the single shared mapping it always was, copied by reference
// (literally the same P2 entries) into every process's table.
//
// The physical range under this slice is reserved from the frame
// allocator (see `lib.rs`), so `frame::allocate()` never hands out a
// frame that falls inside it — every frame used to back a process's own
// pages therefore comes from *outside* the slice, where the identity
// mapping is untouched and universally shared. That's what lets kernel
// code safely `memcpy` into a freshly allocated frame via its raw
// physical address no matter whose CR3 happens to be loaded (see
// `PageMapper::clone_from_template`).
//
// Layout within the zone (all page-aligned, chosen to fit comfortably
// inside the 1 GiB/frame-allocator-tracked range even under a modest
// test VM):
//   0x600000 (6 MiB)  CODE   4 MiB  — ELF image (matches testprog.ld)
//   0xA00000 (10 MiB) HEAP   5 MiB  — grows up via brk()
//   0xF00000 (15 MiB) MMAP   6 MiB  — bump-allocated via mmap()
//   0x1500000 (21 MiB) STACK 1 MiB — grows down
pub const PROC_ZONE_START: usize = 0x600000;
pub const PROC_CODE_START: usize = PROC_ZONE_START;
pub const PROC_CODE_SIZE: usize = 4 * 1024 * 1024;
pub const PROC_HEAP_START: usize = PROC_CODE_START + PROC_CODE_SIZE;
pub const PROC_HEAP_SIZE: usize = 5 * 1024 * 1024;
pub const PROC_MMAP_START: usize = PROC_HEAP_START + PROC_HEAP_SIZE;
pub const PROC_MMAP_SIZE: usize = 6 * 1024 * 1024;
pub const PROC_STACK_START: usize = PROC_MMAP_START + PROC_MMAP_SIZE;
pub const PROC_STACK_SIZE: usize = 1024 * 1024;
pub const PROC_ZONE_END: usize = PROC_STACK_START + PROC_STACK_SIZE;
pub const PROC_ZONE_SIZE: usize = PROC_ZONE_END - PROC_ZONE_START;

const _: () = assert!(PROC_ZONE_START % (2 * 1024 * 1024) == 0);
const _: () = assert!(PROC_ZONE_END % (2 * 1024 * 1024) == 0);

/// Page table entry flags.
pub mod flags {
    pub const PRESENT: u64 = 1 << 0;
    pub const WRITABLE: u64 = 1 << 1;
    pub const USER_ACCESSIBLE: u64 = 1 << 2;
    pub const WRITE_THROUGH: u64 = 1 << 3;
    pub const NO_CACHE: u64 = 1 << 4;
    pub const ACCESSED: u64 = 1 << 5;
    pub const DIRTY: u64 = 1 << 6;
    pub const HUGE_PAGE: u64 = 1 << 7;
    pub const GLOBAL: u64 = 1 << 8;
    pub const NO_EXECUTE: u64 = 1 << 63;

    /// Default flags for kernel pages: present, writable, no user access.
    pub const KERNEL_PAGE: u64 = PRESENT | WRITABLE;

    /// Default flags for user pages: present, writable, user accessible.
    pub const USER_PAGE: u64 = PRESENT | WRITABLE | USER_ACCESSIBLE;

    /// Flags for read-only kernel pages.
    pub const KERNEL_READONLY: u64 = PRESENT;

    /// Flags for read-only user pages.
    pub const USER_READONLY: u64 = PRESENT | USER_ACCESSIBLE;
}

/// A page table entry.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct PageTableEntry(u64);

impl PageTableEntry {
    /// Create an empty (not present) entry.
    pub const fn empty() -> Self {
        PageTableEntry(0)
    }

    /// Create an entry pointing to a frame with given flags.
    pub const fn new(frame: Frame, flags: u64) -> Self {
        PageTableEntry((frame.start_address() as u64) | flags)
    }

    /// Check if the entry is present.
    pub const fn is_present(&self) -> bool {
        self.0 & flags::PRESENT != 0
    }

    /// Check if this is a huge page entry (2 MiB or 1 GiB).
    pub const fn is_huge(&self) -> bool {
        self.0 & flags::HUGE_PAGE != 0
    }

    /// Get the physical address this entry points to.
    pub const fn address(&self) -> u64 {
        self.0 & 0x000F_FFFF_FFFF_F000
    }

    /// Get the frame this entry points to.
    pub const fn frame(&self) -> Frame {
        Frame::containing_address(self.address() as usize)
    }

    /// Get the flags of this entry.
    pub const fn flags(&self) -> u64 {
        self.0 & 0xFFF0_0000_0000_0FFF
    }

    /// Set the entry.
    pub fn set(&mut self, frame: Frame, flags: u64) {
        self.0 = (frame.start_address() as u64) | flags;
    }

    /// Clear the entry (mark as not present).
    pub fn clear(&mut self) {
        self.0 = 0;
    }

    /// Set flags without changing the address.
    pub fn set_flags(&mut self, flags: u64) {
        self.0 = (self.0 & 0x000F_FFFF_FFFF_F000) | flags;
    }
}

/// A page table (array of 512 entries).
#[repr(align(4096))]
pub struct PageTable {
    entries: [PageTableEntry; ENTRIES_PER_TABLE],
}

impl PageTable {
    /// Create an empty page table.
    pub const fn new() -> Self {
        Self {
            entries: [PageTableEntry::empty(); ENTRIES_PER_TABLE],
        }
    }

    /// Get a reference to an entry.
    pub fn entry(&self, index: usize) -> &PageTableEntry {
        &self.entries[index]
    }

    /// Get a mutable reference to an entry.
    pub fn entry_mut(&mut self, index: usize) -> &mut PageTableEntry {
        &mut self.entries[index]
    }

    /// Zero all entries.
    pub fn zero(&mut self) {
        for entry in &mut self.entries {
            entry.clear();
        }
    }
}

impl Default for PageTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Virtual address decomposition for 4-level paging.
#[derive(Debug, Clone, Copy)]
pub struct VirtualAddress(u64);

impl VirtualAddress {
    pub const fn new(addr: u64) -> Self {
        VirtualAddress(addr)
    }

    /// P4 (PML4) index: bits 39-47.
    pub const fn p4_index(&self) -> usize {
        ((self.0 >> 39) & 0x1FF) as usize
    }

    /// P3 (PDPT) index: bits 30-38.
    pub const fn p3_index(&self) -> usize {
        ((self.0 >> 30) & 0x1FF) as usize
    }

    /// P2 (PD) index: bits 21-29.
    pub const fn p2_index(&self) -> usize {
        ((self.0 >> 21) & 0x1FF) as usize
    }

    /// P1 (PT) index: bits 12-20.
    pub const fn p1_index(&self) -> usize {
        ((self.0 >> 12) & 0x1FF) as usize
    }

    /// Page offset: bits 0-11.
    pub const fn page_offset(&self) -> usize {
        (self.0 & 0xFFF) as usize
    }

    /// The raw address value.
    pub const fn as_u64(&self) -> u64 {
        self.0
    }
}

/// Read CR3 (the physical address of the P4 table).
pub fn read_cr3() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!("mov {}, cr3", out(reg) value, options(nomem, nostack));
    }
    value
}

/// Write CR3 (switch to a different P4 table).
///
/// # Safety
/// The address must point to a valid P4 table.
pub unsafe fn write_cr3(addr: u64) {
    core::arch::asm!("mov cr3, {}", in(reg) addr, options(nostack));
}

/// Flush a single TLB entry for the given virtual address.
pub fn flush_tlb(addr: u64) {
    unsafe {
        core::arch::asm!("invlpg [{}]", in(reg) addr, options(nostack));
    }
}

/// Flush the entire TLB by reloading CR3.
pub fn flush_tlb_all() {
    let cr3 = read_cr3();
    unsafe {
        write_cr3(cr3);
    }
}

/// Page mapper for the kernel's address space.
///
/// Currently assumes identity mapping (virtual == physical for kernel space).
pub struct PageMapper {
    p4_physical: u64,
}

/// Get or create a page table at the next level.
///
/// # Safety
/// Assumes identity mapping.
unsafe fn next_table_create(entry: &mut PageTableEntry, flags: u64) -> &mut PageTable {
    if !entry.is_present() {
        // Allocate a new frame for the page table
        let frame = allocate().expect("out of memory for page tables");
        entry.set(frame, flags);

        // Zero the new table
        let table = &mut *(frame.start_address() as *mut PageTable);
        table.zero();
        table
    } else {
        &mut *(entry.address() as *mut PageTable)
    }
}

impl PageMapper {
    /// Create a mapper for the current address space.
    pub fn current() -> Self {
        Self {
            p4_physical: read_cr3() & 0x000F_FFFF_FFFF_F000,
        }
    }

    /// Get a reference to the P4 table.
    ///
    /// # Safety
    /// Assumes identity mapping.
    unsafe fn p4(&self) -> &PageTable {
        &*(self.p4_physical as *const PageTable)
    }

    /// Get a mutable reference to the P4 table.
    ///
    /// # Safety
    /// Assumes identity mapping.
    unsafe fn p4_mut(&mut self) -> &mut PageTable {
        &mut *(self.p4_physical as *mut PageTable)
    }

    /// Map a virtual address to a physical frame.
    ///
    /// # Safety
    /// - The virtual address must be page-aligned.
    /// - The frame must be valid.
    /// - Assumes identity mapping for page table access.
    pub unsafe fn map(&mut self, virt: VirtualAddress, frame: Frame, flags: u64) -> Result<(), MapError> {
        let parent_flags = flags::PRESENT | flags::WRITABLE | (flags & flags::USER_ACCESSIBLE);

        let p4 = self.p4_mut();
        let p3 = next_table_create(p4.entry_mut(virt.p4_index()), parent_flags);
        let p2 = next_table_create(p3.entry_mut(virt.p3_index()), parent_flags);
        let p1 = next_table_create(p2.entry_mut(virt.p2_index()), parent_flags);

        let entry = p1.entry_mut(virt.p1_index());
        if entry.is_present() {
            return Err(MapError::AlreadyMapped);
        }

        entry.set(frame, flags);
        flush_tlb(virt.as_u64());

        Ok(())
    }

    /// Unmap a virtual address.
    ///
    /// Returns the frame that was mapped there.
    ///
    /// # Safety
    /// Assumes identity mapping for page table access.
    pub unsafe fn unmap(&mut self, virt: VirtualAddress) -> Result<Frame, MapError> {
        let p4 = self.p4_mut();
        let p4_entry = p4.entry(virt.p4_index());
        if !p4_entry.is_present() {
            return Err(MapError::NotMapped);
        }

        let p3 = &mut *(p4_entry.address() as *mut PageTable);
        let p3_entry = p3.entry(virt.p3_index());
        if !p3_entry.is_present() {
            return Err(MapError::NotMapped);
        }

        let p2 = &mut *(p3_entry.address() as *mut PageTable);
        let p2_entry = p2.entry(virt.p2_index());
        if !p2_entry.is_present() {
            return Err(MapError::NotMapped);
        }

        let p1 = &mut *(p2_entry.address() as *mut PageTable);
        let entry = p1.entry_mut(virt.p1_index());
        if !entry.is_present() {
            return Err(MapError::NotMapped);
        }

        let frame = entry.frame();
        entry.clear();
        flush_tlb(virt.as_u64());

        Ok(frame)
    }

    /// Translate a virtual address to a physical address.
    ///
    /// # Safety
    /// Assumes identity mapping for page table access.
    pub unsafe fn translate(&self, virt: VirtualAddress) -> Option<u64> {
        let p4 = self.p4();
        let p4_entry = p4.entry(virt.p4_index());
        if !p4_entry.is_present() {
            return None;
        }

        let p3 = &*(p4_entry.address() as *const PageTable);
        let p3_entry = p3.entry(virt.p3_index());
        if !p3_entry.is_present() {
            return None;
        }
        // Check for 1 GiB huge page
        if p3_entry.is_huge() {
            let offset = virt.as_u64() & 0x3FFFFFFF; // 30-bit offset
            return Some(p3_entry.address() + offset);
        }

        let p2 = &*(p3_entry.address() as *const PageTable);
        let p2_entry = p2.entry(virt.p2_index());
        if !p2_entry.is_present() {
            return None;
        }
        // Check for 2 MiB huge page
        if p2_entry.is_huge() {
            let offset = virt.as_u64() & 0x1FFFFF; // 21-bit offset
            return Some(p2_entry.address() + offset);
        }

        let p1 = &*(p2_entry.address() as *const PageTable);
        let p1_entry = p1.entry(virt.p1_index());
        if !p1_entry.is_present() {
            return None;
        }

        Some(p1_entry.address() + virt.page_offset() as u64)
    }

    /// Map a range of virtual addresses to contiguous physical frames.
    ///
    /// # Safety
    /// Same as `map`.
    pub unsafe fn map_range(
        &mut self,
        virt_start: u64,
        frame_start: Frame,
        count: usize,
        flags: u64,
    ) -> Result<(), MapError> {
        for i in 0..count {
            let virt = VirtualAddress::new(virt_start + (i * PAGE_SIZE) as u64);
            let frame = Frame::from_number(frame_start.number() + i);
            self.map(virt, frame, flags)?;
        }
        Ok(())
    }

    /// Identity map a physical frame (virtual address == physical address).
    ///
    /// # Safety
    /// Same as `map`.
    pub unsafe fn identity_map(&mut self, frame: Frame, flags: u64) -> Result<(), MapError> {
        let virt = VirtualAddress::new(frame.start_address() as u64);
        self.map(virt, frame, flags)
    }

    /// Wrap an already-existing address space (e.g. a process's stored
    /// CR3 value) so it can be operated on with `map`/`unmap`/etc — every
    /// method above works against `self`'s own table hierarchy via raw
    /// physical-address dereferences, not "whatever CR3 happens to be
    /// loaded right now", so this is safe to call for any process
    /// regardless of which address space is currently active.
    pub fn for_address_space(p4_physical: u64) -> Self {
        Self { p4_physical }
    }

    /// This address space's CR3 value.
    pub fn cr3(&self) -> u64 {
        self.p4_physical
    }

    /// Allocate a fresh, zeroed frame and map it at `virt` within *this*
    /// address space.
    ///
    /// # Safety
    /// The virtual address must be page-aligned and not already mapped.
    pub unsafe fn alloc_and_map(&mut self, virt: u64, flags: u64) -> Result<Frame, MapError> {
        let frame = allocate().ok_or(MapError::OutOfFrames)?;
        ptr::write_bytes(frame.start_address() as *mut u8, 0, PAGE_SIZE);
        self.map(VirtualAddress::new(virt), frame, flags)?;
        Ok(frame)
    }

    /// Unmap `virt` within *this* address space and free its frame.
    ///
    /// # Safety
    /// The virtual address must be mapped and its frame must have been
    /// allocated by the frame allocator.
    pub unsafe fn unmap_and_free(&mut self, virt: u64) -> Result<(), MapError> {
        let frame = self.unmap(VirtualAddress::new(virt))?;
        deallocate(frame);
        Ok(())
    }

    /// Build a fresh process address space: identical to the kernel's own
    /// tables everywhere except `PROC_ZONE_START..PROC_ZONE_END`, which
    /// starts out completely unmapped — nothing lives there until the
    /// caller `map()`s something in (loading an ELF, `brk`, `mmap`, the
    /// user stack). Requires `demote_process_zone()` to have already run.
    pub fn new_process_space() -> Self {
        Self::clone_from_template(None)
    }

    /// Build a new process address space that's a deep copy of `parent`'s:
    /// every page currently mapped inside the process zone gets its own
    /// freshly allocated physical frame with the same contents, so the
    /// two can diverge from here on without stepping on each other.
    /// Everything outside the zone (kernel/shared mappings) is copied by
    /// reference, same as `new_process_space`. This is what `fork()` uses.
    pub fn fork_from(parent: &PageMapper) -> Self {
        Self::clone_from_template(Some(parent))
    }

    /// Shared implementation of `new_process_space`/`fork_from`: builds a
    /// new P4/P3/P2 that's a byte-for-byte copy of the kernel's own boot
    /// tables (so every non-zone mapping — kernel code, heap, page tables
    /// — is inherited unchanged), then fills in the process-zone P2
    /// entries either empty (`parent = None`) or as a deep copy of
    /// `parent`'s own zone mappings.
    ///
    /// # Safety (internal invariant, not a `pub unsafe fn`)
    /// Every frame this allocates — for new page tables, and for deep
    /// copies of a parent's data pages — comes from `frame::allocate()`,
    /// which never hands out anything inside the process zone's reserved
    /// physical range (see the module docs). That's what makes it sound
    /// to `memcpy` into these frames via raw physical pointers below,
    /// regardless of which address space is currently loaded in CR3.
    fn clone_from_template(parent: Option<&PageMapper>) -> Self {
        let start_idx = VirtualAddress::new(PROC_ZONE_START as u64).p2_index();
        let end_idx = VirtualAddress::new((PROC_ZONE_END - PAGE_SIZE) as u64).p2_index();

        unsafe {
            let kernel_guard = PAGE_MAPPER.lock();
            let kernel = kernel_guard.as_ref().expect("paging not initialized");
            let kp4 = &*(kernel.p4_physical as *const PageTable);
            let kp3 = &*(kp4.entry(0).address() as *const PageTable);
            let kp2 = &*(kp3.entry(0).address() as *const PageTable);

            let p2_frame = allocate().expect("out of frames for new address space");
            let p2 = &mut *(p2_frame.start_address() as *mut PageTable);
            core::ptr::copy_nonoverlapping(kp2 as *const PageTable, p2 as *mut PageTable, 1);

            match parent {
                None => {
                    for idx in start_idx..=end_idx {
                        p2.entry_mut(idx).clear();
                    }
                }
                Some(parent) => {
                    let pp4 = &*(parent.p4_physical as *const PageTable);
                    let pp3 = &*(pp4.entry(0).address() as *const PageTable);
                    let pp2 = &*(pp3.entry(0).address() as *const PageTable);

                    for idx in start_idx..=end_idx {
                        let parent_entry = pp2.entry(idx);
                        if !parent_entry.is_present() {
                            p2.entry_mut(idx).clear();
                            continue;
                        }
                        let parent_p1 = &*(parent_entry.address() as *const PageTable);
                        let child_p1_frame =
                            allocate().expect("out of frames forking address space");
                        let child_p1 = &mut *(child_p1_frame.start_address() as *mut PageTable);
                        child_p1.zero();
                        for i in 0..ENTRIES_PER_TABLE {
                            let src = parent_p1.entry(i);
                            if !src.is_present() {
                                continue;
                            }
                            let new_frame =
                                allocate().expect("out of frames forking address space");
                            core::ptr::copy_nonoverlapping(
                                src.address() as *const u8,
                                new_frame.start_address() as *mut u8,
                                PAGE_SIZE,
                            );
                            child_p1.entry_mut(i).set(new_frame, src.flags());
                        }
                        p2.entry_mut(idx).set(child_p1_frame, flags::USER_PAGE);
                    }
                }
            }

            let p3_frame = allocate().expect("out of frames for new address space");
            let p3 = &mut *(p3_frame.start_address() as *mut PageTable);
            core::ptr::copy_nonoverlapping(kp3 as *const PageTable, p3 as *mut PageTable, 1);
            p3.entry_mut(0).set(p2_frame, flags::USER_PAGE);

            let p4_frame = allocate().expect("out of frames for new address space");
            let p4 = &mut *(p4_frame.start_address() as *mut PageTable);
            core::ptr::copy_nonoverlapping(kp4 as *const PageTable, p4 as *mut PageTable, 1);
            p4.entry_mut(0).set(p3_frame, flags::USER_PAGE);

            PageMapper {
                p4_physical: p4_frame.start_address() as u64,
            }
        }
    }

    /// Unmap and free every currently-present page inside the process
    /// zone, keeping the address space's own P1/P2/P3/P4 structure
    /// intact (just emptied) so it can be reused immediately — what
    /// `execve()` uses to tear down a process's old image before loading
    /// a new one into the same address space.
    ///
    /// # Safety
    /// Must not be called while any of the unmapped pages are still in
    /// use (e.g. by another task sharing this address space — this
    /// kernel doesn't have those, but if it ever does, this needs to
    /// change).
    pub unsafe fn unmap_zone(&mut self) {
        let start_idx = VirtualAddress::new(PROC_ZONE_START as u64).p2_index();
        let end_idx = VirtualAddress::new((PROC_ZONE_END - PAGE_SIZE) as u64).p2_index();

        let p4 = &*(self.p4_physical as *const PageTable);
        let p3 = &*(p4.entry(0).address() as *const PageTable);
        let p2 = &*(p3.entry(0).address() as *const PageTable);

        for idx in start_idx..=end_idx {
            let entry = p2.entry(idx);
            if !entry.is_present() {
                continue;
            }
            let p1 = &mut *(entry.address() as *mut PageTable);
            for i in 0..ENTRIES_PER_TABLE {
                let e = p1.entry_mut(i);
                if e.is_present() {
                    deallocate(e.frame());
                    e.clear();
                }
            }
        }
        flush_tlb_all();
    }

    /// Free every frame this address space owns exclusively: its own
    /// process-zone data pages and P1 tables, and its own P4/P3/P2. Does
    /// *not* touch anything outside the zone — that's the shared kernel
    /// mapping, owned by nobody in particular.
    ///
    /// # Safety
    /// Nothing may access any address inside this address space's
    /// process zone after this returns. It *is* fine to call this while
    /// `self` is still the address space loaded in CR3 (`exit_process`
    /// does exactly that) — every frame this frees comes from outside
    /// the zone's own reserved physical range or is a table these raw
    /// physical dereferences don't need CR3 for in the first place, and
    /// nothing kernel-side ever touches the zone, so a CR3 that's
    /// nominally "stale" for one more scheduler tick is harmless (see
    /// `sched`'s module docs on why CR3 can lag briefly between
    /// switches).
    pub unsafe fn destroy(self) {
        let start_idx = VirtualAddress::new(PROC_ZONE_START as u64).p2_index();
        let end_idx = VirtualAddress::new((PROC_ZONE_END - PAGE_SIZE) as u64).p2_index();

        let p4 = &*(self.p4_physical as *const PageTable);
        let p3_phys = p4.entry(0).address();
        let p3 = &*(p3_phys as *const PageTable);
        let p2_phys = p3.entry(0).address();
        let p2 = &*(p2_phys as *const PageTable);

        for idx in start_idx..=end_idx {
            let entry = p2.entry(idx);
            if !entry.is_present() {
                continue;
            }
            let p1 = &*(entry.address() as *const PageTable);
            for i in 0..ENTRIES_PER_TABLE {
                let e = p1.entry(i);
                if e.is_present() {
                    deallocate(e.frame());
                }
            }
            deallocate(Frame::containing_address(entry.address() as usize));
        }
        deallocate(Frame::containing_address(p2_phys as usize));
        deallocate(Frame::containing_address(p3_phys as usize));
        deallocate(Frame::containing_address(self.p4_physical as usize));
    }
}

/// Turn the process zone's huge-page P2 entries into real 4 KiB page
/// tables — see the module docs above `PROC_ZONE_START`. Call once, after
/// `init()`, before any process address space is created.
pub fn demote_process_zone() {
    let start_idx = VirtualAddress::new(PROC_ZONE_START as u64).p2_index();
    let end_idx = VirtualAddress::new((PROC_ZONE_END - PAGE_SIZE) as u64).p2_index();

    let mut guard = PAGE_MAPPER.lock();
    let mapper = guard.as_mut().expect("paging not initialized");
    unsafe {
        let p4 = &*(mapper.p4_physical as *const PageTable);
        let p3_phys = p4.entry(0).address();
        let p3 = &*(p3_phys as *const PageTable);
        let p2_phys = p3.entry(0).address();
        let p2 = &mut *(p2_phys as *mut PageTable);

        for idx in start_idx..=end_idx {
            let entry = p2.entry_mut(idx);
            debug_assert!(entry.is_present() && entry.is_huge());
            let base_phys = entry.address();

            let p1_frame = allocate().expect("out of frames demoting process zone");
            let p1 = &mut *(p1_frame.start_address() as *mut PageTable);
            for i in 0..ENTRIES_PER_TABLE {
                let phys = base_phys + (i * PAGE_SIZE) as u64;
                p1.entry_mut(i)
                    .set(Frame::containing_address(phys as usize), flags::USER_PAGE);
            }
            entry.set(p1_frame, flags::USER_PAGE);
        }
    }
    drop(guard);
    flush_tlb_all();
}

/// Errors that can occur during mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// The virtual address is already mapped.
    AlreadyMapped,
    /// The virtual address is not mapped.
    NotMapped,
    /// Out of physical frames for page tables.
    OutOfFrames,
}

/// Global page mapper.
static PAGE_MAPPER: Spinlock<Option<PageMapper>> = Spinlock::new(None);

/// Initialize the paging system.
///
/// Call after frame allocator is initialized.
pub fn init() {
    let mut mapper = PAGE_MAPPER.lock();
    *mapper = Some(PageMapper::current());
}

/// The kernel's own (boot-time) CR3 value — what a plain kernel thread
/// should run under. Every process address space is a clone of this one
/// (see `PageMapper::clone_from_template`), so it's also the structural
/// template for "everything outside the process zone."
pub fn boot_cr3() -> u64 {
    PAGE_MAPPER
        .lock()
        .as_ref()
        .expect("paging not initialized")
        .cr3()
}

/// Map a virtual address to a physical frame.
///
/// # Safety
/// See `PageMapper::map`.
pub unsafe fn map(virt: u64, frame: Frame, flags: u64) -> Result<(), MapError> {
    PAGE_MAPPER
        .lock()
        .as_mut()
        .expect("paging not initialized")
        .map(VirtualAddress::new(virt), frame, flags)
}

/// Unmap a virtual address.
///
/// # Safety
/// See `PageMapper::unmap`.
pub unsafe fn unmap(virt: u64) -> Result<Frame, MapError> {
    PAGE_MAPPER
        .lock()
        .as_mut()
        .expect("paging not initialized")
        .unmap(VirtualAddress::new(virt))
}

/// Translate a virtual address to physical.
pub fn translate(virt: u64) -> Option<u64> {
    unsafe {
        PAGE_MAPPER
            .lock()
            .as_ref()
            .expect("paging not initialized")
            .translate(VirtualAddress::new(virt))
    }
}

/// Allocate and map a new page at the given virtual address.
///
/// # Safety
/// The virtual address must be page-aligned and not already mapped.
pub unsafe fn alloc_and_map(virt: u64, flags: u64) -> Result<Frame, MapError> {
    let frame = allocate().ok_or(MapError::OutOfFrames)?;

    // Zero the frame
    ptr::write_bytes(frame.start_address() as *mut u8, 0, PAGE_SIZE);

    map(virt, frame, flags)?;
    Ok(frame)
}

/// Unmap a page and free its frame.
///
/// # Safety
/// The virtual address must be mapped and the frame must have been
/// allocated by the frame allocator.
pub unsafe fn unmap_and_free(virt: u64) -> Result<(), MapError> {
    let frame = unmap(virt)?;
    deallocate(frame);
    Ok(())
}
