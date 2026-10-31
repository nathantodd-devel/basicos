//! Write support: every result is checked two ways — our own reader
//! against an in-memory model, and the real `e2fsck -fn` on the image.

mod common;
use basicos_ext4::Ext4Fs;
use basicos_fs::vfs::{FileSystem, FsError};
use common::*;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
enum Node {
    File(Vec<u8>),
    Dir,
    Link(String),
}

type Model = BTreeMap<String, Node>;

fn fresh(args: &[&str], size_mb: u64, tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = scratch_dir(tag);
    let img = mkfs(&dir, size_mb, args, None);
    (dir, img)
}

fn mount(img: &Path) -> Ext4Fs {
    let mut fs = Ext4Fs::mount(Box::new(FileDisk::open(img))).expect("mount");
    fs.set_clock(|| 1_700_000_000);
    fs
}

/// Compare the whole filesystem tree with `model`.
fn verify(fs: &mut Ext4Fs, model: &Model, ctx: &str) {
    fn walk(fs: &mut Ext4Fs, dir: &str, out: &mut Model) {
        for e in fs.read_dir_at(dir).unwrap() {
            let path = if dir == "/" { format!("/{}", e.name) } else { format!("{}/{}", dir, e.name) };
            if dir == "/" && e.name == "lost+found" {
                continue;
            }
            if e.attr.is_symlink {
                out.insert(path.clone(), Node::Link(fs.read_link(&path).unwrap()));
            } else if e.is_dir {
                out.insert(path.clone(), Node::Dir);
                walk(fs, &path, out);
            } else {
                let data = fs.read_file(&e).unwrap();
                out.insert(path, Node::File(data));
            }
        }
    }
    let mut got = Model::new();
    walk(fs, "/", &mut got);
    let keys_got: Vec<_> = got.keys().collect();
    let keys_want: Vec<_> = model.keys().collect();
    assert_eq!(keys_got, keys_want, "{}: set of paths differs", ctx);
    for (p, n) in model {
        let g = &got[p];
        match (g, n) {
            (Node::File(a), Node::File(b)) => {
                assert!(a == b, "{}: content of {} differs ({} vs {} bytes)", ctx, p, a.len(), b.len())
            }
            _ => assert_eq!(g, n, "{}: {}", ctx, p),
        }
    }
}

fn parent_of(p: &str) -> &str {
    match p.rfind('/') {
        Some(0) => "/",
        Some(i) => &p[..i],
        None => "/",
    }
}

/// One random mutation, applied to both the filesystem and the model.
fn step(fs: &mut Ext4Fs, model: &mut Model, rng: &mut Rng, bs: usize) {
    let names = ["a", "b", "c", "dd", "eee", "f1", "g22", "hhhh", "long_name_number_one", "x"];
    let dirs: Vec<String> =
        std::iter::once("/".to_string()).chain(model.iter().filter(|(_, n)| **n == Node::Dir).map(|(p, _)| p.clone())).collect();
    let files: Vec<String> = model.iter().filter(|(_, n)| matches!(n, Node::File(_))).map(|(p, _)| p.clone()).collect();
    let pick_dir = |rng: &mut Rng| dirs[rng.below(dirs.len() as u64) as usize].clone();
    let join = |d: &str, n: &str| if d == "/" { format!("/{}", n) } else { format!("{}/{}", d, n) };
    let size = |rng: &mut Rng| -> usize {
        match rng.below(10) {
            0 => 0,
            1..=4 => rng.below(200) as usize,
            5..=6 => bs * (1 + rng.below(3) as usize) + rng.below(3) as usize - 1,
            7..=8 => rng.below(30_000) as usize,
            _ => rng.below(400_000) as usize,
        }
    };

    match rng.below(14) {
        0..=2 => {
            // create / overwrite a file
            let p = join(&pick_dir(rng), names[rng.below(10) as usize]);
            if matches!(model.get(&p), Some(Node::Dir) | Some(Node::Link(_))) {
                return;
            }
            let n = size(rng);
            let data = rng.bytes(n);
            fs.write_file(&p, &data).unwrap_or_else(|e| panic!("write_file {}: {:?}", p, e));
            model.insert(p, Node::File(data));
        }
        3..=4 if !files.is_empty() => {
            // append
            let p = &files[rng.below(files.len() as u64) as usize];
            let Some(Node::File(cur)) = model.get(p).cloned() else { return };
            let n = rng.below(20_000) as usize;
            let extra = rng.bytes(n);
            let ino = fs.open_inode(p, false, false).unwrap().unwrap();
            fs.write_inode(ino, cur.len() as u64, &extra).unwrap();
            fs.release_inode(ino);
            let mut v = cur;
            v.extend_from_slice(&extra);
            model.insert(p.clone(), Node::File(v));
        }
        5 if !files.is_empty() => {
            // overwrite in the middle / write past the end (sparse)
            let p = &files[rng.below(files.len() as u64) as usize];
            let Some(Node::File(cur)) = model.get(p).cloned() else { return };
            let off = if cur.is_empty() { rng.below(50_000) } else { rng.below(cur.len() as u64 + 60_000) } as usize;
            let n = rng.below(15_000) as usize + 1;
            let data = rng.bytes(n);
            let ino = fs.open_inode(p, false, false).unwrap().unwrap();
            fs.write_inode(ino, off as u64, &data).unwrap();
            fs.release_inode(ino);
            let mut v = cur;
            if v.len() < off + data.len() {
                v.resize(off + data.len(), 0);
            }
            v[off..off + data.len()].copy_from_slice(&data);
            model.insert(p.clone(), Node::File(v));
        }
        6 if !files.is_empty() => {
            // truncate (shrink or grow)
            let p = &files[rng.below(files.len() as u64) as usize];
            let Some(Node::File(cur)) = model.get(p).cloned() else { return };
            let new_len = rng.below(cur.len() as u64 + 20_000) as usize;
            let ino = fs.open_inode(p, false, false).unwrap().unwrap();
            fs.truncate_inode(ino, new_len as u64).unwrap();
            fs.release_inode(ino);
            let mut v = cur;
            v.resize(new_len, 0);
            model.insert(p.clone(), Node::File(v));
        }
        7 if !files.is_empty() => {
            let p = files[rng.below(files.len() as u64) as usize].clone();
            fs.delete_file(&p).unwrap();
            model.remove(&p);
        }
        8 => {
            let p = join(&pick_dir(rng), names[rng.below(10) as usize]);
            if model.contains_key(&p) {
                assert_eq!(fs.create_dir(&p), Err(FsError::AlreadyExists), "mkdir over {}", p);
            } else {
                fs.create_dir(&p).unwrap();
                model.insert(p, Node::Dir);
            }
        }
        9 => {
            // rmdir a random directory
            let cands: Vec<&String> = model.iter().filter(|(_, n)| **n == Node::Dir).map(|(p, _)| p).collect();
            if cands.is_empty() {
                return;
            }
            let p = cands[rng.below(cands.len() as u64) as usize].clone();
            let empty = !model.keys().any(|k| k.starts_with(&format!("{}/", p)));
            let r = fs.remove_dir(&p);
            if empty {
                r.unwrap();
                model.remove(&p);
            } else {
                assert_eq!(r, Err(FsError::NotEmpty), "rmdir non-empty {}", p);
            }
        }
        10 | 11 => {
            // rename a file or directory somewhere else
            let cands: Vec<String> = model.keys().cloned().collect();
            if cands.is_empty() {
                return;
            }
            let from = cands[rng.below(cands.len() as u64) as usize].clone();
            let to = join(&pick_dir(rng), names[rng.below(10) as usize]);
            let from_is_dir = model[&from] == Node::Dir;
            if to == from || (from_is_dir && (to == from || to.starts_with(&format!("{}/", from)))) {
                return;
            }
            let dst = model.get(&to).cloned();
            let expect_err = match (&model[&from], &dst) {
                (Node::Dir, Some(Node::Dir)) => {
                    if model.keys().any(|k| k.starts_with(&format!("{}/", to))) { Some(FsError::NotEmpty) } else { None }
                }
                (Node::Dir, Some(_)) => Some(FsError::NotADirectory),
                (_, Some(Node::Dir)) => Some(FsError::AlreadyExists),
                _ => None,
            };
            let r = fs.rename(&from, &to);
            match expect_err {
                Some(e) => assert_eq!(r, Err(e), "rename {} -> {}", from, to),
                None => {
                    r.unwrap_or_else(|e| panic!("rename {} -> {}: {:?}", from, to, e));
                    // move the subtree in the model
                    let moved: Vec<(String, Node)> = model
                        .iter()
                        .filter(|(k, _)| **k == from || k.starts_with(&format!("{}/", from)))
                        .map(|(k, v)| (format!("{}{}", to, &k[from.len()..]), v.clone()))
                        .collect();
                    model.retain(|k, _| !(*k == from || k.starts_with(&format!("{}/", from))));
                    model.remove(&to);
                    model.extend(moved);
                }
            }
        }
        12 => {
            // symlink (fast and slow)
            let p = join(&pick_dir(rng), names[rng.below(10) as usize]);
            if model.contains_key(&p) {
                return;
            }
            let target = if rng.below(2) == 0 { "some/target".to_string() } else { format!("/{}", "t".repeat(90 + rng.below(200) as usize)) };
            fs.symlink(&target, &p).unwrap();
            model.insert(p, Node::Link(target));
        }
        _ => {
            // unlink a symlink
            let links: Vec<String> = model.iter().filter(|(_, n)| matches!(n, Node::Link(_))).map(|(p, _)| p.clone()).collect();
            if let Some(p) = links.first() {
                fs.delete_file(p).unwrap();
                model.remove(p);
            }
        }
    }
    // Everything we removed from the model must be gone from the fs
    // (checked wholesale in `verify`).
    let _ = parent_of("/");
}

fn randomized(args: &[&str], size_mb: u64, seed: u64, steps: usize, bs: usize) {
    if !tools_available() {
        eprintln!("skipping: mke2fs/e2fsck not installed");
        return;
    }
    let (dir, img) = fresh(args, size_mb, "rand");
    let mut model = Model::new();
    let mut rng = Rng(seed);
    {
        let mut fs = mount(&img);
        for i in 0..steps {
            step(&mut fs, &mut model, &mut rng, bs);
            if i % 100 == 99 {
                verify(&mut fs, &model, &format!("step {}", i));
            }
        }
        verify(&mut fs, &model, "end");
        fs.sync().unwrap();
    }
    fsck(&img).unwrap_or_else(|r| panic!("e2fsck complains after {:?} seed {}:\n{}", args, seed, r));
    // A cold remount sees the same thing.
    let mut fs = mount(&img);
    verify(&mut fs, &model, "after remount");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn random_default() {
    randomized(&[], 64, 0xC0FFEE, 400, 4096);
}
#[test]
fn random_default_more_seeds() {
    for seed in [1u64, 2, 3, 0xDEAD_BEEF] {
        randomized(&[], 64, seed, 300, 4096);
    }
}
#[test]
fn random_1k_blocks() {
    randomized(&["-b", "1024", "-N", "3000"], 48, 0xABCDEF, 400, 1024);
}
#[test]
fn random_no_metadata_csum() {
    randomized(&["-O", "^metadata_csum,uninit_bg"], 64, 0x5EED, 400, 4096);
}
#[test]
fn random_no_uninit_no_flex() {
    randomized(&["-O", "^metadata_csum,^uninit_bg,^flex_bg"], 64, 0x77, 300, 4096);
}
#[test]
fn random_small_groups() {
    randomized(&["-g", "2048", "-b", "1024", "-N", "2000"], 32, 0x99, 300, 1024);
}
#[test]
fn random_64bit() {
    randomized(&["-O", "64bit"], 64, 0x64, 300, 4096);
}
#[test]
fn random_no_journal() {
    randomized(&["-O", "^has_journal"], 64, 0x4242, 300, 4096);
}

// ---------------------------------------------------------------------------
// Targeted scenarios
// ---------------------------------------------------------------------------

fn clean(fs: Ext4Fs, img: &Path, dir: &Path, what: &str) {
    drop(fs);
    fsck(img).unwrap_or_else(|r| panic!("{}: e2fsck complains:\n{}", what, r));
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn deep_extent_trees_from_fragmentation() {
    if !tools_available() {
        return;
    }
    // 1 KiB blocks: 84 extents fit a leaf, 4 in the inode, so ~500
    // fragments force a depth-2 tree. Two files growing block by block in
    // lockstep interleave on disk and can't merge.
    let (dir, img) = fresh(&["-b", "1024", "-O", "^has_journal"], 32, "deep");
    let mut fs = mount(&img);
    let mut rng = Rng(7);
    let a = fs.open_inode("/a", true, false).unwrap().unwrap();
    let b = fs.open_inode("/b", true, false).unwrap().unwrap();
    let (mut da, mut db) = (Vec::new(), Vec::new());
    for _ in 0..600 {
        let x = rng.bytes(1024);
        fs.write_inode(a, da.len() as u64, &x).unwrap();
        da.extend_from_slice(&x);
        let y = rng.bytes(1024);
        fs.write_inode(b, db.len() as u64, &y).unwrap();
        db.extend_from_slice(&y);
    }
    let mut got = vec![0u8; da.len()];
    assert_eq!(fs.read_inode(a, 0, &mut got).unwrap(), da.len());
    assert!(got == da, "fragmented file A reads back");
    let mut got = vec![0u8; db.len()];
    fs.read_inode(b, 0, &mut got).unwrap();
    assert!(got == db, "fragmented file B reads back");
    fs.release_inode(a);
    fs.release_inode(b);
    // The tree really is three levels deep (root -> index -> leaf).
    let ex = String::from_utf8_lossy(&debugfs(&img, "ex /a")).into_owned();
    assert!(ex.lines().any(|l| l.trim_start().starts_with("0/ 2")), "expected a depth-2 extent tree:\n{}", &ex[..ex.len().min(600)]);
    // Shrink it back down through several tree depths.
    let a = fs.open_inode("/a", false, false).unwrap().unwrap();
    for keep in [400usize, 90, 5, 0] {
        fs.truncate_inode(a, (keep * 1024) as u64).unwrap();
        let mut got = vec![0u8; keep * 1024];
        assert_eq!(fs.read_inode(a, 0, &mut got).unwrap(), keep * 1024);
        assert!(got == da[..keep * 1024], "after truncating to {} blocks", keep);
    }
    fs.release_inode(a);
    fs.delete_file("/a").unwrap();
    fs.delete_file("/b").unwrap();
    clean(fs, &img, &dir, "deep trees");
}

#[test]
fn disk_full_then_recover() {
    if !tools_available() {
        return;
    }
    let (dir, img) = fresh(&["-O", "^has_journal"], 8, "full");
    let mut fs = mount(&img);
    let chunk = vec![0xA5u8; 64 * 1024];
    let mut n = 0;
    loop {
        match fs.write_file(&format!("/f{}", n), &chunk) {
            Ok(()) => n += 1,
            Err(FsError::NoSpace) => break,
            Err(e) => panic!("unexpected {:?}", e),
        }
        assert!(n < 1000);
    }
    assert!(n > 10, "wrote {} files before filling 8 MiB", n);
    // A failed write must not have corrupted anything, and space comes back.
    for i in 0..n {
        assert_eq!(fs.read_file_at(&format!("/f{}", i)).unwrap(), chunk, "f{}", i);
    }
    fs.delete_file("/f0").unwrap();
    fs.delete_file("/f1").unwrap();
    fs.write_file("/again", &chunk).unwrap();
    clean(fs, &img, &dir, "disk full");
}

#[test]
fn indexed_directory_accepts_inserts_and_deletes() {
    if !tools_available() {
        return;
    }
    let dir = scratch_dir("dx");
    let tree = dir.join("tree");
    std::fs::create_dir_all(tree.join("big")).unwrap();
    for i in 0..1500 {
        std::fs::write(tree.join(format!("big/file{:05}", i)), format!("{}", i)).unwrap();
    }
    let img = mkfs(&dir, 32, &[], Some(&tree));
    let mut fs = mount(&img);
    for i in 0..300 {
        fs.write_file(&format!("/big/new{:04}", i), b"fresh").unwrap();
    }
    for i in (0..1500).step_by(3) {
        fs.delete_file(&format!("/big/file{:05}", i)).unwrap();
    }
    let names: Vec<String> = fs.read_dir_at("/big").unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names.len(), 1500 + 300 - 500);
    assert_eq!(fs.read_file_at("/big/file00001").unwrap(), b"1");
    assert_eq!(fs.read_file_at("/big/new0299").unwrap(), b"fresh");
    assert!(fs.read_file_at("/big/file00000").is_err());
    clean(fs, &img, &dir, "dx directory");
}

#[test]
fn big_directory_grows_and_shrinks() {
    if !tools_available() {
        return;
    }
    let (dir, img) = fresh(&["-b", "1024"], 32, "bigdir");
    let mut fs = mount(&img);
    fs.create_dir("/d").unwrap();
    for i in 0..3000 {
        fs.write_file(&format!("/d/entry_with_a_longish_name_{:05}", i), b"").unwrap();
    }
    assert_eq!(fs.read_dir_at("/d").unwrap().len(), 3000);
    for i in 0..3000 {
        fs.delete_file(&format!("/d/entry_with_a_longish_name_{:05}", i)).unwrap();
    }
    assert_eq!(fs.read_dir_at("/d").unwrap().len(), 0);
    fs.remove_dir("/d").unwrap();
    clean(fs, &img, &dir, "big directory");
}

#[test]
fn unlinked_open_file_survives_until_release() {
    if !tools_available() {
        return;
    }
    let (dir, img) = fresh(&["-O", "^has_journal"], 16, "orphan");
    let mut fs = mount(&img);
    let before = fs.space().1;
    let data = vec![0x42u8; 300_000];
    fs.write_file("/tmp", &data).unwrap();
    let ino = fs.open_inode("/tmp", false, false).unwrap().unwrap();
    fs.delete_file("/tmp").unwrap();
    assert!(fs.find("/tmp").is_err());
    // Still fully readable and writable through the handle.
    let mut got = vec![0u8; data.len()];
    assert_eq!(fs.read_inode(ino, 0, &mut got).unwrap(), data.len());
    assert!(got == data);
    fs.write_inode(ino, 0, b"XY").unwrap();
    assert!(fs.space().1 < before, "space is still held");
    fs.release_inode(ino);
    assert_eq!(fs.space().1, before, "space returned on last close");
    clean(fs, &img, &dir, "orphans");
}

#[test]
fn hard_links_and_link_counts() {
    if !tools_available() {
        return;
    }
    let (dir, img) = fresh(&[], 16, "links");
    let mut fs = mount(&img);
    fs.write_file("/orig", b"shared").unwrap();
    fs.hard_link("/orig", "/alias").unwrap();
    assert_eq!(fs.find("/orig").unwrap().attr.nlink, 2);
    assert_eq!(fs.find("/orig").unwrap().first_cluster, fs.find("/alias").unwrap().first_cluster);
    fs.write_file("/alias", b"changed through the alias").unwrap();
    assert_eq!(fs.read_file_at("/orig").unwrap(), b"changed through the alias");
    fs.delete_file("/orig").unwrap();
    assert_eq!(fs.find("/alias").unwrap().attr.nlink, 1);
    assert_eq!(fs.read_file_at("/alias").unwrap(), b"changed through the alias");
    fs.create_dir("/d").unwrap();
    assert_eq!(fs.hard_link("/d", "/d2"), Err(FsError::NotAFile), "no hard links to directories");
    // Directory link counts follow their subdirectories.
    assert_eq!(fs.find("/d").unwrap().attr.nlink, 2);
    fs.create_dir("/d/x").unwrap();
    fs.create_dir("/d/y").unwrap();
    assert_eq!(fs.find("/d").unwrap().attr.nlink, 4);
    fs.rename("/d/x", "/x").unwrap();
    assert_eq!(fs.find("/d").unwrap().attr.nlink, 3);
    fs.remove_dir("/d/y").unwrap();
    assert_eq!(fs.find("/d").unwrap().attr.nlink, 2);
    clean(fs, &img, &dir, "links");
}

#[test]
fn permissions_and_symlinked_directories() {
    if !tools_available() {
        return;
    }
    let (dir, img) = fresh(&[], 16, "perm");
    let mut fs = mount(&img);
    fs.create_dir("/real").unwrap();
    fs.symlink("/real", "/alias").unwrap();
    fs.symlink("real", "/rel").unwrap();
    fs.write_file("/alias/through_abs", b"1").unwrap();
    fs.write_file("/rel/through_rel", b"2").unwrap();
    assert_eq!(fs.read_file_at("/real/through_abs").unwrap(), b"1");
    assert_eq!(fs.read_file_at("/real/through_rel").unwrap(), b"2");
    fs.set_perm("/real/through_abs", 0o640).unwrap();
    assert_eq!(fs.find("/real/through_abs").unwrap().attr.perm, 0o640);
    fs.symlink("/loop2", "/loop1").unwrap();
    fs.symlink("/loop1", "/loop2").unwrap();
    assert_eq!(fs.find("/loop1").err(), Some(FsError::TooManyLinks));
    assert_eq!(fs.read_link("/real"), Err(FsError::Invalid), "not a symlink");
    clean(fs, &img, &dir, "perm");
}

#[test]
fn large_file_round_trip() {
    if !tools_available() {
        return;
    }
    let (dir, img) = fresh(&["-O", "^has_journal"], 96, "large");
    let mut fs = mount(&img);
    let mut rng = Rng(99);
    let ino = fs.open_inode("/big", true, false).unwrap().unwrap();
    let mut all = Vec::new();
    for _ in 0..40 {
        let c = rng.bytes(1 << 20);
        fs.write_inode(ino, all.len() as u64, &c).unwrap();
        all.extend_from_slice(&c);
    }
    let mut got = vec![0u8; all.len()];
    assert_eq!(fs.read_inode(ino, 0, &mut got).unwrap(), all.len());
    assert!(got == all, "40 MiB round trip");
    // Unaligned reads across extent boundaries.
    for &(off, len) in &[(1usize, 5000usize), (4095, 3), (1 << 20, 100_000), (all.len() - 10, 50)] {
        let mut b = vec![0u8; len];
        let n = fs.read_inode(ino, off as u64, &mut b).unwrap();
        assert_eq!(&b[..n], &all[off..(off + len).min(all.len())]);
    }
    fs.release_inode(ino);
    clean(fs, &img, &dir, "large file");
}

#[test]
fn refuses_writes_where_it_cannot_do_them_safely() {
    if !tools_available() {
        return;
    }
    // ext3 (no extents): readable, not writable.
    let (dir, img) = fresh(&["-t", "ext3"], 16, "ro3");
    let mut fs = mount(&img);
    assert!(fs.read_only());
    assert_eq!(fs.write_file("/x", b"1"), Err(FsError::ReadOnly));
    assert_eq!(fs.create_dir("/d"), Err(FsError::ReadOnly));
    assert_eq!(fs.delete_file("/lost+found"), Err(FsError::ReadOnly));
    assert!(fs.read_dir_at("/").is_ok());
    drop(fs);
    std::fs::remove_dir_all(&dir).ok();

    // A device that refuses writes also yields a read-only view.
    struct RoDisk(FileDisk);
    impl basicos_fs::block::BlockDevice for RoDisk {
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
    let (dir, img) = fresh(&["-O", "^has_journal"], 16, "rodev");
    let mut fs = Ext4Fs::mount(Box::new(RoDisk(FileDisk::open(&img)))).unwrap();
    assert_eq!(fs.write_file("/x", b"1"), Err(FsError::ReadOnly));
    drop(fs);
    std::fs::remove_dir_all(&dir).ok();

    // Features we can't handle are refused outright.
    let (dir, img) = fresh(&["-O", "inline_data", "-b", "4096"], 16, "inline");
    assert!(Ext4Fs::mount(Box::new(FileDisk::open(&img))).is_err(), "inline_data");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn write_file_replaces_and_shrinks() {
    if !tools_available() {
        return;
    }
    let (dir, img) = fresh(&[], 16, "replace");
    let mut fs = mount(&img);
    let free0 = fs.space().1;
    fs.write_file("/f", &vec![1u8; 200_000]).unwrap();
    fs.write_file("/f", b"tiny").unwrap();
    assert_eq!(fs.read_file_at("/f").unwrap(), b"tiny");
    assert_eq!(fs.find("/f").unwrap().size, 4);
    fs.delete_file("/f").unwrap();
    assert_eq!(fs.space().1, free0, "no blocks leaked");
    clean(fs, &img, &dir, "replace");
}
