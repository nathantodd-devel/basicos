//! Read support, checked against host directory trees that `mke2fs -d`
//! copies into real images.

mod common;
use basicos_ext4::Ext4Fs;
use basicos_fs::vfs::FileSystem;
use common::*;
use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

/// Build a directory tree exercising sizes, names and file kinds.
fn populate(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let mut want: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut put = |rel: &str, data: Vec<u8>| {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, &data).unwrap();
        want.insert(rel.to_string(), data);
    };
    put("empty", vec![]);
    put("one", vec![b'x']);
    for &n in &[1023usize, 1024, 1025, 4095, 4096, 4097, 12 * 1024 + 5, 300_000, 3_000_000] {
        put(&format!("sizes/f{}", n), rng.bytes(n));
    }
    put("dir/nested/deeper/file.txt", b"nested content\n".to_vec());
    put(&format!("names/{}", "n".repeat(255)), b"long name".to_vec());
    put("names/héllo wörld ☃.txt", "unicode".as_bytes().to_vec());
    put("names/with space", vec![1, 2, 3]);
    // Enough entries in one directory to make mke2fs build a hash tree.
    for i in 0..1500 {
        put(&format!("many/file{:05}.txt", i), format!("content {}\n", i).into_bytes());
    }
    // A sparse file.
    {
        use std::io::{Seek, SeekFrom, Write};
        let p = root.join("sparse");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(b"head").unwrap();
        f.seek(SeekFrom::Start(5_000_000)).unwrap();
        f.write_all(b"tail").unwrap();
        let mut v = vec![0u8; 5_000_004];
        v[..4].copy_from_slice(b"head");
        v[5_000_000..].copy_from_slice(b"tail");
        want.insert("sparse".to_string(), v);
    }
    std::os::unix::fs::symlink("sizes/f4096", root.join("link_fast")).unwrap();
    std::os::unix::fs::symlink("d".repeat(100) + "/x", root.join("link_slow")).unwrap();
    std::fs::hard_link(root.join("one"), root.join("one_hard")).unwrap();
    want.insert("one_hard".to_string(), vec![b'x']);
    std::fs::set_permissions(root.join("one"), std::fs::Permissions::from_mode(0o600)).unwrap();
    want
}

fn check_tree(fs: &mut Ext4Fs, root: &Path, want: &BTreeMap<String, Vec<u8>>) {
    for (rel, data) in want {
        let e = fs.find(&format!("/{}", rel)).unwrap_or_else(|e| panic!("find {}: {:?}", rel, e));
        assert!(!e.is_dir, "{}", rel);
        assert_eq!(e.size, data.len() as u64, "size of {}", rel);
        let got = fs.read_file(&e).unwrap();
        assert!(got == *data, "content of {} differs ({} vs {} bytes)", rel, got.len(), data.len());
    }
    // Directory listings match the host's.
    fn walk(fs: &mut Ext4Fs, host: &Path, rel: &str) {
        let mut host_names: Vec<String> =
            std::fs::read_dir(host).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        host_names.sort();
        let mut got: Vec<String> = fs.read_dir_at(rel).unwrap().into_iter().map(|e| e.name).collect();
        got.sort();
        got.retain(|n| !(rel == "/" && n == "lost+found")); // mke2fs's own
        assert_eq!(got, host_names, "listing of {}", rel);
        for n in host_names {
            let p = host.join(&n);
            let md = std::fs::symlink_metadata(&p).unwrap();
            if md.is_dir() {
                walk(fs, &p, &format!("{}/{}", rel, n));
            }
        }
    }
    walk(fs, root, "/");
    let md = std::fs::metadata(root.join("one")).unwrap();
    let e = fs.find("/one").unwrap();
    assert_eq!(e.attr.perm, md.mode() & 0o7777);
    assert_eq!(e.attr.nlink, 2, "hard-linked file");
}

fn run(args: &[&str], size_mb: u64) {
    if !tools_available() {
        eprintln!("skipping: mke2fs/e2fsck not installed");
        return;
    }
    let dir = scratch_dir("read");
    let tree = dir.join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    let want = populate(&tree);
    let img = mkfs(&dir, size_mb, args, Some(&tree));
    let mut fs = Ext4Fs::mount(Box::new(FileDisk::open(&img))).unwrap_or_else(|e| panic!("mount {:?}: {:?}", args, e));
    check_tree(&mut fs, &tree, &want);

    // Symlinks: contents, and following them.
    assert_eq!(fs.read_link("/link_fast").unwrap(), "sizes/f4096");
    assert_eq!(fs.read_link("/link_slow").unwrap(), "d".repeat(100) + "/x");
    assert_eq!(fs.find("/link_fast").unwrap().size, 4096, "find follows symlinks");
    assert!(fs.find_nofollow("/link_fast").unwrap().attr.is_symlink);
    assert!(fs.find("/link_slow").is_err(), "dangling link");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn default_4k() {
    run(&[], 64);
}
#[test]
fn block_1k() {
    run(&["-b", "1024"], 64);
}
#[test]
fn block_2k_no_csum() {
    run(&["-b", "2048", "-O", "^metadata_csum"], 64);
}
#[test]
fn no_flex_bg_no_64bit() {
    run(&["-O", "^flex_bg,^64bit,^metadata_csum"], 64);
}
#[test]
fn with_64bit() {
    run(&["-O", "64bit,metadata_csum"], 64);
}
#[test]
fn no_journal() {
    run(&["-O", "^has_journal"], 64);
}
#[test]
fn legacy_block_maps_ext3() {
    // ext3: block-mapped files (no extents), read-only for us.
    run(&["-t", "ext3"], 64);
}
#[test]
fn legacy_block_maps_ext2() {
    run(&["-t", "ext2"], 64);
}
#[test]
fn many_small_groups() {
    run(&["-g", "1024", "-b", "1024"], 40);
}
