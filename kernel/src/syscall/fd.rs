//! Per-process file descriptor tables and the objects behind them.
//!
//! An fd is an index into the calling process's table; each slot points
//! at a shared, reference-counted *open file description* (`Handle`) —
//! offset, status flags, and the object itself. `dup`, `dup2` and `fork`
//! copy the pointer, not the description, so duplicated fds share one
//! file offset and one pipe end, exactly as POSIX says. The description
//! is torn down (files flushed to disk, pipe ends and UDP ports
//! released) when its last fd goes away.
//!
//! Tables are keyed by scheduler task slot and hooked into `sched`
//! (`reset_slot` on spawn, `on_fork`, `on_exec`, `on_exit`).
//!
//! Locking: the table lock is only held to look up or edit slots, never
//! across I/O or a blocking wait — callers grab a `Handle` clone and
//! release it first.

use super::{console, errno, open_flags as of};
use crate::sync::Spinlock;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

pub const MAX_FDS: usize = 64;

#[cfg(feature = "scheduler")]
const SLOTS: usize = crate::sched::MAX_TASKS;
#[cfg(not(feature = "scheduler"))]
const SLOTS: usize = 1;

fn current_slot() -> usize {
    #[cfg(feature = "scheduler")]
    return crate::sched::current_index();
    #[cfg(not(feature = "scheduler"))]
    return 0;
}

// ---------------------------------------------------------------------------
// Objects
// ---------------------------------------------------------------------------

/// A pipe: a bounded byte queue plus how many read/write ends exist.
pub struct Pipe {
    st: Spinlock<PipeState>,
}

struct PipeState {
    buf: VecDeque<u8>,
    readers: usize,
    writers: usize,
}

const PIPE_CAPACITY: usize = 16 * 1024;

impl Pipe {
    fn pair() -> (Object, Object) {
        let pipe = Arc::new(Pipe {
            st: Spinlock::new(PipeState { buf: VecDeque::new(), readers: 1, writers: 1 }),
        });
        (Object::PipeRead(pipe.clone()), Object::PipeWrite(pipe))
    }
}

/// How a [`File`]'s bytes are reached.
#[cfg(feature = "storage")]
pub enum Backing {
    /// The whole file in memory: read at `open`, written back on
    /// `fsync`/last `close`. What FAT32 uses (it has no partial update).
    Mem { data: Vec<u8>, dirty: bool },
    /// Straight to the filesystem by inode number — no buffering, so a
    /// 2 GiB file costs no memory, and every write is on the volume
    /// (subject to its own cache) immediately. ext4 uses this; the inode
    /// stays pinned (even if unlinked) until this description closes.
    Stream { ino: u64 },
}

#[cfg(feature = "storage")]
pub struct File {
    pub drive: crate::fs::Drive,
    /// Drive-relative, normalized.
    pub path: String,
    pub pos: usize,
    pub backing: Backing,
}

#[cfg(feature = "storage")]
impl File {
    pub fn is_stream(&self) -> bool {
        matches!(self.backing, Backing::Stream { .. })
    }

    fn fs_err(e: crate::fs::FsError) -> i64 {
        super::file::fs_err(e)
    }

    pub fn size(&mut self) -> Result<u64, i64> {
        match &self.backing {
            Backing::Mem { data, .. } => Ok(data.len() as u64),
            Backing::Stream { ino } => {
                let ino = *ino;
                crate::fs::with_drive(self.drive, |fs| fs.size_inode(ino))
                    .ok_or(errno::ENODEV)?
                    .map_err(Self::fs_err)
            }
        }
    }

    /// Read up to `buf.len()` bytes at `off`.
    pub fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        match &self.backing {
            Backing::Mem { data, .. } => {
                let off = off as usize;
                let n = buf.len().min(data.len().saturating_sub(off));
                buf[..n].copy_from_slice(&data[off.min(data.len())..off.min(data.len()) + n]);
                Ok(n)
            }
            Backing::Stream { ino } => {
                let ino = *ino;
                crate::fs::with_drive(self.drive, |fs| fs.read_inode(ino, off, buf))
                    .ok_or(errno::ENODEV)?
                    .map_err(Self::fs_err)
            }
        }
    }

    /// Write `src` at `off`, extending (zero-filled) as needed.
    pub fn write_at(&mut self, off: u64, src: &[u8]) -> Result<usize, i64> {
        if super::file::volume_read_only(self.drive) {
            return Err(errno::EROFS);
        }
        match &mut self.backing {
            Backing::Mem { data, dirty } => {
                let off = off as usize;
                let end = off + src.len();
                if data.len() < end {
                    data.resize(end, 0);
                }
                data[off..end].copy_from_slice(src);
                *dirty = true;
                Ok(src.len())
            }
            Backing::Stream { ino } => {
                let ino = *ino;
                crate::fs::with_drive(self.drive, |fs| fs.write_inode(ino, off, src))
                    .ok_or(errno::ENODEV)?
                    .map_err(Self::fs_err)
            }
        }
    }

    pub fn truncate(&mut self, len: u64) -> Result<(), i64> {
        if super::file::volume_read_only(self.drive) {
            return Err(errno::EROFS);
        }
        match &mut self.backing {
            Backing::Mem { data, dirty } => {
                data.resize(len as usize, 0);
                *dirty = true;
                Ok(())
            }
            Backing::Stream { ino } => {
                let ino = *ino;
                crate::fs::with_drive(self.drive, |fs| fs.truncate_inode(ino, len))
                    .ok_or(errno::ENODEV)?
                    .map_err(Self::fs_err)
            }
        }
    }

    /// Push buffered changes to the volume.
    pub fn flush(&mut self) -> Result<(), i64> {
        match &mut self.backing {
            Backing::Mem { data, dirty } => {
                if !*dirty {
                    return Ok(());
                }
                let (drive, path) = (self.drive, &self.path);
                match crate::fs::with_drive(drive, |vol| vol.write_file(path, data)) {
                    Some(Ok(())) => {
                        *dirty = false;
                        Ok(())
                    }
                    Some(Err(e)) => Err(Self::fs_err(e)),
                    None => Err(errno::ENODEV),
                }
            }
            Backing::Stream { .. } => {
                crate::fs::with_drive(self.drive, |fs| fs.sync());
                Ok(())
            }
        }
    }

    /// Release what the description holds on the volume (the pinned inode).
    fn release(&mut self) {
        if let Backing::Stream { ino } = &self.backing {
            let ino = *ino;
            crate::fs::with_drive(self.drive, |fs| fs.release_inode(ino));
            self.backing = Backing::Mem { data: Vec::new(), dirty: false };
        }
    }
}

#[cfg(feature = "storage")]
pub struct Dir {
    pub drive: crate::fs::Drive,
    pub path: String,
    pub entries: Vec<crate::fs::DirEntry>,
    pub pos: usize,
}

#[cfg(feature = "net")]
pub struct Udp {
    /// 0 = not bound yet (bound to an ephemeral port on first send).
    pub port: u16,
    /// `connect()`ed peer: default destination, and the only source
    /// datagrams are accepted from.
    pub peer: Option<(core::net::Ipv4Addr, u16)>,
    /// `SO_RCVTIMEO`; 0 = wait forever.
    pub rcv_timeout_ms: u64,
}

#[cfg(feature = "net")]
pub struct Raw {
    /// The stack's handle for this socket.
    pub id: u32,
    pub protocol: u8,
    /// `connect()`ed peer: default destination, and the only source
    /// packets are accepted from.
    pub peer: Option<core::net::Ipv4Addr>,
    /// `SO_RCVTIMEO`; 0 = wait forever.
    pub rcv_timeout_ms: u64,
}

/// An open `/proc` file: its text, generated once at open.
#[cfg(feature = "storage")]
pub struct ProcFile {
    pub data: Vec<u8>,
    pub pos: usize,
}

pub enum Object {
    ConsoleIn,
    ConsoleOut,
    /// `/dev/null`
    Null,
    /// `/dev/zero`
    Zero,
    /// `/dev/random`, `/dev/urandom`
    Random,
    PipeRead(Arc<Pipe>),
    PipeWrite(Arc<Pipe>),
    #[cfg(feature = "storage")]
    File(File),
    #[cfg(feature = "storage")]
    Dir(Dir),
    #[cfg(feature = "storage")]
    Proc(ProcFile),
    #[cfg(feature = "net")]
    Udp(Udp),
    #[cfg(feature = "net")]
    Raw(Raw),
}

impl Object {
    /// Release whatever this object holds. Idempotent — `Desc`'s `Drop`
    /// calls it too, after `close` has already reported any error.
    pub fn finish(&mut self) -> Result<(), i64> {
        match self {
            Object::PipeRead(p) => {
                let mut st = p.st.lock();
                st.readers = st.readers.saturating_sub(1);
                // Mark released so a second `finish` doesn't decrement again.
                drop(st);
                *self = Object::Null;
                Ok(())
            }
            Object::PipeWrite(p) => {
                let mut st = p.st.lock();
                st.writers = st.writers.saturating_sub(1);
                drop(st);
                *self = Object::Null;
                Ok(())
            }
            #[cfg(feature = "storage")]
            Object::File(f) => {
                let r = f.flush();
                f.release();
                r
            }
            #[cfg(feature = "net")]
            Object::Udp(u) => {
                if u.port != 0 {
                    let port = u.port;
                    u.port = 0;
                    let _ = crate::net::with_stack(|s| s.unbind_udp(port));
                }
                Ok(())
            }
            #[cfg(feature = "net")]
            Object::Raw(r) => {
                let id = r.id;
                let _ = crate::net::with_stack(|s| s.close_raw(id));
                *self = Object::Null;
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// An open file description.
pub struct Desc {
    pub obj: Object,
    /// Access mode (`O_RDONLY`/`WRONLY`/`RDWR`) plus `O_APPEND` and
    /// `O_NONBLOCK`.
    pub flags: u64,
}

impl Drop for Desc {
    fn drop(&mut self) {
        let _ = self.obj.finish();
    }
}

pub type Handle = Arc<Spinlock<Desc>>;

pub fn new_handle(obj: Object, flags: u64) -> Handle {
    Arc::new(Spinlock::new(Desc { obj, flags }))
}

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Slot {
    handle: Handle,
    cloexec: bool,
}

pub struct FdTable {
    slots: [Option<Slot>; MAX_FDS],
    /// Current directory, DOS-style (`C:/some/dir`).
    pub cwd: String,
    pub umask: u32,
}

impl Clone for FdTable {
    fn clone(&self) -> Self {
        FdTable { slots: self.slots.clone(), cwd: self.cwd.clone(), umask: self.umask }
    }
}

impl FdTable {
    fn with_stdio() -> Self {
        let mut t = FdTable {
            slots: [const { None }; MAX_FDS],
            cwd: String::from("C:/"),
            umask: 0o022,
        };
        t.slots[0] = Some(Slot { handle: new_handle(Object::ConsoleIn, of::O_RDONLY), cloexec: false });
        t.slots[1] = Some(Slot { handle: new_handle(Object::ConsoleOut, of::O_WRONLY), cloexec: false });
        t.slots[2] = Some(Slot { handle: new_handle(Object::ConsoleOut, of::O_WRONLY), cloexec: false });
        t
    }

    fn free_from(&self, min: usize) -> Option<usize> {
        (min..MAX_FDS).find(|&i| self.slots[i].is_none())
    }
}

static TABLES: Spinlock<[Option<FdTable>; SLOTS]> = Spinlock::new([const { None }; SLOTS]);

fn with_table<R>(f: impl FnOnce(&mut FdTable) -> R) -> R {
    let mut tables = TABLES.lock();
    f(tables[current_slot()].get_or_insert_with(FdTable::with_stdio))
}

/// A fresh process in `slot`: forget whatever the slot's previous
/// occupant had. The new table (stdio on the console, cwd `C:/`) is
/// created lazily on first use.
pub fn reset_slot(slot: usize) {
    let old = TABLES.lock()[slot].take();
    drop(old); // closes files outside the lock
}

/// `fork`: the child's table is a copy of the parent's, sharing every
/// open file description.
pub fn on_fork(parent: usize, child: usize) {
    let old = {
        let mut tables = TABLES.lock();
        let copy = tables[parent].get_or_insert_with(FdTable::with_stdio).clone();
        tables[child].replace(copy)
    };
    drop(old);
}

/// `exit`: close everything.
pub fn on_exit(slot: usize) {
    reset_slot(slot);
}

/// `execve`: close-on-exec fds go away, the rest survive.
pub fn on_exec(slot: usize) {
    let mut closing = Vec::new();
    {
        let mut tables = TABLES.lock();
        if let Some(t) = tables[slot].as_mut() {
            for s in t.slots.iter_mut() {
                if s.as_ref().is_some_and(|x| x.cloexec) {
                    closing.push(s.take());
                }
            }
        }
    }
    drop(closing);
}

pub fn get(fd: u64) -> Result<Handle, i64> {
    with_table(|t| {
        t.slots
            .get(fd as usize)
            .and_then(|s| s.as_ref())
            .map(|s| s.handle.clone())
            .ok_or(errno::EBADF)
    })
}

pub fn install(handle: Handle, cloexec: bool) -> Result<u64, i64> {
    install_from(handle, cloexec, 0)
}

pub fn install_from(handle: Handle, cloexec: bool, min: usize) -> Result<u64, i64> {
    with_table(|t| {
        let fd = t.free_from(min).ok_or(errno::EMFILE)?;
        t.slots[fd] = Some(Slot { handle, cloexec });
        Ok(fd as u64)
    })
}

pub fn close(fd: u64) -> i64 {
    let slot = with_table(|t| t.slots.get_mut(fd as usize).and_then(Option::take));
    let Some(slot) = slot else { return errno::EBADF };
    // Last reference: flush now so a write-back failure is reported by
    // `close` itself rather than swallowed by the destructor.
    if Arc::strong_count(&slot.handle) == 1 {
        if let Err(e) = slot.handle.lock().obj.finish() {
            return e;
        }
    }
    0
}

/// `dup`/`dup2`/`dup3`/`fcntl(F_DUPFD)`. `target = None` picks the lowest
/// free fd at or above `min`.
pub fn dup(fd: u64, target: Option<u64>, min: usize, cloexec: bool) -> i64 {
    let old = with_table(|t| {
        let Some(src) = t.slots.get(fd as usize).and_then(|s| s.clone()) else {
            return Err(errno::EBADF);
        };
        match target {
            None => {
                let new = t.free_from(min).ok_or(errno::EMFILE)?;
                t.slots[new] = Some(Slot { handle: src.handle, cloexec });
                Ok((new as i64, None))
            }
            Some(new) => {
                if new as usize >= MAX_FDS {
                    return Err(errno::EBADF);
                }
                let replaced = t.slots[new as usize].replace(Slot { handle: src.handle, cloexec });
                Ok((new as i64, replaced))
            }
        }
    });
    match old {
        Ok((new, replaced)) => {
            drop(replaced);
            new
        }
        Err(e) => e,
    }
}

pub fn get_cloexec(fd: u64) -> Result<bool, i64> {
    with_table(|t| {
        t.slots.get(fd as usize).and_then(|s| s.as_ref()).map(|s| s.cloexec).ok_or(errno::EBADF)
    })
}

pub fn set_cloexec(fd: u64, on: bool) -> Result<(), i64> {
    with_table(|t| {
        t.slots
            .get_mut(fd as usize)
            .and_then(|s| s.as_mut())
            .map(|s| s.cloexec = on)
            .ok_or(errno::EBADF)
    })
}

pub fn cwd() -> String {
    with_table(|t| t.cwd.clone())
}

pub fn set_cwd(cwd: String) {
    with_table(|t| t.cwd = cwd);
}

pub fn umask(new: u32) -> u32 {
    with_table(|t| core::mem::replace(&mut t.umask, new & 0o777))
}

pub fn open_count() -> usize {
    with_table(|t| t.slots.iter().filter(|s| s.is_some()).count())
}

// ---------------------------------------------------------------------------
// pipe()
// ---------------------------------------------------------------------------

/// `pipe2(flags)`: returns `(read_fd, write_fd)`.
pub fn make_pipe(flags: u64) -> Result<(u64, u64), i64> {
    let nonblock = flags & of::O_NONBLOCK;
    let cloexec = flags & of::O_CLOEXEC != 0;
    let (r, w) = Pipe::pair();
    let rfd = install(new_handle(r, of::O_RDONLY | nonblock), cloexec)?;
    match install(new_handle(w, of::O_WRONLY | nonblock), cloexec) {
        Ok(wfd) => Ok((rfd, wfd)),
        Err(e) => {
            close(rfd);
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Reading and writing
// ---------------------------------------------------------------------------

/// A snapshot of what a description is, taken under its lock so the
/// (possibly blocking) operation itself runs without holding it.
enum Kind {
    ConsoleIn,
    ConsoleOut,
    Null,
    Zero,
    Random,
    PipeRead(Arc<Pipe>),
    PipeWrite(Arc<Pipe>),
    File,
    Dir,
    #[cfg(feature = "storage")]
    Proc,
    #[cfg(feature = "net")]
    Udp { port: u16, peer: Option<(core::net::Ipv4Addr, u16)>, timeout_ms: u64 },
    #[cfg(feature = "net")]
    Raw { id: u32 },
}

fn kind_of(d: &Desc) -> Kind {
    match &d.obj {
        Object::ConsoleIn => Kind::ConsoleIn,
        Object::ConsoleOut => Kind::ConsoleOut,
        Object::Null => Kind::Null,
        Object::Zero => Kind::Zero,
        Object::Random => Kind::Random,
        Object::PipeRead(p) => Kind::PipeRead(p.clone()),
        Object::PipeWrite(p) => Kind::PipeWrite(p.clone()),
        #[cfg(feature = "storage")]
        Object::File(_) => Kind::File,
        #[cfg(feature = "storage")]
        Object::Dir(_) => Kind::Dir,
        #[cfg(feature = "storage")]
        Object::Proc(_) => Kind::Proc,
        #[cfg(feature = "net")]
        Object::Udp(u) => Kind::Udp { port: u.port, peer: u.peer, timeout_ms: u.rcv_timeout_ms },
        #[cfg(feature = "net")]
        Object::Raw(r) => Kind::Raw { id: r.id },
    }
}

/// Wait for `poll` to produce a value: return immediately if
/// `nonblock`, give up with `EAGAIN` after `timeout_ms` (0 = forever).
pub fn wait_for<T>(
    nonblock: bool,
    timeout_ms: u64,
    mut poll: impl FnMut() -> Option<T>,
) -> Result<T, i64> {
    let start = super::now_ms();
    loop {
        if let Some(v) = poll() {
            return Ok(v);
        }
        if nonblock || (timeout_ms != 0 && super::now_ms().saturating_sub(start) >= timeout_ms) {
            return Err(errno::EAGAIN);
        }
        super::relax();
    }
}

pub fn read(h: &Handle, buf: &mut [u8]) -> i64 {
    let (kind, flags) = {
        let d = h.lock();
        (kind_of(&d), d.flags)
    };
    if flags & of::O_ACCMODE == of::O_WRONLY {
        return errno::EBADF;
    }
    let nonblock = flags & of::O_NONBLOCK != 0;
    match kind {
        Kind::ConsoleIn => console::read(buf, nonblock),
        #[cfg(feature = "storage")]
        Kind::Proc => {
            let mut d = h.lock();
            let Object::Proc(p) = &mut d.obj else { return errno::EBADF };
            let n = (p.data.len() - p.pos).min(buf.len());
            buf[..n].copy_from_slice(&p.data[p.pos..p.pos + n]);
            p.pos += n;
            n as i64
        }
        Kind::ConsoleOut | Kind::PipeWrite(_) => errno::EBADF,
        Kind::Null => 0,
        Kind::Zero => {
            buf.fill(0);
            buf.len() as i64
        }
        Kind::Random => {
            fill_random(buf);
            buf.len() as i64
        }
        Kind::PipeRead(p) => {
            if buf.is_empty() {
                return 0;
            }
            let got = wait_for(nonblock, 0, || {
                let mut st = p.st.lock();
                if !st.buf.is_empty() {
                    let n = buf.len().min(st.buf.len());
                    for slot in buf.iter_mut().take(n) {
                        *slot = st.buf.pop_front().unwrap();
                    }
                    Some(n)
                } else if st.writers == 0 {
                    Some(0) // every write end closed: EOF
                } else {
                    None
                }
            });
            match got {
                Ok(n) => n as i64,
                Err(e) => e,
            }
        }
        #[cfg(feature = "storage")]
        Kind::File => {
            let mut d = h.lock();
            let Object::File(f) = &mut d.obj else { return errno::EBADF };
            match f.read_at(f.pos as u64, buf) {
                Ok(n) => {
                    f.pos += n;
                    n as i64
                }
                Err(e) => e,
            }
        }
        #[cfg(not(feature = "storage"))]
        Kind::File => errno::EBADF,
        Kind::Dir => errno::EISDIR,
        #[cfg(feature = "net")]
        Kind::Udp { .. } | Kind::Raw { .. } => super::sock::recv_into(h, buf, 0).map(|(n, _)| n as i64).unwrap_or_else(|e| e),
    }
}

pub fn write(h: &Handle, buf: &[u8]) -> i64 {
    let (kind, flags) = {
        let d = h.lock();
        (kind_of(&d), d.flags)
    };
    if flags & of::O_ACCMODE == of::O_RDONLY {
        return errno::EBADF;
    }
    let nonblock = flags & of::O_NONBLOCK != 0;
    match kind {
        Kind::ConsoleOut => console::write(buf),
        #[cfg(feature = "storage")]
        Kind::Proc => errno::EBADF,
        Kind::ConsoleIn | Kind::PipeRead(_) => errno::EBADF,
        Kind::Null | Kind::Zero | Kind::Random => buf.len() as i64,
        Kind::PipeWrite(p) => {
            // Blocking semantics: don't return until everything is in
            // (or the read end is gone). A non-blocking write takes what
            // fits, or EAGAIN if nothing does.
            let mut written = 0;
            while written < buf.len() {
                let step = wait_for(nonblock && written == 0, 0, || {
                    let mut st = p.st.lock();
                    if st.readers == 0 {
                        return Some(Err(errno::EPIPE));
                    }
                    let room = PIPE_CAPACITY - st.buf.len();
                    if room == 0 {
                        return None;
                    }
                    let n = room.min(buf.len() - written);
                    st.buf.extend(&buf[written..written + n]);
                    Some(Ok(n))
                });
                match step {
                    Ok(Ok(n)) => written += n,
                    Ok(Err(e)) => return if written > 0 { written as i64 } else { e },
                    Err(e) => return if written > 0 { written as i64 } else { e },
                }
                if nonblock {
                    break;
                }
            }
            written as i64
        }
        #[cfg(feature = "storage")]
        Kind::File => {
            let mut d = h.lock();
            let append = d.flags & of::O_APPEND != 0;
            let Object::File(f) = &mut d.obj else { return errno::EBADF };
            if append {
                match f.size() {
                    Ok(n) => f.pos = n as usize,
                    Err(e) => return e,
                }
            }
            match f.write_at(f.pos as u64, buf) {
                Ok(n) => {
                    f.pos += n;
                    n as i64
                }
                Err(e) => e,
            }
        }
        #[cfg(not(feature = "storage"))]
        Kind::File => errno::EBADF,
        Kind::Dir => errno::EISDIR,
        #[cfg(feature = "net")]
        Kind::Udp { .. } | Kind::Raw { .. } => super::sock::send_from(h, buf, None).unwrap_or_else(|e| e),
    }
}

/// Whether a read (`want_out = false`) or write (`true`) would complete
/// without blocking — for `poll`. Returns `(ready, hangup)`.
pub fn ready(h: &Handle, want_out: bool) -> (bool, bool) {
    let kind = kind_of(&h.lock());
    match kind {
        Kind::ConsoleIn => (!want_out && console::readable(), false),
        Kind::ConsoleOut => (want_out, false),
        Kind::Null | Kind::Zero | Kind::Random | Kind::File | Kind::Dir => (true, false),
        #[cfg(feature = "storage")]
        Kind::Proc => (!want_out, false),
        Kind::PipeRead(p) => {
            if want_out {
                return (false, false);
            }
            let st = p.st.lock();
            (!st.buf.is_empty() || st.writers == 0, st.writers == 0)
        }
        Kind::PipeWrite(p) => {
            if !want_out {
                return (false, false);
            }
            let st = p.st.lock();
            (st.buf.len() < PIPE_CAPACITY || st.readers == 0, st.readers == 0)
        }
        #[cfg(feature = "net")]
        Kind::Raw { id } => (want_out || super::sock::raw_pending(id) > 0, false),
        #[cfg(feature = "net")]
        Kind::Udp { port, .. } => {
            if want_out {
                (true, false)
            } else {
                (port != 0 && super::sock::has_data(port), false)
            }
        }
    }
}

/// For a file description: its volume and path (for `fstat` to ask the
/// filesystem about attributes).
#[cfg(feature = "storage")]
pub fn file_location(h: &Handle) -> Option<(crate::fs::Drive, String)> {
    match &h.lock().obj {
        Object::File(f) => Some((f.drive, f.path.clone())),
        _ => None,
    }
}

#[cfg(feature = "storage")]
pub fn dir_drive(h: &Handle) -> Option<crate::fs::Drive> {
    match &h.lock().obj {
        Object::Dir(d) => Some(d.drive),
        _ => None,
    }
}

/// Bytes readable right now (`FIONREAD`), where that makes sense.
pub fn bytes_available(h: &Handle) -> usize {
    match kind_of(&h.lock()) {
        Kind::ConsoleIn => console::available(),
        Kind::PipeRead(p) => p.st.lock().buf.len(),
        #[cfg(feature = "net")]
        Kind::Udp { port, .. } if port != 0 => super::sock::pending_len(port),
        #[cfg(feature = "net")]
        Kind::Raw { id } => super::sock::raw_pending(id),
        _ => 0,
    }
}

/// The `S_IFMT` bits (and a size) for `fstat`.
pub fn stat_kind(h: &Handle) -> (u32, u64) {
    let mut d = h.lock();
    match &mut d.obj {
        Object::ConsoleIn | Object::ConsoleOut => (0o020000, 0),
        Object::Null | Object::Zero | Object::Random => (0o020000, 0),
        Object::PipeRead(_) | Object::PipeWrite(_) => (0o010000, 0),
        #[cfg(feature = "storage")]
        Object::File(f) => (0o100000, f.size().unwrap_or(0)),
        #[cfg(feature = "storage")]
        Object::Dir(_) => (0o040000, 0),
        #[cfg(feature = "storage")]
        Object::Proc(p) => (0o100000, p.data.len() as u64),
        #[cfg(feature = "net")]
        Object::Udp(_) | Object::Raw(_) => (0o140000, 0),
    }
}

// ---------------------------------------------------------------------------
// Randomness (non-cryptographic: xorshift64* seeded from the TSC)
// ---------------------------------------------------------------------------

static RNG: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn fill_random(buf: &mut [u8]) {
    use core::sync::atomic::Ordering::Relaxed;
    let mut x = RNG.load(Relaxed);
    if x == 0 {
        x = unsafe { core::arch::x86_64::_rdtsc() } | 1;
    }
    for chunk in buf.chunks_mut(8) {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        let out = x.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes();
        chunk.copy_from_slice(&out[..chunk.len()]);
    }
    RNG.store(x, Relaxed);
}
