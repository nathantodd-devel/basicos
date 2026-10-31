//! Shared helpers: build real ext4 images with mke2fs, expose them as
//! BlockDevices, and check them with e2fsck.
#![allow(dead_code)]

use basicos_fs::block::{BlockDevice, BlockError, BLOCK_SIZE};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct FileDisk {
    f: File,
    blocks: u64,
}

impl FileDisk {
    pub fn open(path: &Path) -> FileDisk {
        let f = OpenOptions::new().read(true).write(true).open(path).unwrap();
        let blocks = f.metadata().unwrap().len() / BLOCK_SIZE as u64;
        FileDisk { f, blocks }
    }
}

impl BlockDevice for FileDisk {
    fn block_count(&self) -> u64 {
        self.blocks
    }
    fn read_block(&mut self, i: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        if i >= self.blocks {
            return Err(BlockError::OutOfRange);
        }
        self.f.seek(SeekFrom::Start(i * BLOCK_SIZE as u64)).map_err(|_| BlockError::Io)?;
        self.f.read_exact(&mut buf[..BLOCK_SIZE]).map_err(|_| BlockError::Io)
    }
    fn write_block(&mut self, i: u64, buf: &[u8]) -> Result<(), BlockError> {
        if i >= self.blocks {
            return Err(BlockError::OutOfRange);
        }
        self.f.seek(SeekFrom::Start(i * BLOCK_SIZE as u64)).map_err(|_| BlockError::Io)?;
        self.f.write_all(&buf[..BLOCK_SIZE]).map_err(|_| BlockError::Io)
    }
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

pub fn scratch_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("basicos-ext4-{}-{}-{}", std::process::id(), tag, n));
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn tools_available() -> bool {
    ["mkfs.ext4", "e2fsck", "debugfs"].iter().all(|t| Command::new(t).arg("-V").output().is_ok())
}

/// Create an image of `size_mb` MiB with `mke2fs` (`args` are extra
/// options), optionally populated from a host directory.
pub fn mkfs(dir: &Path, size_mb: u64, args: &[&str], populate: Option<&Path>) -> PathBuf {
    let img = dir.join("disk.img");
    let f = File::create(&img).unwrap();
    f.set_len(size_mb << 20).unwrap();
    let mut cmd = Command::new("mkfs.ext4");
    cmd.arg("-q").arg("-F");
    // mke2fs picks 1 KiB blocks for small images; make 4 KiB the default
    // so "default" really means 4 KiB and other sizes are asked for.
    if !args.contains(&"-b") {
        cmd.args(["-b", "4096"]);
    }
    cmd.args(args);
    if let Some(p) = populate {
        cmd.arg("-d").arg(p);
    }
    cmd.arg(&img);
    let out = cmd.output().expect("run mkfs.ext4");
    assert!(out.status.success(), "mkfs failed: {}", String::from_utf8_lossy(&out.stderr));
    img
}

/// `e2fsck -fn`: Ok(()) if the filesystem is clean, else its report.
pub fn fsck(img: &Path) -> Result<(), String> {
    let out = Command::new("e2fsck").arg("-fn").arg(img).output().expect("run e2fsck");
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "e2fsck exit {:?}\n{}{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

pub fn debugfs(img: &Path, cmd: &str) -> Vec<u8> {
    let out = Command::new("debugfs").arg("-R").arg(cmd).arg(img).output().expect("run debugfs");
    out.stdout
}

/// Deterministic pseudo-random bytes.
pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}
