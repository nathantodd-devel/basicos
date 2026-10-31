//! The interface every filesystem in basicos implements, so the kernel
//! can mount FAT32 and ext4 volumes side by side under drive letters and
//! treat them uniformly (see `kernel/src/fs`).
//!
//! Paths passed in are *volume-relative*: `/`-separated, no drive letter,
//! `""` or `"/"` for the root. The kernel normalizes `.`/`..` before it
//! gets here, so implementations only see plain components.

use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    Io,
    NotFat32,
    NotFound,
    NotAFile,
    NotADirectory,
    NoSpace,
    NameTooLong,
    /// The target of a create/rename already exists.
    AlreadyExists,
    /// `remove_dir` on a directory that still has entries.
    NotEmpty,
    /// Nonsense request (removing the root, moving a directory into itself).
    Invalid,
    /// The volume (or this operation on it) is read-only.
    ReadOnly,
    /// A feature of the on-disk format this driver doesn't implement.
    Unsupported,
    /// On-disk structures are inconsistent or fail their checksums.
    Corrupt,
    /// Too many symbolic links followed (or a loop).
    TooManyLinks,
    /// The file (or link count) would exceed the format's limits.
    TooBig,
}

pub type FsResult<T> = Result<T, FsError>;

/// Kind bits of a file — the `S_IFMT` values of `st_mode`.
pub const S_IFMT: u32 = 0o170000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFLNK: u32 = 0o120000;

/// Extra metadata some formats keep (ext4 does; FAT mostly doesn't).
/// All-zero means "unknown": callers fall back to defaults.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Attr {
    /// Permission bits (`0o755`), *not* including the file type.
    pub perm: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub is_symlink: bool,
}

/// One directory entry / file's metadata.
#[derive(Debug, Clone, Default)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// FAT: first cluster. ext4: the inode number. Either way, unique per
    /// file within a volume (0 for the FAT root), which is all `getdents`
    /// and `stat` need from it.
    pub first_cluster: u32,
    pub attr: Attr,
}

/// What a mounted volume can do. Everything has a default that says
/// "not supported", so a format only implements what it has.
pub trait FileSystem: Send {
    /// `"fat32"`, `"ext4"`, ...
    fn fs_type(&self) -> &'static str;

    /// Volume label, if the format has one.
    fn label(&self) -> String {
        String::new()
    }

    /// One-line remark about how the mount went (journal replayed, why
    /// it's read-only, ...), for the boot log.
    fn note(&self) -> Option<String> {
        None
    }

    /// True if writes will be refused (read-only media, or an on-disk
    /// feature this driver can't safely update).
    fn read_only(&self) -> bool {
        false
    }

    /// `(total_bytes, free_bytes)`.
    fn space(&mut self) -> (u64, u64) {
        (0, 0)
    }

    fn read_dir_at(&mut self, path: &str) -> FsResult<Vec<DirEntry>>;

    /// Look `path` up, following symbolic links in every component
    /// including the last.
    fn find(&mut self, path: &str) -> FsResult<DirEntry>;

    /// Like [`find`](Self::find) but doesn't follow a symlink in the last
    /// component (`lstat`).
    fn find_nofollow(&mut self, path: &str) -> FsResult<DirEntry> {
        self.find(path)
    }

    fn read_file(&mut self, entry: &DirEntry) -> FsResult<Vec<u8>>;

    fn read_file_at(&mut self, path: &str) -> FsResult<Vec<u8>> {
        let entry = self.find(path)?;
        self.read_file(&entry)
    }

    /// Create or overwrite the file at `path` with `data`.
    fn write_file(&mut self, path: &str, data: &[u8]) -> FsResult<()>;

    fn delete_file(&mut self, path: &str) -> FsResult<()>;
    fn create_dir(&mut self, path: &str) -> FsResult<()>;
    fn remove_dir(&mut self, path: &str) -> FsResult<()>;
    fn rename(&mut self, from: &str, to: &str) -> FsResult<()>;

    fn read_link(&mut self, _path: &str) -> FsResult<String> {
        Err(FsError::Unsupported)
    }
    /// Create a symlink at `link` pointing at `target`.
    fn symlink(&mut self, _target: &str, _link: &str) -> FsResult<()> {
        Err(FsError::Unsupported)
    }
    /// Create `new` as another name for the existing file `existing`.
    fn hard_link(&mut self, _existing: &str, _new: &str) -> FsResult<()> {
        Err(FsError::Unsupported)
    }
    /// Set the permission bits (`chmod`).
    fn set_perm(&mut self, _path: &str, _perm: u32) -> FsResult<()> {
        Err(FsError::Unsupported)
    }

    // --- streaming access ----------------------------------------------
    //
    // Formats that can read and write a byte range of a file directly
    // implement these, so big files needn't be held in memory. The kernel
    // falls back to whole-file buffering (`read_file`/`write_file`) when
    // `open_inode` returns `Ok(None)`.

    /// Open the regular file at `path` (following symlinks), optionally
    /// creating it, and pin it: it stays valid — even if unlinked — until
    /// [`release_inode`](Self::release_inode). `Ok(None)`: not supported.
    fn open_inode(&mut self, _path: &str, _create: bool, _truncate: bool) -> FsResult<Option<u64>> {
        Ok(None)
    }
    fn read_inode(&mut self, _ino: u64, _offset: u64, _buf: &mut [u8]) -> FsResult<usize> {
        Err(FsError::Unsupported)
    }
    fn write_inode(&mut self, _ino: u64, _offset: u64, _data: &[u8]) -> FsResult<usize> {
        Err(FsError::Unsupported)
    }
    fn truncate_inode(&mut self, _ino: u64, _len: u64) -> FsResult<()> {
        Err(FsError::Unsupported)
    }
    fn size_inode(&mut self, _ino: u64) -> FsResult<u64> {
        Err(FsError::Unsupported)
    }
    fn release_inode(&mut self, _ino: u64) {}
    /// Push anything cached to the device.
    fn sync(&mut self) -> FsResult<()> {
        Ok(())
    }
}
