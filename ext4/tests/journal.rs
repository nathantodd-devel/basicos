//! Journal replay, exercised with dirty journals written by the real
//! `debugfs` (its `journal_open`/`journal_write`/`journal_close`).

mod common;
use basicos_ext4::Ext4Fs;
use basicos_fs::vfs::FileSystem;
use common::*;
use std::io::Write;
use std::path::Path;
use std::process::Command;

fn debugfs_script(img: &Path, script: &str) {
    let dir = img.parent().unwrap();
    let f = dir.join("cmds");
    std::fs::File::create(&f).unwrap().write_all(script.as_bytes()).unwrap();
    let out = Command::new("debugfs").arg("-w").arg("-f").arg(&f).arg(img).output().unwrap();
    assert!(out.status.success(), "debugfs failed: {}", String::from_utf8_lossy(&out.stderr));
}

fn needs_recovery(img: &Path) -> bool {
    let out = Command::new("dumpe2fs").arg("-h").arg(img).output().unwrap();
    String::from_utf8_lossy(&out.stdout).lines().any(|l| l.starts_with("Filesystem features:") && l.contains("needs_recovery"))
}

/// A data block that isn't in use by anything: past the end of the
/// inode table and journal, in the last group.
fn spare_block(img: &Path, blocks_from_end: u64) -> u64 {
    let len = std::fs::metadata(img).unwrap().len();
    len / 4096 - blocks_from_end
}

fn read_raw_block(img: &Path, blk: u64, bs: u64) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(img).unwrap();
    f.seek(SeekFrom::Start(blk * bs)).unwrap();
    let mut b = vec![0u8; bs as usize];
    f.read_exact(&mut b).unwrap();
    b
}

fn run(mkfs_args: &[&str]) {
    if !tools_available() {
        return;
    }
    let dir = scratch_dir("journal");
    let img = mkfs(&dir, 16, mkfs_args, None);
    let blk = spare_block(&img, 5);
    let before = read_raw_block(&img, blk, 4096);
    assert!(before.iter().all(|&b| b == 0), "test block should start out unused");

    let mut payload = vec![0u8; 4096];
    payload[..21].copy_from_slice(b"REPLAYED-FROM-JOURNAL");
    std::fs::write(dir.join("payload"), &payload).unwrap();
    debugfs_script(&img, &format!("jo\njw -b {} {}\njc\n", blk, dir.join("payload").display()));
    assert!(needs_recovery(&img), "debugfs should have left a dirty journal");
    assert!(read_raw_block(&img, blk, 4096).iter().all(|&b| b == 0), "not yet at its home location");

    let mut fs = Ext4Fs::mount(Box::new(FileDisk::open(&img))).expect("mount replays the journal");
    assert_eq!(fs.info().replayed_transactions, 1);
    assert_eq!(read_raw_block(&img, blk, 4096), payload, "block reached its home location");
    assert!(!needs_recovery(&img), "needs_recovery cleared");
    // And the filesystem is usable afterwards.
    fs.write_file("/after", b"still works").unwrap();
    assert_eq!(fs.read_file_at("/after").unwrap(), b"still works");
    drop(fs);
    fsck(&img).unwrap_or_else(|r| panic!("fsck after replay:\n{}", r));

    // Mounting again finds nothing to replay.
    let fs = Ext4Fs::mount(Box::new(FileDisk::open(&img))).unwrap();
    assert_eq!(fs.info().replayed_transactions, 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn replays_a_committed_transaction() {
    run(&[]);
}

#[test]
fn replays_without_metadata_csum() {
    run(&["-O", "^metadata_csum"]);
}

#[test]
fn replays_with_1k_blocks() {
    // 1 KiB blocks: same mechanism, different geometry (the helper's
    // fixed 4 KiB block numbers still land in unused space).
    if !tools_available() {
        return;
    }
    let dir = scratch_dir("journal1k");
    let img = mkfs(&dir, 16, &["-b", "1024"], None);
    let blk = 16 * 1024 - 20; // near the end, in 1 KiB blocks
    let mut payload = vec![0u8; 1024];
    payload[..8].copy_from_slice(b"ONE-KIB!");
    std::fs::write(dir.join("payload"), &payload).unwrap();
    debugfs_script(&img, &format!("jo\njw -b {} {}\njc\n", blk, dir.join("payload").display()));
    let fs = Ext4Fs::mount(Box::new(FileDisk::open(&img))).unwrap();
    assert_eq!(fs.info().replayed_transactions, 1);
    assert_eq!(read_raw_block(&img, blk, 1024), payload);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn several_transactions_replay_in_order() {
    if !tools_available() {
        return;
    }
    let dir = scratch_dir("journal-multi");
    let img = mkfs(&dir, 16, &[], None);
    let blk = spare_block(&img, 6);
    let mut script = String::from("jo\n");
    for i in 0..3u8 {
        let mut p = vec![0u8; 4096];
        p[0] = b'0' + i;
        std::fs::write(dir.join(format!("p{}", i)), &p).unwrap();
        // Each transaction rewrites the same block; the last one must win.
        script += &format!("jw -b {} {}\njc\n", blk, dir.join(format!("p{}", i)).display());
        if i < 2 {
            script += "jo\n";
        }
    }
    debugfs_script(&img, &script);
    let fs = Ext4Fs::mount(Box::new(FileDisk::open(&img))).unwrap();
    assert!(fs.info().replayed_transactions >= 1);
    assert_eq!(read_raw_block(&img, blk, 4096)[0], b'2', "the last transaction's data wins");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_revoked_block_is_not_replayed() {
    if !tools_available() {
        return;
    }
    let dir = scratch_dir("journal-revoke");
    let img = mkfs(&dir, 16, &[], None);
    let blk = spare_block(&img, 7);
    let mut p = vec![0u8; 4096];
    p[..7].copy_from_slice(b"REVOKED");
    std::fs::write(dir.join("p"), &p).unwrap();
    // Write the block and revoke it in the same transaction.
    debugfs_script(&img, &format!("jo\njw -b {} {}\njw -r {}\njc\n", blk, dir.join("p").display(), blk));
    let fs = Ext4Fs::mount(Box::new(FileDisk::open(&img))).unwrap();
    assert!(read_raw_block(&img, blk, 4096).iter().all(|&b| b == 0), "revoked block stays untouched");
    assert!(!needs_recovery(&img));
    drop(fs);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn uncommitted_transaction_is_discarded() {
    if !tools_available() {
        return;
    }
    let dir = scratch_dir("journal-uncommitted");
    let img = mkfs(&dir, 16, &[], None);
    let blk = spare_block(&img, 8);
    let mut p = vec![0u8; 4096];
    p[..5].copy_from_slice(b"NOPE!");
    std::fs::write(dir.join("p"), &p).unwrap();
    // `-c`: write the transaction but don't commit it.
    debugfs_script(&img, &format!("jo\njw -b {} -c {}\njc\n", blk, dir.join("p").display()));
    let fs = Ext4Fs::mount(Box::new(FileDisk::open(&img))).unwrap();
    assert!(read_raw_block(&img, blk, 4096).iter().all(|&b| b == 0), "uncommitted data must not be applied");
    drop(fs);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_read_only_device_cannot_replay() {
    if !tools_available() {
        return;
    }
    struct Ro(FileDisk);
    impl basicos_fs::block::BlockDevice for Ro {
        fn block_count(&self) -> u64 {
            self.0.block_count()
        }
        fn read_block(&mut self, i: u64, b: &mut [u8]) -> Result<(), basicos_fs::block::BlockError> {
            self.0.read_block(i, b)
        }
        fn write_block(&mut self, _: u64, _: &[u8]) -> Result<(), basicos_fs::block::BlockError> {
            Err(basicos_fs::block::BlockError::ReadOnly)
        }
    }
    let dir = scratch_dir("journal-ro");
    let img = mkfs(&dir, 16, &[], None);
    let blk = spare_block(&img, 5);
    std::fs::write(dir.join("p"), vec![1u8; 4096]).unwrap();
    debugfs_script(&img, &format!("jo\njw -b {} {}\njc\n", blk, dir.join("p").display()));
    assert!(Ext4Fs::mount(Box::new(Ro(FileDisk::open(&img)))).is_err(), "must not serve a dirty journal read-only");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn replays_a_checksummed_journal() {
    // `jo -c` makes debugfs write a journal-checksum-v3 transaction.
    if !tools_available() {
        return;
    }
    let dir = scratch_dir("journal-csum");
    let img = mkfs(&dir, 16, &[], None);
    let blk = spare_block(&img, 9);
    let mut p = vec![0u8; 4096];
    p[..12].copy_from_slice(b"CSUM-JOURNAL");
    std::fs::write(dir.join("p"), &p).unwrap();
    debugfs_script(&img, &format!("jo -c\njw -b {} {}\njc\n", blk, dir.join("p").display()));
    let out = Command::new("dumpe2fs").arg("-h").arg(&img).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("journal_checksum_v3") || text.contains("journal_checksum_v2"), "journal should be checksummed:\n{}", text);
    let fs = Ext4Fs::mount(Box::new(FileDisk::open(&img))).unwrap();
    assert_eq!(fs.info().replayed_transactions, 1);
    assert_eq!(read_raw_block(&img, blk, 4096), p);
    drop(fs);
    fsck(&img).unwrap();
    std::fs::remove_dir_all(&dir).ok();
}
