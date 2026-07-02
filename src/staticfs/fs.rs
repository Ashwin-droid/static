//! `fuser::Filesystem` implementation (contract B3): synthetic inode table
//! with dynamic-children support, FOPEN_DIRECT_IO on every open, per-handle
//! write line-buffers, trigger-file semantics, truncate-as-no-op setattr and
//! the FsError -> errno mapping.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use fuser::{
    AccessFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, LockOwner, OpenAccMode, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};

use super::tree::Node;
use super::{Ctx, FsError};

/// Attr/entry TTL: contents are fully dynamic, so the kernel must re-ask us
/// every time (this is also what makes `tail -f playback/lyrics` poll work).
const TTL: Duration = Duration::ZERO;

const BLKSIZE: u32 = 4096;

fn errno_of(err: &FsError) -> Errno {
    match err {
        FsError::Invalid(message) => {
            tracing::debug!("staticfs EINVAL: {message}");
            Errno::EINVAL
        }
        FsError::Io(message) => {
            tracing::debug!("staticfs EIO: {message}");
            Errno::EIO
        }
        FsError::NotFound => Errno::ENOENT,
    }
}

/// Bidirectional ino <-> node map. The fixed tree is preallocated at mount;
/// dynamic nodes (settings keys, stats track dirs/files) are assigned inodes
/// on first lookup/readdir and cached for the life of the mount.
/// Generation is always 0.
struct InodeTable {
    by_ino: HashMap<u64, Node>,
    by_node: HashMap<Node, u64>,
    next_ino: u64,
}

impl InodeTable {
    fn new() -> Self {
        let mut table = Self {
            by_ino: HashMap::new(),
            by_node: HashMap::new(),
            next_ino: 1,
        };
        table.preallocate(&Node::Root);
        table
    }

    fn preallocate(&mut self, node: &Node) {
        self.get_or_assign(node);
        for (_, child) in node.static_children() {
            self.preallocate(&child);
        }
    }

    fn get_or_assign(&mut self, node: &Node) -> u64 {
        if let Some(&ino) = self.by_node.get(node) {
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.by_ino.insert(ino, node.clone());
        self.by_node.insert(node.clone(), ino);
        ino
    }

    fn node(&self, ino: u64) -> Option<Node> {
        self.by_ino.get(&ino).cloned()
    }
}

/// Per-open-handle write state (contract B3): bytes accumulate in `buf`;
/// every complete newline-terminated line is processed as one command as it
/// arrives, the unterminated remainder at flush/release. The first failing
/// line records its errno, aborts the rest of the buffered input and is
/// returned on that write()/flush(). Trigger files fire once per handle on
/// the first write() call, whatever its contents. `wrote`/`saw_content`
/// track whether any write() happened and whether any line had
/// non-whitespace content, so an ENTIRELY empty write to playback/state
/// fires exactly one TogglePause at flush/release (contract B5) while blank
/// lines inside non-empty writes stay plain no-ops (contract B3).
struct HandleState {
    node: Node,
    buf: Vec<u8>,
    triggered: bool,
    failed: Option<Errno>,
    /// At least one write() call reached this handle (pure truncate /
    /// open-close never counts as an empty write).
    wrote: bool,
    /// Some processed line had non-whitespace content.
    saw_content: bool,
}

pub(crate) struct StaticFs {
    ctx: Ctx,
    inodes: Mutex<InodeTable>,
    handles: Mutex<HashMap<u64, HandleState>>,
    next_fh: AtomicU64,
    mount_time: SystemTime,
    uid: u32,
    gid: u32,
}

impl StaticFs {
    pub(crate) fn new(ctx: Ctx) -> Self {
        Self {
            ctx,
            inodes: Mutex::new(InodeTable::new()),
            handles: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            mount_time: SystemTime::now(),
            // SAFETY-free libc calls: geteuid/getegid cannot fail.
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
        }
    }

    fn node_of(&self, ino: INodeNo) -> Option<Node> {
        self.inodes
            .lock()
            .expect("inode table mutex poisoned")
            .node(ino.0)
    }

    fn assign_ino(&self, node: &Node) -> u64 {
        self.inodes
            .lock()
            .expect("inode table mutex poisoned")
            .get_or_assign(node)
    }

    /// getattr computes the CURRENT content and reports its exact byte
    /// length (cheap: one D-Bus GetState / one SQL query per read). Errors
    /// while rendering surface as the mapped errno.
    fn content_len(&self, node: &Node) -> Result<u64, Errno> {
        if node.is_dir() || !node.readable() {
            return Ok(0);
        }
        node.read(&self.ctx)
            .map(|bytes| bytes.len() as u64)
            .map_err(|err| errno_of(&err))
    }

    fn attr_for(&self, ino: u64, node: &Node, size: u64) -> FileAttr {
        let kind = if node.is_dir() {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        // Timestamps: mount time for static directories, "now" for dynamic
        // content nodes (files regenerate on every read).
        let timestamp = if node.is_dir() {
            self.mount_time
        } else {
            SystemTime::now()
        };
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: timestamp,
            mtime: timestamp,
            ctime: timestamp,
            crtime: self.mount_time,
            kind,
            perm: node.perm(),
            nlink: if node.is_dir() { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: BLKSIZE,
            flags: 0,
        }
    }

    /// Processes every complete newline-terminated line currently buffered
    /// for the handle; on the first failure records the errno, drops the
    /// rest of the buffer and returns the error.
    fn drain_complete_lines(&self, handle: &mut HandleState) -> Result<(), Errno> {
        while let Some(newline_at) = handle.buf.iter().position(|&byte| byte == b'\n') {
            let mut line: Vec<u8> = handle.buf.drain(..=newline_at).collect();
            line.pop();
            match self.process_line(&handle.node, &line) {
                Ok(saw_content) => handle.saw_content |= saw_content,
                Err(errno) => {
                    handle.failed = Some(errno);
                    handle.buf.clear();
                    return Err(errno);
                }
            }
        }
        Ok(())
    }

    /// Processes the unterminated remainder (at flush/release), then fires
    /// the node's empty-write command (playback/state -> TogglePause,
    /// contract B5) when the handle was written but never carried any
    /// non-whitespace content — once per handle, via the same `triggered`
    /// guard the trigger files use.
    fn drain_remainder(&self, handle: &mut HandleState) -> Result<(), Errno> {
        if handle.failed.is_some() {
            handle.buf.clear();
            return Ok(());
        }
        if !handle.buf.is_empty() {
            let line = std::mem::take(&mut handle.buf);
            match self.process_line(&handle.node, &line) {
                Ok(saw_content) => handle.saw_content |= saw_content,
                Err(errno) => {
                    handle.failed = Some(errno);
                    return Err(errno);
                }
            }
        }
        if let Some(command) = pending_empty_write_command(handle) {
            handle.triggered = true;
            if let Err(err) = handle.node.write_line(&self.ctx, command) {
                let errno = errno_of(&err);
                handle.failed = Some(errno);
                return Err(errno);
            }
        }
        Ok(())
    }

    /// One buffered line -> one command. Empty (after trim) lines are plain
    /// no-ops per contract B3 (an ENTIRELY empty write is handled at
    /// flush/release via `pending_empty_write_command` instead); non-UTF-8
    /// input is EINVAL. Returns whether the line had non-whitespace content.
    fn process_line(&self, node: &Node, line: &[u8]) -> Result<bool, Errno> {
        let Ok(text) = std::str::from_utf8(line) else {
            tracing::debug!("staticfs EINVAL: write is not valid UTF-8");
            return Err(Errno::EINVAL);
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(false);
        }
        node.write_line(&self.ctx, trimmed)
            .map(|()| true)
            .map_err(|err| errno_of(&err))
    }
}

/// The empty-write command this handle still owes at flush/release, if any:
/// contract B5 makes an entirely empty/whitespace write to playback/state
/// one TogglePause (`echo > playback/state`), fired exactly once per handle
/// and only when at least one write() actually happened. Handles that saw
/// any non-whitespace line, already fired, or failed never fire (blank
/// lines inside non-empty writes are plain no-ops per contract B3).
fn pending_empty_write_command(handle: &HandleState) -> Option<&'static str> {
    if !handle.wrote || handle.saw_content || handle.triggered || handle.failed.is_some() {
        return None;
    }
    handle.node.empty_write_command()
}

impl Filesystem for StaticFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_node) = self.node_of(parent) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::ENOENT);
            return;
        };
        match parent_node.lookup_child(&self.ctx, name) {
            Ok(node) => {
                let ino = self.assign_ino(&node);
                match self.content_len(&node) {
                    Ok(size) => {
                        reply.entry(&TTL, &self.attr_for(ino, &node, size), Generation(0));
                    }
                    Err(errno) => reply.error(errno),
                }
            }
            Err(err) => reply.error(errno_of(&err)),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(node) = self.node_of(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.content_len(&node) {
            Ok(size) => reply.attr(&TTL, &self.attr_for(ino.0, &node, size)),
            Err(errno) => reply.error(errno),
        }
    }

    /// setattr with size (O_TRUNC / truncate from `>` redirection) succeeds
    /// as a no-op on writable files; everything else (times from `touch`,
    /// ...) is accepted as a no-op too.
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let Some(node) = self.node_of(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if size.is_some() && !node.writable() {
            reply.error(Errno::EACCES);
            return;
        }
        match self.content_len(&node) {
            Ok(current) => reply.attr(&TTL, &self.attr_for(ino.0, &node, current)),
            Err(_) => reply.attr(&TTL, &self.attr_for(ino.0, &node, 0)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let Some(node) = self.node_of(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if node.is_dir() {
            reply.error(Errno::EISDIR);
            return;
        }
        let allowed = match flags.acc_mode() {
            OpenAccMode::O_RDONLY => node.readable(),
            OpenAccMode::O_WRONLY => node.writable(),
            OpenAccMode::O_RDWR => node.readable() && node.writable(),
        };
        if !allowed {
            reply.error(Errno::EACCES);
            return;
        }
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.handles
            .lock()
            .expect("handle table mutex poisoned")
            .insert(
                fh,
                HandleState {
                    node,
                    buf: Vec::new(),
                    triggered: false,
                    failed: None,
                    wrote: false,
                    saw_content: false,
                },
            );
        // Contents are dynamic: force direct IO on every open so read/write
        // sizes come straight from us instead of the page cache.
        reply.opened(FileHandle(fh), FopenFlags::FOPEN_DIRECT_IO);
    }

    /// read regenerates the content fresh and serves
    /// bytes[offset..offset+size]; offset past EOF yields empty data. The
    /// slicing is byte-based and clamped, so it never panics (no mid-slice
    /// UTF-8 requirement).
    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let Some(node) = self.node_of(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if !node.readable() {
            reply.error(Errno::EACCES);
            return;
        }
        match node.read(&self.ctx) {
            Ok(bytes) => {
                let len = bytes.len() as u64;
                if offset >= len {
                    reply.data(&[]);
                    return;
                }
                let start = offset as usize;
                let end = usize::min(bytes.len(), start.saturating_add(size as usize));
                reply.data(&bytes[start..end]);
            }
            Err(err) => reply.error(errno_of(&err)),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let mut handles = self.handles.lock().expect("handle table mutex poisoned");
        let Some(handle) = handles.get_mut(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        if handle.node.is_trigger() {
            // ANY write — even empty or "\n" from `echo` — fires the trigger
            // exactly once per open handle.
            if handle.triggered {
                reply.written(data.len() as u32);
                return;
            }
            handle.triggered = true;
            match handle.node.trigger(&self.ctx) {
                Ok(()) => reply.written(data.len() as u32),
                Err(err) => reply.error(errno_of(&err)),
            }
            return;
        }
        if !handle.node.writable() {
            reply.error(Errno::EACCES);
            return;
        }
        if let Some(errno) = handle.failed {
            reply.error(errno);
            return;
        }
        handle.wrote = true;
        handle.buf.extend_from_slice(data);
        match self.drain_complete_lines(handle) {
            Ok(()) => reply.written(data.len() as u32),
            Err(errno) => reply.error(errno),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let mut handles = self.handles.lock().expect("handle table mutex poisoned");
        let Some(handle) = handles.get_mut(&fh.0) else {
            reply.ok();
            return;
        };
        match self.drain_remainder(handle) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let mut handles = self.handles.lock().expect("handle table mutex poisoned");
        if let Some(mut handle) = handles.remove(&fh.0) {
            // flush is not guaranteed; process any unterminated remainder
            // here. Errors cannot reach close(), so they are only logged.
            let _ = self.drain_remainder(&mut handle);
        }
        reply.ok();
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(node) = self.node_of(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if !node.is_dir() {
            reply.error(Errno::ENOTDIR);
            return;
        }
        let children = match node.children(&self.ctx) {
            Ok(children) => children,
            Err(err) => {
                reply.error(errno_of(&err));
                return;
            }
        };
        let parent_ino = self.assign_ino(&node.parent());
        let mut entries: Vec<(u64, FileType, String)> = Vec::with_capacity(children.len() + 2);
        entries.push((ino.0, FileType::Directory, ".".to_string()));
        entries.push((parent_ino, FileType::Directory, "..".to_string()));
        for (name, child) in children {
            let child_ino = self.assign_ino(&child);
            let kind = if child.is_dir() {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            entries.push((child_ino, kind, name));
        }
        for (index, (entry_ino, kind, name)) in
            entries.into_iter().enumerate().skip(offset as usize)
        {
            if reply.add(INodeNo(entry_ino), (index + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    /// Access checks follow the per-node permission bits (fuser's default
    /// replies ENOSYS, which the kernel treats as allow-all but logs a
    /// warning per call).
    fn access(&self, _req: &Request, ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
        let Some(node) = self.node_of(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let allowed = (!mask.contains(AccessFlags::R_OK) || node.readable())
            && (!mask.contains(AccessFlags::W_OK) || node.writable())
            && (!mask.contains(AccessFlags::X_OK) || node.is_dir());
        if allowed {
            reply.ok();
        } else {
            reply.error(Errno::EACCES);
        }
    }

    /// No extended attributes anywhere in the tree (quietly, instead of the
    /// default's per-call ENOSYS warning).
    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(Errno::ENODATA);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }
}

#[cfg(test)]
#[path = "tests/fs.rs"]
mod tests;
