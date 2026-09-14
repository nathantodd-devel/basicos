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
