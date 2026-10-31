//! Constants and byte-level helpers for the on-disk format. Everything
//! is little-endian except the jbd2 journal (big-endian, see
//! `journal.rs`).

pub const SB_OFFSET: usize = 1024;
pub const SB_SIZE: usize = 1024;
pub const EXT4_MAGIC: u16 = 0xEF53;

pub const ROOT_INO: u32 = 2;
pub const JOURNAL_INO: u32 = 8;

// --- superblock feature flags -------------------------------------------------
pub const COMPAT_HAS_JOURNAL: u32 = 0x4;
pub const COMPAT_DIR_INDEX: u32 = 0x20;
pub const COMPAT_SPARSE_SUPER2: u32 = 0x200;
pub const COMPAT_FAST_COMMIT: u32 = 0x400;

pub const INCOMPAT_FILETYPE: u32 = 0x2;
pub const INCOMPAT_RECOVER: u32 = 0x4;
pub const INCOMPAT_JOURNAL_DEV: u32 = 0x8;
pub const INCOMPAT_META_BG: u32 = 0x10;
pub const INCOMPAT_EXTENTS: u32 = 0x40;
pub const INCOMPAT_64BIT: u32 = 0x80;
pub const INCOMPAT_MMP: u32 = 0x100;
pub const INCOMPAT_FLEX_BG: u32 = 0x200;
pub const INCOMPAT_EA_INODE: u32 = 0x400;
pub const INCOMPAT_CSUM_SEED: u32 = 0x2000;
pub const INCOMPAT_LARGEDIR: u32 = 0x4000;
pub const INCOMPAT_INLINE_DATA: u32 = 0x8000;
pub const INCOMPAT_ENCRYPT: u32 = 0x10000;
pub const INCOMPAT_CASEFOLD: u32 = 0x20000;
/// Incompat bits this driver can read *and* write.
pub const INCOMPAT_SUPPORTED: u32 = INCOMPAT_FILETYPE
    | INCOMPAT_RECOVER
    | INCOMPAT_META_BG
    | INCOMPAT_EXTENTS
    | INCOMPAT_64BIT
    | INCOMPAT_FLEX_BG
    | INCOMPAT_EA_INODE
    | INCOMPAT_CSUM_SEED
    | INCOMPAT_LARGEDIR;

pub const RO_COMPAT_SPARSE_SUPER: u32 = 0x1;
pub const RO_COMPAT_LARGE_FILE: u32 = 0x2;
pub const RO_COMPAT_HUGE_FILE: u32 = 0x8;
pub const RO_COMPAT_GDT_CSUM: u32 = 0x10;
pub const RO_COMPAT_DIR_NLINK: u32 = 0x20;
pub const RO_COMPAT_EXTRA_ISIZE: u32 = 0x40;
pub const RO_COMPAT_QUOTA: u32 = 0x100;
pub const RO_COMPAT_BIGALLOC: u32 = 0x200;
pub const RO_COMPAT_METADATA_CSUM: u32 = 0x400;
pub const RO_COMPAT_READONLY: u32 = 0x1000;
pub const RO_COMPAT_PROJECT: u32 = 0x2000;
pub const RO_COMPAT_SHARED_BLOCKS: u32 = 0x4000;
pub const RO_COMPAT_VERITY: u32 = 0x8000;
/// ro_compat bits that make us refuse to *write* (but reading is fine).
/// Every ro_compat bit this driver knows the meaning of; any other bit
/// (e.g. `orphan_present`) means on-disk state we can't keep consistent,
/// so writes are refused.
pub const RO_COMPAT_KNOWN: u32 = RO_COMPAT_SPARSE_SUPER
    | RO_COMPAT_LARGE_FILE
    | 0x4
    | RO_COMPAT_HUGE_FILE
    | RO_COMPAT_GDT_CSUM
    | RO_COMPAT_DIR_NLINK
    | RO_COMPAT_EXTRA_ISIZE
    | RO_COMPAT_QUOTA
    | RO_COMPAT_BIGALLOC
    | RO_COMPAT_METADATA_CSUM
    | RO_COMPAT_READONLY
    | RO_COMPAT_PROJECT
    | RO_COMPAT_SHARED_BLOCKS
    | RO_COMPAT_VERITY;
pub const RO_COMPAT_FORCES_READONLY: u32 = RO_COMPAT_QUOTA
    | RO_COMPAT_BIGALLOC
    | RO_COMPAT_READONLY
    | RO_COMPAT_PROJECT
    | RO_COMPAT_SHARED_BLOCKS
    | RO_COMPAT_VERITY;

// --- inode flags --------------------------------------------------------------
pub const INODE_IMMUTABLE: u32 = 0x10;
pub const INODE_APPEND: u32 = 0x20;
pub const INODE_INDEX: u32 = 0x1000;
pub const INODE_HUGE_FILE: u32 = 0x40000;
pub const INODE_EXTENTS: u32 = 0x80000;
pub const INODE_INLINE_DATA: u32 = 0x1000_0000;
pub const INODE_ENCRYPT: u32 = 0x800;

// --- group descriptor flags ---------------------------------------------------
pub const BG_INODE_UNINIT: u16 = 1;
pub const BG_BLOCK_UNINIT: u16 = 2;
pub const BG_INODE_ZEROED: u16 = 4;

// --- extents ------------------------------------------------------------------
pub const EXT_MAGIC: u16 = 0xF30A;
/// Longest initialized extent (bit 15 of `ee_len` marks "uninitialized").
pub const EXT_INIT_MAX_LEN: u32 = 32768;

// --- directory entry types ------------------------------------------------------
pub const FT_UNKNOWN: u8 = 0;
pub const FT_REG: u8 = 1;
pub const FT_DIR: u8 = 2;
pub const FT_SYMLINK: u8 = 7;

// --- st_mode ---------------------------------------------------------------------
pub const S_IFMT: u16 = 0o170000;
pub const S_IFREG: u16 = 0o100000;
pub const S_IFDIR: u16 = 0o040000;
pub const S_IFLNK: u16 = 0o120000;

pub fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
pub fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
pub fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
pub fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
pub fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
pub fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}
