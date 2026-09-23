//! A minimal ELF64 loader: parse an `ET_EXEC` binary and copy its
//! `PT_LOAD` segments into a process's own address space.
//!
//! Still narrow — no relocation, no dynamic linking, `ET_DYN` (PIE) is
//! rejected outright, and every segment has to fit inside the fixed
//! `mem::paging::PROC_CODE_START..+PROC_CODE_SIZE` window (a binary's
//! linker script picks a load address inside that window; see
//! `testprog.ld`). What *isn't* narrow anymore: each process gets its
//! own private physical backing for that virtual window (see
//! `mem::paging`'s module docs on the process zone), so two processes
//! loading the exact same fixed-address binary don't collide — `load_into`
//! writes through the caller-supplied `PageMapper`'s own page-table walk
//! (`translate`), not a raw physical pointer, specifically so this works
//! no matter which address space is currently loaded in CR3.

use crate::fs;
use crate::mem::paging::{self, flags, MapError, PageMapper, VirtualAddress, PAGE_SIZE};

const EI_MAG: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const EI_CLASS_64: u8 = 2;
const EI_DATA_LE: u8 = 1;
const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;

#[repr(C)]
struct Elf64Ehdr {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}

#[repr(C)]
struct Elf64Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

#[derive(Debug, Clone, Copy)]
pub enum ElfError {
    TooShort,
    BadMagic,
    WrongClassOrEndian,
    WrongMachine,
    NotAnExecutable,
    NoProgramHeaders,
    Truncated,
    /// A `PT_LOAD` segment doesn't fit entirely inside
    /// `PROC_CODE_START..+PROC_CODE_SIZE`.
    UnsafeLoadAddress,
    /// Ran out of physical memory while mapping a segment.
    OutOfMemory,
}

#[derive(Debug, Clone, Copy)]
pub enum RunError {
    NotFound,
    Fs(fs::Fat32Error),
    Elf(ElfError),
    /// The scheduler's task table is full.
    NoTaskSlots,
}

/// Parse `data` as an ELF64 `ET_EXEC`/`EM_X86_64` binary and copy its
/// `PT_LOAD` segments into `space`, mapping fresh pages as needed.
/// Returns the entry point.
///
/// # Safety
/// `space` must be a process address space nobody else is concurrently
/// modifying.
pub unsafe fn load_into(space: &mut PageMapper, data: &[u8]) -> Result<u64, ElfError> {
    if data.len() < core::mem::size_of::<Elf64Ehdr>() {
        return Err(ElfError::TooShort);
    }
    // Safety: length checked above; alignment doesn't matter for a
    // packed-by-construction byte-for-byte-matching read like this on
    // x86_64 (unaligned loads are fine), and we only read through
    // `ehdr`, never take a reference that outlives this scope.
    let ehdr = &*(data.as_ptr() as *const Elf64Ehdr);

    if ehdr.e_ident[0..4] != EI_MAG {
        return Err(ElfError::BadMagic);
    }
    if ehdr.e_ident[4] != EI_CLASS_64 || ehdr.e_ident[5] != EI_DATA_LE {
        return Err(ElfError::WrongClassOrEndian);
    }
    if ehdr.e_machine != EM_X86_64 {
        return Err(ElfError::WrongMachine);
    }
    if ehdr.e_type != ET_EXEC {
        // Covers ET_DYN (PIE) too — we don't do relocations.
        return Err(ElfError::NotAnExecutable);
    }
    if ehdr.e_phnum == 0 {
        return Err(ElfError::NoProgramHeaders);
    }

    let zone_start = paging::PROC_CODE_START as u64;
    let zone_end = zone_start + paging::PROC_CODE_SIZE as u64;
    let phentsize = ehdr.e_phentsize as usize;
    let phoff = ehdr.e_phoff as usize;

    for i in 0..ehdr.e_phnum as usize {
        let off = phoff + i * phentsize;
        if off + core::mem::size_of::<Elf64Phdr>() > data.len() {
            return Err(ElfError::Truncated);
        }
        let phdr = &*(data.as_ptr().add(off) as *const Elf64Phdr);
        if phdr.p_type != PT_LOAD {
            continue;
        }

        let seg_end = match phdr.p_vaddr.checked_add(phdr.p_memsz) {
            Some(end) => end,
            None => return Err(ElfError::UnsafeLoadAddress),
        };
        if phdr.p_vaddr < zone_start || seg_end > zone_end {
            return Err(ElfError::UnsafeLoadAddress);
        }

        let file_start = phdr.p_offset as usize;
        let file_end = file_start
            .checked_add(phdr.p_filesz as usize)
            .ok_or(ElfError::Truncated)?;
        if file_end > data.len() {
            return Err(ElfError::Truncated);
        }

        // Map every page this segment touches. Segments can share a page
        // at a boundary (e.g. .text's tail and .data's head), so an
        // `AlreadyMapped` here just means an earlier segment already
        // claimed it — not an error.
        let first_page = phdr.p_vaddr & !(PAGE_SIZE as u64 - 1);
        let mut page = first_page;
        while page < seg_end {
            match space.alloc_and_map(page, flags::USER_PAGE) {
                Ok(_) | Err(MapError::AlreadyMapped) => {}
                Err(_) => return Err(ElfError::OutOfMemory),
            }
            page += PAGE_SIZE as u64;
        }

        // Copy file bytes, then zero the .bss tail (p_memsz > p_filesz),
        // one byte at a time through the address space's own page-table
        // walk — not a raw physical pointer — so this is correct
        // regardless of which CR3 happens to be loaded right now.
        for k in 0..phdr.p_filesz {
            let v = phdr.p_vaddr + k;
            let phys = space
                .translate(VirtualAddress::new(v))
                .expect("just mapped");
            *(phys as *mut u8) = data[file_start + k as usize];
        }
        for k in phdr.p_filesz..phdr.p_memsz {
            let v = phdr.p_vaddr + k;
            let phys = space
                .translate(VirtualAddress::new(v))
                .expect("just mapped");
            *(phys as *mut u8) = 0;
        }
    }

    Ok(ehdr.e_entry)
}

/// Load `path` off a mounted drive (DOS-style `A:`/`B:`/`C:` prefix, or
/// the older no-prefix/`/initrd/...` rules — see `fs::resolve_path`) and
/// start it as a brand-new scheduler-managed process. Returns its pid.
///
/// Unlike the old one-way `run()`, this returns immediately — the new
/// process is queued `Ready` and runs concurrently with the caller, same
/// as any other `sched::spawn`ed task.
pub fn spawn(name: &'static str, path: &str) -> Result<u64, RunError> {
    crate::sched::spawn_process(name, path)
}
