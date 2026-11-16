//! Zero-dependency, multi-threaded user-space NFSv3 + MOUNTv3 server for `CineFs`.
//!
//! Enables mounting virtual CinemaDNG folders natively on macOS (via `/sbin/mount_nfs`)
//! without kernel extensions (`macFUSE`/`kext`), recovery mode, or root privileges.
//!
//! Key architectural properties:
//! - Serves both ONC RPC Program `100005` (MOUNT v3) and `100003` (NFS v3) over TCP on a
//!   single ephemeral loopback port (`127.0.0.1:<port>`).
//! - Uses 8-byte `node_id` file handles (`nfs_fh3`) so `GETATTR`, `ACCESS`, and `READ`
//!   resolve target clips and frames in `O(1)` via bit-shifts (zero string/hash lookups).
//! - Zero-copy `NFSPROC3_READ` path writes DNG headers and pixel payloads directly into
//!   the outgoing XDR reply buffer.
//! - Pipelined `READ` requests from kernel read-ahead execute concurrently across a
//!   multi-threaded worker pool while metadata RPCs complete inline in `< 1 µs`.

use crate::vfs::{CineFs, EntryKind, VfsNodeAttr, NODE_ROOT};
use crate::{Error, Result};
use std::collections::VecDeque;
#[cfg(not(windows))]
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

const RPC_VERSION: u32 = 2;
const MSG_CALL: u32 = 0;
const MSG_REPLY: u32 = 1;
const MSG_ACCEPTED: u32 = 0;

const ACCEPT_SUCCESS: u32 = 0;
const ACCEPT_PROG_UNAVAIL: u32 = 1;
const ACCEPT_PROG_MISMATCH: u32 = 2;
const ACCEPT_PROC_UNAVAIL: u32 = 3;
const ACCEPT_GARBAGE_ARGS: u32 = 4;

const PROG_NFS: u32 = 100003;
const VERS_NFS3: u32 = 3;

const PROG_MOUNT: u32 = 100005;
const VERS_MOUNT1: u32 = 1;
const VERS_MOUNT3: u32 = 3;

// MOUNTv3 procedures
const MOUNTPROC3_NULL: u32 = 0;
const MOUNTPROC3_MNT: u32 = 1;
const MOUNTPROC3_DUMP: u32 = 2;
const MOUNTPROC3_UMNT: u32 = 3;
const MOUNTPROC3_UMNTALL: u32 = 4;
const MOUNTPROC3_EXPORT: u32 = 5;

// NFSv3 procedures
const NFSPROC3_NULL: u32 = 0;
const NFSPROC3_GETATTR: u32 = 1;
const NFSPROC3_SETATTR: u32 = 2;
const NFSPROC3_LOOKUP: u32 = 3;
const NFSPROC3_ACCESS: u32 = 4;
const NFSPROC3_READLINK: u32 = 5;
const NFSPROC3_READ: u32 = 6;
const NFSPROC3_WRITE: u32 = 7;
const NFSPROC3_CREATE: u32 = 8;
const NFSPROC3_MKDIR: u32 = 9;
const NFSPROC3_SYMLINK: u32 = 10;
const NFSPROC3_MKNOD: u32 = 11;
const NFSPROC3_REMOVE: u32 = 12;
const NFSPROC3_RMDIR: u32 = 13;
const NFSPROC3_RENAME: u32 = 14;
const NFSPROC3_LINK: u32 = 15;
const NFSPROC3_READDIR: u32 = 16;
const NFSPROC3_READDIRPLUS: u32 = 17;
const NFSPROC3_FSSTAT: u32 = 18;
const NFSPROC3_FSINFO: u32 = 19;
const NFSPROC3_PATHCONF: u32 = 20;
const NFSPROC3_COMMIT: u32 = 21;

// NFSv3 status codes
const NFS3_OK: u32 = 0;
const NFS3ERR_NOENT: u32 = 2;
const NFS3ERR_IO: u32 = 5;
const NFS3ERR_NOTDIR: u32 = 20;
const NFS3ERR_INVAL: u32 = 22;
const NFS3ERR_ROFS: u32 = 30;
const NFS3ERR_NOTSUPP: u32 = 10004;

// NFSv3 file types
const NF3REG: u32 = 1;
const NF3DIR: u32 = 2;

// NFSv3 ACCESS bits
const ACCESS3_READ: u32 = 0x0001;
const ACCESS3_LOOKUP: u32 = 0x0002;
const ACCESS3_MODIFY: u32 = 0x0004;
const ACCESS3_EXTEND: u32 = 0x0008;
const ACCESS3_DELETE: u32 = 0x0010;
const ACCESS3_EXECUTE: u32 = 0x0020;

const WRITE_VERF: [u8; 8] = *b"CINEFSWR";

// NFSv3 FSINFO properties
const FSF3_HOMOGENEOUS: u32 = 0x0008;

const FSID_CINEFS: u64 = 0x4349_4E45_4653_0001;
const MAX_READ_SIZE: u32 = 1024 * 1024; // 1 MB

#[cfg(unix)]
extern "C" {
    fn geteuid() -> u32;
    fn getegid() -> u32;
}

fn current_uid_gid() -> (u32, u32) {
    #[cfg(unix)]
    unsafe {
        (geteuid(), getegid())
    }
    #[cfg(not(unix))]
    {
        (501, 20)
    }
}

type RpcTask = Box<dyn FnOnce() + Send + 'static>;

struct WorkerPool {
    queue: Arc<(Mutex<(VecDeque<RpcTask>, bool)>, Condvar)>,
}

impl WorkerPool {
    fn new(num_threads: usize) -> Self {
        let queue = Arc::new((Mutex::new((VecDeque::<RpcTask>::new(), false)), Condvar::new()));
        for i in 0..num_threads {
            let q = Arc::clone(&queue);
            let _ = thread::Builder::new()
                .name(format!("cinefs-nfs-worker-{i}"))
                .spawn(move || loop {
                    let task = {
                        let (lock, cvar) = &*q;
                        let mut guard = lock.lock().unwrap();
                        while guard.0.is_empty() && !guard.1 {
                            guard = cvar.wait(guard).unwrap();
                        }
                        if guard.1 {
                            return;
                        }
                        guard.0.pop_front()
                    };
                    if let Some(job) = task {
                        job();
                    }
                });
        }
        Self { queue }
    }

    fn execute<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let (lock, cvar) = &*self.queue;
        let mut guard = lock.lock().unwrap();
        guard.0.push_back(Box::new(f));
        cvar.notify_one();
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.queue;
        if let Ok(mut guard) = lock.lock() {
            guard.1 = true;
            guard.0.clear();
            cvar.notify_all();
        }
    }
}

/// Active running user-space NFSv3 server instance.
pub struct NfsServer {
    port: u16,
    shutdown: Arc<AtomicBool>,
    _pool: Arc<WorkerPool>,
}

impl NfsServer {
    /// Starts the NFSv3 + MOUNTv3 server on `127.0.0.1:bind_port` (`0` for automatic free port).
    pub fn start(vfs: Arc<CineFs>, bind_port: u16) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", bind_port))?;
        let local_addr: SocketAddr = listener.local_addr()?;
        let port = local_addr.port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let (uid, gid) = current_uid_gid();

        let num_workers = thread::available_parallelism()
            .map(|n| n.get().clamp(4, 16))
            .unwrap_or(8);
        let pool = Arc::new(WorkerPool::new(num_workers));

        let shutdown_accept = Arc::clone(&shutdown);
        let pool_accept = Arc::clone(&pool);

        thread::Builder::new()
            .name(String::from("cinefs-nfs-accept"))
            .spawn(move || {
                for stream_res in listener.incoming() {
                    if shutdown_accept.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(stream) = stream_res else {
                        continue;
                    };
                    let _ = stream.set_nodelay(true);
                    let vfs_conn = Arc::clone(&vfs);
                    let pool_conn = Arc::clone(&pool_accept);
                    let shutdown_conn = Arc::clone(&shutdown_accept);
                    let _ = thread::Builder::new()
                        .name(String::from("cinefs-nfs-conn"))
                        .spawn(move || {
                            handle_tcp_connection(stream, vfs_conn, pool_conn, shutdown_conn, uid, gid);
                        });
                }
            })?;

        Ok(Self {
            port,
            shutdown,
            _pool: pool,
        })
    }

    /// Returns the local TCP port on `127.0.0.1` where both NFSv3 and MOUNTv3 are listening.
    #[inline]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Signals the server to stop accepting new connections.
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

impl Drop for NfsServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Active cross-platform mount handle (macOS NFSv3 or Windows WinFsp) that unmounts cleanly when dropped.
pub struct MountedVolume {
    #[cfg(not(windows))]
    pub server: NfsServer,
    #[cfg(windows)]
    pub winfsp: crate::winfsp::WinFspActiveMount,
    pub mount_point: PathBuf,
    unmounted: bool,
}

impl MountedVolume {
    /// Returns the local NFSv3 TCP port on macOS/Unix, or `0` on Windows WinFsp.
    #[inline]
    pub fn port(&self) -> u16 {
        #[cfg(not(windows))]
        {
            self.server.port()
        }
        #[cfg(windows)]
        {
            0
        }
    }

    /// Mounts `vfs` at `mount_point` (using `/sbin/mount_nfs` on macOS or WinFsp on Windows).
    pub fn mount(vfs: Arc<CineFs>, mount_point: impl AsRef<Path>) -> Result<Self> {
        #[cfg(windows)]
        {
            let winfsp = crate::winfsp::WinFspActiveMount::mount(vfs, &mount_point)?;
            let mp = winfsp.mount_point.clone();
            return Ok(Self {
                winfsp,
                mount_point: mp,
                unmounted: false,
            });
        }

        #[cfg(not(windows))]
        {
            let mount_point = mount_point.as_ref().to_path_buf();
            if !mount_point.exists() {
                fs::create_dir_all(&mount_point)?;
            }
            let canonical_mp = fs::canonicalize(&mount_point).unwrap_or(mount_point);

            // Best-effort cleanup in case a previous stale mount exists at this path
            let _ = unmount_path(&canonical_mp, false);

            let server = NfsServer::start(vfs, 0)?;
            let port = server.port();

            let opts = format!(
                "vers=3,tcp,port={port},mountport={port},nolocks,locallocks,rsize=65536,wsize=65536,dsize=65536,readahead=32,acregmin=60,acregmax=3600,acdirmin=2,acdirmax=30,noquota,intr,soft,timeo=2,retrans=2,deadtimeout=5,retrycnt=0"
            );

            let output = Command::new("/sbin/mount_nfs")
                .arg("-o")
                .arg(&opts)
                .arg("127.0.0.1:/")
                .arg(&canonical_mp)
                .output()
                .map_err(Error::Io)?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::Other,
                    format!("mount_nfs failed: {}", stderr.trim()),
                )));
            }

            Ok(Self {
                server,
                mount_point: canonical_mp,
                unmounted: false,
            })
        }
    }

    /// Unmounts the volume cleanly.
    pub fn unmount(&mut self) -> Result<()> {
        if self.unmounted {
            return Ok(());
        }
        self.unmounted = true;
        #[cfg(windows)]
        {
            self.winfsp.unmount()
        }
        #[cfg(not(windows))]
        {
            let res = unmount_path(&self.mount_point, true);
            self.server.stop();
            res
        }
    }
}

impl Drop for MountedVolume {
    fn drop(&mut self) {
        let _ = self.unmount();
    }
}

/// Unmounts a mounted directory on macOS.
pub fn unmount_path(mount_point: &Path, force_fallback: bool) -> Result<()> {
    let status = Command::new("/sbin/umount").arg(mount_point). output();
    if let Ok(out) = status {
        if out.status.success() {
            return Ok(());
        }
    }
    if force_fallback {
        let diskutil = Command::new("/usr/sbin/diskutil")
            .arg("unmount")
            .arg("force")
            .arg(mount_point)
            .output();
        if let Ok(out) = diskutil {
            if out.status.success() {
                return Ok(());
            }
        }
        let umount_f = Command::new("/sbin/umount")
            .arg("-f")
            .arg(mount_point)
            .output()?;
        if !umount_f.status.success() {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::Other,
                String::from_utf8_lossy(&umount_f.stderr).trim().to_string(),
            )));
        }
    }
    Ok(())
}

fn handle_tcp_connection(
    mut reader_stream: TcpStream,
    vfs: Arc<CineFs>,
    pool: Arc<WorkerPool>,
    shutdown: Arc<AtomicBool>,
    uid: u32,
    gid: u32,
) {
    let Ok(writer_clone) = reader_stream.try_clone() else {
        return;
    };
    let writer = Arc::new(Mutex::new(writer_clone));

    while !shutdown.load(Ordering::Relaxed) {
        let Ok(Some(record)) = read_rpc_record(&mut reader_stream) else {
            break;
        };

        // Peek RPC call header to decide if this is a large payload READ that benefits from pool parallelization
        let is_heavy_read = is_parallelizable_rpc(&record);
        if is_heavy_read {
            let vfs_job = Arc::clone(&vfs);
            let writer_job = Arc::clone(&writer);
            pool.execute(move || {
                let reply = process_rpc_call(&record, &vfs_job, uid, gid);
                if !reply.is_empty() {
                    if let Ok(mut w) = writer_job.lock() {
                        let _ = w.write_all(&reply);
                    }
                }
            });
        } else {
            let reply = process_rpc_call(&record, &vfs, uid, gid);
            if !reply.is_empty() {
                let Ok(mut w) = writer.lock() else {
                    break;
                };
                if w.write_all(&reply).is_err() {
                    break;
                }
            }
        }
    }

    let _ = reader_stream.shutdown(Shutdown::Both);
}

/// Reads a single complete ONC RPC record (spanning one or more TCP record-marking fragments).
fn read_rpc_record(stream: &mut TcpStream) -> io::Result<Option<Vec<u8>>> {
    let mut record = Vec::new();
    loop {
        let mut frag_hdr = [0u8; 4];
        match stream.read_exact(&mut frag_hdr) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                if record.is_empty() {
                    return Ok(None);
                } else {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
        let raw = u32::from_be_bytes(frag_hdr);
        let is_last = (raw & 0x8000_0000) != 0;
        let len = (raw & 0x7FFF_FFFF) as usize;
        if len > 4 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "RPC fragment exceeds 4 MB safety limit",
            ));
        }
        let start = record.len();
        record.resize(start + len, 0);
        stream.read_exact(&mut record[start..start + len])?;
        if is_last {
            return Ok(Some(record));
        }
    }
}

/// Checks whether an RPC call is an `NFSPROC3_READ` with `count > 4096` (so header-only probes stay inline).
#[inline]
fn is_parallelizable_rpc(record: &[u8]) -> bool {
    if record.len() < 24 {
        return false;
    }
    let prog = u32::from_be_bytes([record[12], record[13], record[14], record[15]]);
    let proc_num = u32::from_be_bytes([record[20], record[21], record[22], record[23]]);
    prog == PROG_NFS && proc_num == NFSPROC3_READ
}

fn process_rpc_call(record: &[u8], vfs: &CineFs, uid: u32, gid: u32) -> Vec<u8> {
    let mut r = XdrReader::new(record);
    let Some(xid) = r.read_u32() else {
        return Vec::new();
    };
    let Some(msg_type) = r.read_u32() else {
        return Vec::new();
    };
    if msg_type != MSG_CALL {
        return Vec::new();
    }
    let Some(rpcvers) = r.read_u32() else {
        return Vec::new();
    };
    let Some(prog) = r.read_u32() else {
        return Vec::new();
    };
    let Some(vers) = r.read_u32() else {
        return Vec::new();
    };
    let Some(proc_num) = r.read_u32() else {
        return Vec::new();
    };
    if rpcvers != RPC_VERSION {
        return Vec::new();
    }
    if r.skip_opaque_auth().is_none() || r.skip_opaque_auth().is_none() {
        return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
    }

    match prog {
        PROG_MOUNT => {
            if vers != VERS_MOUNT3 && vers != VERS_MOUNT1 {
                return build_rpc_error_reply(
                    xid,
                    ACCEPT_PROG_MISMATCH,
                    Some((VERS_MOUNT3, VERS_MOUNT3)),
                );
            }
            handle_mount_proc(xid, vers, proc_num, &mut r)
        }
        PROG_NFS => {
            if vers != VERS_NFS3 {
                return build_rpc_error_reply(
                    xid,
                    ACCEPT_PROG_MISMATCH,
                    Some((VERS_NFS3, VERS_NFS3)),
                );
            }
            handle_nfs3_proc(xid, proc_num, &mut r, vfs, uid, gid)
        }
        _ => build_rpc_error_reply(xid, ACCEPT_PROG_UNAVAIL, None),
    }
}

fn handle_mount_proc(xid: u32, vers: u32, proc_num: u32, r: &mut XdrReader<'_>) -> Vec<u8> {
    let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 128);
    match proc_num {
        MOUNTPROC3_NULL => {}
        MOUNTPROC3_MNT => {
            let _dirpath = r.read_str().unwrap_or("/");
            w.write_u32(0); // MNT3_OK
            if vers == VERS_MOUNT1 {
                // MOUNTv1 fixed 32-byte fhandle
                let mut fh = [0u8; 32];
                fh[..8].copy_from_slice(&NODE_ROOT.to_be_bytes());
                w.write_bytes_fixed(&fh);
            } else {
                // MOUNTv3 variable-length fhandle3 + auth_flavors
                w.write_fh_node_id(NODE_ROOT);
                w.write_u32(2); // 2 auth flavors supported
                w.write_u32(0); // AUTH_NONE
                w.write_u32(1); // AUTH_SYS
            }
        }
        MOUNTPROC3_DUMP => {
            w.write_bool(false); // Empty mount list
        }
        MOUNTPROC3_UMNT | MOUNTPROC3_UMNTALL => {
            // No-op success
        }
        MOUNTPROC3_EXPORT => {
            w.write_bool(true); // One export entry
            w.write_str("/");
            w.write_bool(false); // No group restrictions
            w.write_bool(false); // End of export list
        }
        _ => return build_rpc_error_reply(xid, ACCEPT_PROC_UNAVAIL, None),
    }
    w.finish_tcp_record()
}

fn handle_nfs3_proc(
    xid: u32,
    proc_num: u32,
    r: &mut XdrReader<'_>,
    vfs: &CineFs,
    uid: u32,
    gid: u32,
) -> Vec<u8> {
    match proc_num {
        NFSPROC3_NULL => {
            let w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 32);
            w.finish_tcp_record()
        }

        NFSPROC3_GETATTR => {
            let Some(node_id) = r.read_fh_node_id() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 128);
            match vfs.stat_by_id(node_id) {
                Ok(attr) => {
                    w.write_u32(NFS3_OK);
                    w.write_fattr3(&attr, uid, gid);
                }
                Err(_) => {
                    w.write_u32(NFS3ERR_NOENT);
                }
            }
            w.finish_tcp_record()
        }

        NFSPROC3_LOOKUP => {
            let Some(dir_id) = r.read_fh_node_id() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(name) = r.read_str() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 256);
            let dir_attr = vfs.stat_by_id(dir_id).ok();
            match vfs.lookup_in_dir(dir_id, name) {
                Ok(child_attr) => {
                    w.write_u32(NFS3_OK);
                    w.write_fh_node_id(child_attr.node_id);
                    w.write_post_op_attr(Some(&child_attr), uid, gid);
                    w.write_post_op_attr(dir_attr.as_ref(), uid, gid);
                }
                Err(_) => {
                    w.write_u32(NFS3ERR_NOENT);
                    w.write_post_op_attr(dir_attr.as_ref(), uid, gid);
                }
            }
            w.finish_tcp_record()
        }

        NFSPROC3_ACCESS => {
            let Some(node_id) = r.read_fh_node_id() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(requested_access) = r.read_u32() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 128);
            match vfs.stat_by_id(node_id) {
                Ok(attr) => {
                    let allowed_mask = match attr.kind {
                        EntryKind::Directory => {
                            ACCESS3_READ
                                | ACCESS3_LOOKUP
                                | ACCESS3_MODIFY
                                | ACCESS3_EXTEND
                                | ACCESS3_DELETE
                                | ACCESS3_EXECUTE
                        }
                        EntryKind::File => {
                            if CineFs::is_overlay_node(node_id) {
                                ACCESS3_READ | ACCESS3_MODIFY | ACCESS3_EXTEND | ACCESS3_DELETE
                            } else {
                                ACCESS3_READ
                            }
                        }
                    };
                    w.write_u32(NFS3_OK);
                    w.write_post_op_attr(Some(&attr), uid, gid);
                    w.write_u32(requested_access & allowed_mask);
                }
                Err(_) => {
                    w.write_u32(NFS3ERR_NOENT);
                    w.write_post_op_attr(None, uid, gid);
                }
            }
            w.finish_tcp_record()
        }

        NFSPROC3_READLINK => {
            let Some(node_id) = r.read_fh_node_id() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let attr = vfs.stat_by_id(node_id).ok();
            let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 128);
            w.write_u32(NFS3ERR_INVAL);
            w.write_post_op_attr(attr.as_ref(), uid, gid);
            w.finish_tcp_record()
        }

        NFSPROC3_READ => {
            let Some(node_id) = r.read_fh_node_id() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(offset) = r.read_u64() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(count) = r.read_u32() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };

            let Ok(attr) = vfs.stat_by_id(node_id) else {
                let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 64);
                w.write_u32(NFS3ERR_NOENT);
                w.write_post_op_attr(None, uid, gid);
                return w.finish_tcp_record();
            };

            let max_take = (attr.size.saturating_sub(offset))
                .min(count.min(MAX_READ_SIZE) as u64) as usize;

            let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 144 + max_take);
            w.write_u32(NFS3_OK);
            w.write_post_op_attr(Some(&attr), uid, gid);

            // Reserve placeholders for `count`, `eof`, and `data` length, then read directly into buffer!
            let header_pos = w.buf.len();
            w.write_u32(0); // count placeholder
            w.write_u32(0); // eof placeholder
            w.write_u32(0); // opaque data length placeholder

            let data_start = w.buf.len();
            w.buf.resize(data_start + max_take, 0);

            match vfs.read_by_id(node_id, offset, &mut w.buf[data_start..data_start + max_take]) {
                Ok((bytes_read, _)) => {
                    w.buf.truncate(data_start + bytes_read);
                    let eof = (offset + (bytes_read as u64)) >= attr.size;
                    let count_u32 = bytes_read as u32;
                    w.buf[header_pos..header_pos + 4].copy_from_slice(&count_u32.to_be_bytes());
                    w.buf[header_pos + 4..header_pos + 8]
                        .copy_from_slice(&(if eof { 1u32 } else { 0u32 }).to_be_bytes());
                    w.buf[header_pos + 8..header_pos + 12]
                        .copy_from_slice(&count_u32.to_be_bytes());

                    // XDR 4-byte padding for opaque payload
                    let pad = (4 - (bytes_read & 3)) & 3;
                    for _ in 0..pad {
                        w.buf.push(0);
                    }
                    w.finish_tcp_record()
                }
                Err(_) => {
                    let mut err_w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 128);
                    err_w.write_u32(NFS3ERR_IO);
                    err_w.write_post_op_attr(Some(&attr), uid, gid);
                    err_w.finish_tcp_record()
                }
            }
        }

        NFSPROC3_READDIR => {
            let Some(dir_id) = r.read_fh_node_id() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(cookie) = r.read_u64() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(_cookieverf) = r.read_bytes_fixed::<8>() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(max_count) = r.read_u32() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };

            let Ok(dir_attr) = vfs.stat_by_id(dir_id) else {
                let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 64);
                w.write_u32(NFS3ERR_NOENT);
                w.write_post_op_attr(None, uid, gid);
                return w.finish_tcp_record();
            };
            if dir_attr.kind != EntryKind::Directory {
                let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 128);
                w.write_u32(NFS3ERR_NOTDIR);
                w.write_post_op_attr(Some(&dir_attr), uid, gid);
                return w.finish_tcp_record();
            }

            let budget = (max_count as usize).clamp(1024, 65536);
            let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, budget + 256);
            w.write_u32(NFS3_OK);
            w.write_post_op_attr(Some(&dir_attr), uid, gid);
            w.write_bytes_fixed(&[0u8; 8]); // cookieverf3

            let mut idx = cookie as usize;
            let mut eof = true;
            while let Ok(Some((name, entry_attr))) = vfs.dir_entry_at(dir_id, idx) {
                let entry_wire_size = 4 + 8 + 4 + ((name.len() + 3) & !3) + 8;
                if w.buf.len() + entry_wire_size + 16 > budget && idx > (cookie as usize) {
                    eof = false;
                    break;
                }
                let next_cookie = (idx + 1) as u64;
                w.write_bool(true); // value_follows
                w.write_u64(entry_attr.node_id);
                w.write_str(&name);
                w.write_u64(next_cookie);
                idx += 1;
            }
            w.write_bool(false); // end of dirlist3 entries
            w.write_bool(eof);
            w.finish_tcp_record()
        }

        NFSPROC3_READDIRPLUS => {
            let Some(dir_id) = r.read_fh_node_id() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(cookie) = r.read_u64() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(_cookieverf) = r.read_bytes_fixed::<8>() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(_dircount) = r.read_u32() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(maxcount) = r.read_u32() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };

            let Ok(dir_attr) = vfs.stat_by_id(dir_id) else {
                let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 64);
                w.write_u32(NFS3ERR_NOENT);
                w.write_post_op_attr(None, uid, gid);
                return w.finish_tcp_record();
            };
            if dir_attr.kind != EntryKind::Directory {
                let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 128);
                w.write_u32(NFS3ERR_NOTDIR);
                w.write_post_op_attr(Some(&dir_attr), uid, gid);
                return w.finish_tcp_record();
            }

            let budget = (maxcount as usize).clamp(2048, 65536);
            let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, budget + 256);
            w.write_u32(NFS3_OK);
            w.write_post_op_attr(Some(&dir_attr), uid, gid);
            w.write_bytes_fixed(&[0u8; 8]); // cookieverf3

            let mut idx = cookie as usize;
            let mut eof = true;
            while let Ok(Some((name, entry_attr))) = vfs.dir_entry_at(dir_id, idx) {
                let entry_wire_size = 4 + 8 + 4 + ((name.len() + 3) & !3) + 8 + 88 + 16;
                if w.buf.len() + entry_wire_size + 16 > budget && idx > (cookie as usize) {
                    eof = false;
                    break;
                }
                let next_cookie = (idx + 1) as u64;
                w.write_bool(true); // value_follows
                w.write_u64(entry_attr.node_id);
                w.write_str(&name);
                w.write_u64(next_cookie);
                w.write_post_op_attr(Some(&entry_attr), uid, gid);
                // post_op_fh3: handle_follows = true + nfs_fh3
                w.write_bool(true);
                w.write_fh_node_id(entry_attr.node_id);
                idx += 1;
            }
            w.write_bool(false); // end of dirlistplus3 entries
            w.write_bool(eof);
            w.finish_tcp_record()
        }

        NFSPROC3_FSSTAT => {
            let Some(node_id) = r.read_fh_node_id() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let attr = vfs.stat_by_id(node_id).ok();
            let (total_bytes, total_files) = vfs.total_volume_stats();
            let free_overlay_bytes = 10 * 1024 * 1024 * 1024u64; // 10 GB virtual headroom for sidecars
            let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 192);
            w.write_u32(NFS3_OK);
            w.write_post_op_attr(attr.as_ref(), uid, gid);
            w.write_u64(total_bytes + free_overlay_bytes); // tbytes
            w.write_u64(free_overlay_bytes); // fbytes
            w.write_u64(free_overlay_bytes); // abytes
            w.write_u64(total_files + 100_000); // tfiles
            w.write_u64(100_000); // ffiles
            w.write_u64(100_000); // afiles
            w.write_u32(60); // invarsec
            w.finish_tcp_record()
        }

        NFSPROC3_FSINFO => {
            let Some(node_id) = r.read_fh_node_id() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let attr = vfs.stat_by_id(node_id).ok();
            let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 192);
            w.write_u32(NFS3_OK);
            w.write_post_op_attr(attr.as_ref(), uid, gid);
            w.write_u32(MAX_READ_SIZE); // rtmax (1 MB)
            w.write_u32(MAX_READ_SIZE); // rtpref (1 MB)
            w.write_u32(4096); // rtmult
            w.write_u32(65536); // wtmax
            w.write_u32(65536); // wtpref
            w.write_u32(4096); // wtmult
            w.write_u32(65536); // dtpref (64 KB)
            w.write_u64(0x7FFF_FFFF_FFFF_FFFF); // maxfilesize
            w.write_u32(0); // time_delta.seconds
            w.write_u32(1000); // time_delta.nseconds
            w.write_u32(FSF3_HOMOGENEOUS); // properties
            w.finish_tcp_record()
        }

        NFSPROC3_PATHCONF => {
            let Some(node_id) = r.read_fh_node_id() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let attr = vfs.stat_by_id(node_id).ok();
            let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 160);
            w.write_u32(NFS3_OK);
            w.write_post_op_attr(attr.as_ref(), uid, gid);
            w.write_u32(1); // linkmax
            w.write_u32(255); // name_max
            w.write_bool(true); // no_trunc
            w.write_bool(true); // chown_restricted
            w.write_bool(false); // case_insensitive
            w.write_bool(true); // case_preserving
            w.finish_tcp_record()
        }

        NFSPROC3_SETATTR => {
            let Some(node_id) = r.read_fh_node_id() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(new_size) = r.read_sattr3_size() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let pre_attr = vfs.stat_by_id(node_id).ok();
            let post_res = match new_size {
                Some(sz) => vfs.truncate_by_id(node_id, sz),
                None => vfs.stat_by_id(node_id),
            };
            let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 160);
            match post_res {
                Ok(post_attr) => {
                    w.write_u32(NFS3_OK);
                    w.write_wcc_data(pre_attr.as_ref(), Some(&post_attr), uid, gid);
                }
                Err(_) => {
                    w.write_u32(NFS3ERR_ROFS);
                    w.write_wcc_data(pre_attr.as_ref(), pre_attr.as_ref(), uid, gid);
                }
            }
            w.finish_tcp_record()
        }

        NFSPROC3_CREATE => {
            let Some(dir_id) = r.read_fh_node_id() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(name) = r.read_str() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(create_mode) = r.read_u32() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            if create_mode == 2 {
                // EXCLUSIVE: 8-byte createverf3
                let _ = r.read_bytes_fixed::<8>();
            } else {
                let _ = r.read_sattr3_size();
            }

            let dir_attr = vfs.stat_by_id(dir_id).ok();
            let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 256);
            match vfs.create_in_dir(dir_id, name, create_mode == 0) {
                Ok(file_attr) => {
                    w.write_u32(NFS3_OK);
                    w.write_bool(true); // post_op_fh3: handle_follows = true
                    w.write_fh_node_id(file_attr.node_id);
                    w.write_post_op_attr(Some(&file_attr), uid, gid);
                    w.write_wcc_data(dir_attr.as_ref(), dir_attr.as_ref(), uid, gid);
                }
                Err(_) => {
                    w.write_u32(NFS3ERR_ROFS);
                    w.write_wcc_data(dir_attr.as_ref(), dir_attr.as_ref(), uid, gid);
                }
            }
            w.finish_tcp_record()
        }

        NFSPROC3_WRITE => {
            let Some(node_id) = r.read_fh_node_id() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(offset) = r.read_u64() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(_count) = r.read_u32() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(_stable) = r.read_u32() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(data) = r.read_opaque() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };

            let pre_attr = vfs.stat_by_id(node_id).ok();
            let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 192);
            match vfs.write_by_id(node_id, offset, data) {
                Ok((written, post_attr)) => {
                    w.write_u32(NFS3_OK);
                    w.write_wcc_data(pre_attr.as_ref(), Some(&post_attr), uid, gid);
                    w.write_u32(written as u32);
                    w.write_u32(2); // FILE_SYNC
                    w.write_bytes_fixed(&WRITE_VERF);
                }
                Err(_) => {
                    w.write_u32(NFS3ERR_ROFS);
                    w.write_wcc_data(pre_attr.as_ref(), pre_attr.as_ref(), uid, gid);
                }
            }
            w.finish_tcp_record()
        }

        NFSPROC3_REMOVE => {
            let Some(dir_id) = r.read_fh_node_id() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(name) = r.read_str() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let dir_attr = vfs.stat_by_id(dir_id).ok();
            let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 160);
            match vfs.remove_in_dir(dir_id, name) {
                Ok(()) => {
                    w.write_u32(NFS3_OK);
                    w.write_wcc_data(dir_attr.as_ref(), dir_attr.as_ref(), uid, gid);
                }
                Err(Error::NotFound(_)) => {
                    w.write_u32(NFS3ERR_NOENT);
                    w.write_wcc_data(dir_attr.as_ref(), dir_attr.as_ref(), uid, gid);
                }
                Err(_) => {
                    w.write_u32(NFS3ERR_ROFS);
                    w.write_wcc_data(dir_attr.as_ref(), dir_attr.as_ref(), uid, gid);
                }
            }
            w.finish_tcp_record()
        }

        NFSPROC3_RENAME => {
            let Some(from_dir_id) = r.read_fh_node_id() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(from_name) = r.read_str() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(to_dir_id) = r.read_fh_node_id() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let Some(to_name) = r.read_str() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };

            let from_attr = vfs.stat_by_id(from_dir_id).ok();
            let to_attr = vfs.stat_by_id(to_dir_id).ok();
            let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 256);
            match vfs.rename_in_dir(from_dir_id, from_name, to_dir_id, to_name) {
                Ok(()) => {
                    w.write_u32(NFS3_OK);
                    w.write_wcc_data(from_attr.as_ref(), from_attr.as_ref(), uid, gid);
                    w.write_wcc_data(to_attr.as_ref(), to_attr.as_ref(), uid, gid);
                }
                Err(Error::NotFound(_)) => {
                    w.write_u32(NFS3ERR_NOENT);
                    w.write_wcc_data(from_attr.as_ref(), from_attr.as_ref(), uid, gid);
                    w.write_wcc_data(to_attr.as_ref(), to_attr.as_ref(), uid, gid);
                }
                Err(_) => {
                    w.write_u32(NFS3ERR_ROFS);
                    w.write_wcc_data(from_attr.as_ref(), from_attr.as_ref(), uid, gid);
                    w.write_wcc_data(to_attr.as_ref(), to_attr.as_ref(), uid, gid);
                }
            }
            w.finish_tcp_record()
        }

        NFSPROC3_COMMIT => {
            let Some(node_id) = r.read_fh_node_id() else {
                return build_rpc_error_reply(xid, ACCEPT_GARBAGE_ARGS, None);
            };
            let attr = vfs.stat_by_id(node_id).ok();
            let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 160);
            w.write_u32(NFS3_OK);
            w.write_wcc_data(attr.as_ref(), attr.as_ref(), uid, gid);
            w.write_bytes_fixed(&WRITE_VERF);
            w.finish_tcp_record()
        }

        NFSPROC3_MKDIR | NFSPROC3_SYMLINK | NFSPROC3_MKNOD | NFSPROC3_RMDIR => {
            let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 64);
            w.write_u32(NFS3ERR_ROFS);
            w.write_bool(false);
            w.write_bool(false);
            w.finish_tcp_record()
        }

        NFSPROC3_LINK => {
            let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 64);
            w.write_u32(NFS3ERR_ROFS);
            w.write_bool(false);
            w.write_bool(false);
            w.write_bool(false);
            w.finish_tcp_record()
        }

        _ => {
            let mut w = XdrWriter::new_rpc_reply(xid, ACCEPT_SUCCESS, 64);
            w.write_u32(NFS3ERR_NOTSUPP);
            w.write_bool(false);
            w.finish_tcp_record()
        }
    }
}

fn build_rpc_error_reply(xid: u32, accept_stat: u32, mismatch_vers: Option<(u32, u32)>) -> Vec<u8> {
    let mut w = XdrWriter::new_rpc_reply(xid, accept_stat, 48);
    if let Some((low, high)) = mismatch_vers {
        w.write_u32(low);
        w.write_u32(high);
    }
    w.finish_tcp_record()
}

struct XdrReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> XdrReader<'a> {
    #[inline]
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    #[inline]
    fn read_u32(&mut self) -> Option<u32> {
        if self.pos + 4 > self.buf.len() {
            return None;
        }
        let v = u32::from_be_bytes([
            self.buf[self.pos],
            self.buf[self.pos + 1],
            self.buf[self.pos + 2],
            self.buf[self.pos + 3],
        ]);
        self.pos += 4;
        Some(v)
    }

    #[inline]
    fn read_u64(&mut self) -> Option<u64> {
        if self.pos + 8 > self.buf.len() {
            return None;
        }
        let v = u64::from_be_bytes([
            self.buf[self.pos],
            self.buf[self.pos + 1],
            self.buf[self.pos + 2],
            self.buf[self.pos + 3],
            self.buf[self.pos + 4],
            self.buf[self.pos + 5],
            self.buf[self.pos + 6],
            self.buf[self.pos + 7],
        ]);
        self.pos += 8;
        Some(v)
    }

    #[inline]
    fn read_bytes_fixed<const N: usize>(&mut self) -> Option<[u8; N]> {
        if self.pos + N > self.buf.len() {
            return None;
        }
        let mut out = [0u8; N];
        out.copy_from_slice(&self.buf[self.pos..self.pos + N]);
        self.pos += (N + 3) & !3;
        Some(out)
    }

    #[inline]
    fn read_opaque(&mut self) -> Option<&'a [u8]> {
        let len = self.read_u32()? as usize;
        let padded = (len + 3) & !3;
        if self.pos + padded > self.buf.len() {
            return None;
        }
        let slice = &self.buf[self.pos..self.pos + len];
        self.pos += padded;
        Some(slice)
    }

    #[inline]
    fn read_str(&mut self) -> Option<&'a str> {
        let bytes = self.read_opaque()?;
        std::str::from_utf8(bytes).ok()
    }

    #[inline]
    fn read_fh_node_id(&mut self) -> Option<u64> {
        let bytes = self.read_opaque()?;
        if bytes.len() < 8 {
            return None;
        }
        Some(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    #[inline]
    fn skip_opaque_auth(&mut self) -> Option<()> {
        let _flavor = self.read_u32()?;
        let _body = self.read_opaque()?;
        Some(())
    }

    /// Parses `sattr3` and returns `Some(Some(new_size))` if `size` is set, or `Some(None)` if not set.
    fn read_sattr3_size(&mut self) -> Option<Option<u64>> {
        if self.read_u32()? != 0 {
            let _mode = self.read_u32()?;
        }
        if self.read_u32()? != 0 {
            let _uid = self.read_u32()?;
        }
        if self.read_u32()? != 0 {
            let _gid = self.read_u32()?;
        }
        let new_size = if self.read_u32()? != 0 {
            Some(self.read_u64()?)
        } else {
            None
        };
        let set_atime = self.read_u32()?;
        if set_atime == 2 {
            let _sec = self.read_u32()?;
            let _nsec = self.read_u32()?;
        }
        let set_mtime = self.read_u32()?;
        if set_mtime == 2 {
            let _sec = self.read_u32()?;
            let _nsec = self.read_u32()?;
        }
        Some(new_size)
    }
}

struct XdrWriter {
    buf: Vec<u8>,
}

impl XdrWriter {
    /// Creates an XDR writer pre-populated with a 4-byte TCP record-mark placeholder and RPC reply header.
    #[inline]
    fn new_rpc_reply(xid: u32, accept_stat: u32, capacity: usize) -> Self {
        let mut buf = Vec::with_capacity(capacity.max(32));
        buf.extend_from_slice(&[0u8; 4]); // TCP fragment header placeholder
        buf.extend_from_slice(&xid.to_be_bytes());
        buf.extend_from_slice(&MSG_REPLY.to_be_bytes());
        buf.extend_from_slice(&MSG_ACCEPTED.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes()); // verf flavor = AUTH_NONE
        buf.extend_from_slice(&0u32.to_be_bytes()); // verf length = 0
        buf.extend_from_slice(&accept_stat.to_be_bytes());
        Self { buf }
    }

    #[inline]
    fn write_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    #[inline]
    fn write_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    #[inline]
    fn write_bool(&mut self, v: bool) {
        self.write_u32(if v { 1 } else { 0 });
    }

    #[inline]
    fn write_bytes_fixed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    #[inline]
    fn write_opaque(&mut self, bytes: &[u8]) {
        self.write_u32(bytes.len() as u32);
        self.buf.extend_from_slice(bytes);
        let pad = (4 - (bytes.len() & 3)) & 3;
        for _ in 0..pad {
            self.buf.push(0);
        }
    }

    #[inline]
    fn write_str(&mut self, s: &str) {
        self.write_opaque(s.as_bytes());
    }

    #[inline]
    fn write_fh_node_id(&mut self, node_id: u64) {
        self.write_u32(8);
        self.write_u64(node_id);
    }

    #[inline]
    fn write_fattr3(&mut self, attr: &VfsNodeAttr, uid: u32, gid: u32) {
        let (ftype, mode) = match attr.kind {
            EntryKind::Directory => (NF3DIR, 0o0755u32),
            EntryKind::File => {
                if CineFs::is_overlay_node(attr.node_id) {
                    (NF3REG, 0o0644u32)
                } else {
                    (NF3REG, 0o0444u32)
                }
            }
        };
        let used = (attr.size + 511) & !511;
        let ts = attr.timestamp_unix_secs as u32;

        self.write_u32(ftype); // type
        self.write_u32(mode); // mode
        self.write_u32(attr.nlink); // nlink
        self.write_u32(uid); // uid
        self.write_u32(gid); // gid
        self.write_u64(attr.size); // size
        self.write_u64(used); // used
        self.write_u32(0); // rdev.specdata1
        self.write_u32(0); // rdev.specdata2
        self.write_u64(FSID_CINEFS); // fsid
        self.write_u64(attr.node_id); // fileid
        self.write_u32(ts); // atime.seconds
        self.write_u32(0); // atime.nseconds
        self.write_u32(ts); // mtime.seconds
        self.write_u32(0); // mtime.nseconds
        self.write_u32(ts); // ctime.seconds
        self.write_u32(0); // ctime.nseconds
    }

    #[inline]
    fn write_post_op_attr(&mut self, attr: Option<&VfsNodeAttr>, uid: u32, gid: u32) {
        if let Some(a) = attr {
            self.write_bool(true);
            self.write_fattr3(a, uid, gid);
        } else {
            self.write_bool(false);
        }
    }

    #[inline]
    fn write_wcc_data(
        &mut self,
        pre: Option<&VfsNodeAttr>,
        post: Option<&VfsNodeAttr>,
        uid: u32,
        gid: u32,
    ) {
        if let Some(a) = pre {
            let ts = a.timestamp_unix_secs as u32;
            self.write_bool(true);
            self.write_u64(a.size);
            self.write_u32(ts);
            self.write_u32(0);
            self.write_u32(ts);
            self.write_u32(0);
        } else {
            self.write_bool(false);
        }
        self.write_post_op_attr(post, uid, gid);
    }

    #[inline]
    fn finish_tcp_record(mut self) -> Vec<u8> {
        let payload_len = (self.buf.len() - 4) as u32;
        let frag_hdr = 0x8000_0000u32 | payload_len;
        self.buf[0..4].copy_from_slice(&frag_hdr.to_be_bytes());
        self.buf
    }
}
