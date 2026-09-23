//! Multiboot2 boot information parsing.
//!
//! Parses the memory map and other tags provided by GRUB to discover
//! available RAM regions.

use core::mem;

/// Multiboot2 magic value expected in `eax` on kernel entry.
pub const MULTIBOOT2_BOOTLOADER_MAGIC: u32 = 0x36d76289;

/// Memory map entry types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum MemoryType {
    /// RAM usable by the OS.
    Available = 1,
    /// Reserved or unusable.
    Reserved = 2,
    /// ACPI reclaimable memory.
    AcpiReclaimable = 3,
    /// ACPI NVS memory.
    AcpiNvs = 4,
    /// Bad memory.
    BadMemory = 5,
}

impl MemoryType {
    fn from_u32(value: u32) -> Self {
        match value {
            1 => MemoryType::Available,
            2 => MemoryType::Reserved,
            3 => MemoryType::AcpiReclaimable,
            4 => MemoryType::AcpiNvs,
            5 => MemoryType::BadMemory,
            _ => MemoryType::Reserved,
        }
    }
}

/// A single memory region from the multiboot2 memory map.
#[derive(Debug, Clone, Copy)]
pub struct MemoryRegion {
    pub base_addr: u64,
    pub length: u64,
    pub memory_type: MemoryType,
}

impl MemoryRegion {
    /// Returns true if this region is usable RAM.
    pub fn is_available(&self) -> bool {
        self.memory_type == MemoryType::Available
    }

    /// End address (exclusive) of this region.
    pub fn end_addr(&self) -> u64 {
        self.base_addr + self.length
    }
}

/// Tag types we care about.
const TAG_TYPE_END: u32 = 0;
#[cfg(feature = "storage")]
const TAG_TYPE_MODULE: u32 = 3;
const TAG_TYPE_MMAP: u32 = 6;

/// Fixed-size header at the start of the multiboot2 info structure.
#[repr(C)]
struct BootInfoHeader {
    total_size: u32,
    _reserved: u32,
}

/// Tag header common to all tags.
#[repr(C)]
struct TagHeader {
    tag_type: u32,
    size: u32,
}

/// Memory map tag header.
#[repr(C)]
struct MmapTagHeader {
    tag_type: u32,
    size: u32,
    entry_size: u32,
    entry_version: u32,
}

/// Module tag header (GRUB `module2` directive). A null-terminated
/// cmdline string follows, padded out to `size`; we don't read it.
#[cfg(feature = "storage")]
#[repr(C)]
struct ModuleTagHeader {
    tag_type: u32,
    size: u32,
    mod_start: u32,
    mod_end: u32,
}

/// A boot module loaded by the bootloader alongside the kernel (used for
/// the initrd) — a flat physical address range, nothing more.
#[cfg(feature = "storage")]
#[derive(Debug, Clone, Copy)]
pub struct Module {
    pub start: u32,
    pub end: u32,
}

#[cfg(feature = "storage")]
impl Module {
    pub fn len(&self) -> usize {
        (self.end - self.start) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    /// View the module's bytes.
    ///
    /// # Safety
    /// The caller must have reserved `start..end` in the frame allocator
    /// (`mem::frame::reserve_range`) *before* any other frame allocation
    /// could hand those physical frames out, and the range must fall
    /// inside the kernel's identity-mapped region (true for anything
    /// GRUB loads low in memory, which is all it ever does).
    pub unsafe fn as_slice(&self) -> &'static [u8] {
        core::slice::from_raw_parts(self.start as *const u8, self.len())
    }
}

/// A single memory map entry (as stored in the tag).
#[repr(C)]
struct MmapEntry {
    base_addr: u64,
    length: u64,
    entry_type: u32,
    _reserved: u32,
}

/// Maximum number of memory regions we'll track.
const MAX_REGIONS: usize = 32;

/// Parsed memory map from multiboot2.
pub struct MemoryMap {
    regions: [MemoryRegion; MAX_REGIONS],
    count: usize,
}

impl MemoryMap {
    const fn new() -> Self {
        Self {
            regions: [MemoryRegion {
                base_addr: 0,
                length: 0,
                memory_type: MemoryType::Reserved,
            }; MAX_REGIONS],
            count: 0,
        }
    }

    /// Number of regions in the map.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Returns true if the map is empty.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Iterate over all regions.
    pub fn iter(&self) -> impl Iterator<Item = &MemoryRegion> {
        self.regions[..self.count].iter()
    }

    /// Iterate over available (usable) regions only.
    pub fn available_regions(&self) -> impl Iterator<Item = &MemoryRegion> {
        self.iter().filter(|r| r.is_available())
    }

    /// Total available RAM in bytes.
    pub fn total_available(&self) -> u64 {
        self.available_regions().map(|r| r.length).sum()
    }

    /// Highest address of any available region (useful for sizing the frame bitmap).
    pub fn max_available_addr(&self) -> u64 {
        self.available_regions()
            .map(|r| r.end_addr())
            .max()
            .unwrap_or(0)
    }
}

/// Parse the multiboot2 info structure and extract the memory map.
///
/// # Safety
/// `mb_info_addr` must be the valid physical address passed by GRUB in RSI.
/// The memory at that address must be readable.
pub unsafe fn parse_memory_map(mb_info_addr: u64) -> MemoryMap {
    let mut map = MemoryMap::new();

    let header = &*(mb_info_addr as *const BootInfoHeader);
    let total_size = header.total_size as usize;

    // Walk through tags
    let mut offset = mem::size_of::<BootInfoHeader>();

    while offset < total_size {
        // Tags are 8-byte aligned
        offset = (offset + 7) & !7;

        let tag_ptr = (mb_info_addr as usize + offset) as *const TagHeader;
        let tag = &*tag_ptr;

        if tag.tag_type == TAG_TYPE_END {
            break;
        }

        if tag.tag_type == TAG_TYPE_MMAP {
            let mmap_tag = &*(tag_ptr as *const MmapTagHeader);
            let entry_size = mmap_tag.entry_size as usize;
            let entries_start = tag_ptr as usize + mem::size_of::<MmapTagHeader>();
            let entries_end = tag_ptr as usize + mmap_tag.size as usize;

            let mut entry_offset = entries_start;
            while entry_offset + entry_size <= entries_end && map.count < MAX_REGIONS {
                let entry = &*(entry_offset as *const MmapEntry);
                map.regions[map.count] = MemoryRegion {
                    base_addr: entry.base_addr,
                    length: entry.length,
                    memory_type: MemoryType::from_u32(entry.entry_type),
                };
                map.count += 1;
                entry_offset += entry_size;
            }
        }

        offset += tag.size as usize;
    }

    map
}

/// Find the first module the bootloader loaded (GRUB `module2` in
/// `grub.cfg`) — used as the initrd. Returns `None` if there isn't one.
///
/// # Safety
/// Same requirement as [`parse_memory_map`]: `mb_info_addr` must be the
/// valid, readable physical address GRUB passed in RSI.
#[cfg(feature = "storage")]
pub unsafe fn find_first_module(mb_info_addr: u64) -> Option<Module> {
    let header = &*(mb_info_addr as *const BootInfoHeader);
    let total_size = header.total_size as usize;

    let mut offset = mem::size_of::<BootInfoHeader>();
    while offset < total_size {
        offset = (offset + 7) & !7;

        let tag_ptr = (mb_info_addr as usize + offset) as *const TagHeader;
        let tag = &*tag_ptr;

        if tag.tag_type == TAG_TYPE_END {
            break;
        }

        if tag.tag_type == TAG_TYPE_MODULE {
            let mod_tag = &*(tag_ptr as *const ModuleTagHeader);
            return Some(Module {
                start: mod_tag.mod_start,
                end: mod_tag.mod_end,
            });
        }

        offset += tag.size as usize;
    }

    None
}
