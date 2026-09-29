//! Descriptor-relative file primitives (procedures.ts:95-118, 334-349 and
//! storage.ts:129-199). The TypeScript used `/proc/self/fd/<fd>/<name>` paths
//! with `O_NOFOLLOW`; this uses the real `*at` system calls with the same flags.

use crate::error::{IbaraError, Result};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::jsv;

pub const O_RDONLY: i32 = libc::O_RDONLY;
pub const O_WRONLY: i32 = libc::O_WRONLY;
pub const O_RDWR: i32 = libc::O_RDWR;
pub const O_CREAT: i32 = libc::O_CREAT;
pub const O_EXCL: i32 = libc::O_EXCL;
pub const O_DIRECTORY: i32 = libc::O_DIRECTORY;
pub const O_NOFOLLOW: i32 = libc::O_NOFOLLOW;
/// Added to read-only opens so a FIFO is refused by the later type check
/// instead of blocking the process.
pub const O_NONBLOCK: i32 = libc::O_NONBLOCK;

const COPY_BUF: usize = 64 * 1024;

/// `fail(code, message, retrySafe)` (procedures.ts:88-93): `requires_reconciliation`
/// defaults to `!retrySafe`.
pub fn fail(code: &'static str, message: impl Into<String>, retry_safe: bool) -> IbaraError {
    IbaraError::new(code, message, retry_safe).with("requires_reconciliation", !retry_safe)
}

/// The free-space and budget refusals carry `execution_not_started`.
pub fn fail_not_started(code: &'static str, message: impl Into<String>) -> IbaraError {
    fail(code, message, true).with("execution_not_started", true)
}

pub fn errno(e: &io::Error) -> i32 {
    e.raw_os_error().unwrap_or(0)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn cstr(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

fn cpath(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(rc) }
}

/// `procChild` name validation (procedures.ts:95-100).
pub fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') || name.contains('\\') {
        return Err(fail("INVALID_ARGUMENT", "Invalid path component.", true));
    }
    Ok(())
}

/// `open(2)` on a path.
pub fn open_path(path: &Path, flags: i32, mode: u32) -> io::Result<OwnedFd> {
    let c = cpath(path)?;
    // SAFETY: valid NUL-terminated path; the returned descriptor is owned.
    let fd = check(unsafe { libc::open(c.as_ptr(), flags | libc::O_CLOEXEC, mode as libc::c_uint) })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `openat(2)` relative to a directory with `O_NOFOLLOW | O_CLOEXEC`, without name checks.
pub fn openat_raw(dir: BorrowedFd<'_>, name: &str, flags: i32, mode: u32) -> io::Result<OwnedFd> {
    let c = cstr(name)?;
    // SAFETY: dir is a live descriptor; the returned descriptor is owned.
    let fd = check(unsafe {
        libc::openat(dir.as_raw_fd(), c.as_ptr(), flags | O_NOFOLLOW | libc::O_CLOEXEC, mode as libc::c_uint)
    })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `openChild` (procedures.ts:102-104). The outer error is an invalid name; the
/// inner one is the system call's, so callers can branch on errno.
pub fn open_child(dir: BorrowedFd<'_>, name: &str, flags: i32, mode: u32) -> Result<io::Result<OwnedFd>> {
    check_name(name)?;
    Ok(openat_raw(dir, name, flags, mode))
}

/// A duplicate directory descriptor (`open('/proc/self/fd/<fd>')`).
pub fn reopen_dir(dir: BorrowedFd<'_>) -> io::Result<OwnedFd> {
    let c = cstr(".")?;
    // SAFETY: as above.
    let fd = check(unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), O_RDONLY | O_DIRECTORY | libc::O_CLOEXEC) })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `openDir` (procedures.ts:106-114): follows the given absolute path, then
/// requires a real directory.
pub fn open_dir(abs: &Path) -> Result<OwnedFd> {
    let fd = open_path(abs, O_RDONLY | O_DIRECTORY, 0)?;
    let st = fstat(fd_ref(&fd))?;
    if !st.is_dir() {
        return Err(fail("PERMISSION_DENIED", "Directory must be a real directory, not a symlink or special file.", true));
    }
    Ok(fd)
}

pub fn fd_ref(fd: &OwnedFd) -> BorrowedFd<'_> {
    use std::os::fd::AsFd;
    fd.as_fd()
}

/// The subset of `fs.Stats` storage reads, from `statx` like libuv.
#[derive(Debug, Clone, Copy)]
pub struct Stat {
    pub mode: u32,
    pub nlink: u64,
    pub size: u64,
    pub dev: u64,
    pub ino: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub birth_ns: i128,
}

impl Stat {
    fn kind(&self) -> u32 {
        self.mode & libc::S_IFMT
    }
    pub fn is_file(&self) -> bool {
        self.kind() == libc::S_IFREG
    }
    pub fn is_dir(&self) -> bool {
        self.kind() == libc::S_IFDIR
    }
    pub fn is_symlink(&self) -> bool {
        self.kind() == libc::S_IFLNK
    }
    /// Symlink, FIFO, socket, character or block device.
    pub fn is_special(&self) -> bool {
        !self.is_file() && !self.is_dir()
    }
    /// `${dev}:${ino}:${size}:${mtimeNs}:${ctimeNs}` (storage.ts:456, 517).
    pub fn version(&self) -> String {
        format!("{}:{}:{}:{}:{}", self.dev, self.ino, self.size, self.mtime_ns, self.ctime_ns)
    }
}

fn makedev(major: u32, minor: u32) -> u64 {
    let (major, minor) = (u64::from(major), u64::from(minor));
    ((major & 0xffff_f000) << 32) | ((major & 0x0000_0fff) << 8) | ((minor & 0xffff_ff00) << 12) | (minor & 0x0000_00ff)
}

fn ts_ns(t: libc::statx_timestamp) -> i128 {
    i128::from(t.tv_sec) * 1_000_000_000 + i128::from(t.tv_nsec)
}

fn statx_at(dir: libc::c_int, name: &CString, flags: libc::c_int) -> io::Result<Stat> {
    // SAFETY: statx writes into a zeroed, correctly sized buffer.
    let mut sx: libc::statx = unsafe { std::mem::zeroed() };
    check(unsafe { libc::statx(dir, name.as_ptr(), flags, libc::STATX_BASIC_STATS | libc::STATX_BTIME, &mut sx) })?;
    Ok(Stat {
        mode: u32::from(sx.stx_mode),
        nlink: u64::from(sx.stx_nlink),
        size: sx.stx_size,
        dev: makedev(sx.stx_dev_major, sx.stx_dev_minor),
        ino: sx.stx_ino,
        mtime_ns: ts_ns(sx.stx_mtime),
        ctime_ns: ts_ns(sx.stx_ctime),
        birth_ns: ts_ns(sx.stx_btime),
    })
}

pub fn fstat(fd: BorrowedFd<'_>) -> io::Result<Stat> {
    statx_at(fd.as_raw_fd(), &cstr("")?, libc::AT_EMPTY_PATH)
}

/// `lstat` of a child of a directory descriptor.
pub fn lstat_at(dir: BorrowedFd<'_>, name: &str) -> io::Result<Stat> {
    statx_at(dir.as_raw_fd(), &cstr(name)?, libc::AT_SYMLINK_NOFOLLOW)
}

pub fn lstat_path(path: &Path) -> io::Result<Stat> {
    statx_at(libc::AT_FDCWD, &cpath(path)?, libc::AT_SYMLINK_NOFOLLOW)
}

pub fn pread(fd: BorrowedFd<'_>, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    loop {
        // SAFETY: buf is valid for writes of buf.len() bytes.
        let n = unsafe { libc::pread(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), offset as libc::off_t) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

pub fn pwrite(fd: BorrowedFd<'_>, buf: &[u8], offset: u64) -> io::Result<usize> {
    loop {
        // SAFETY: buf is valid for reads of buf.len() bytes.
        let n = unsafe { libc::pwrite(fd.as_raw_fd(), buf.as_ptr().cast(), buf.len(), offset as libc::off_t) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Write all of `buf` at `offset`; a zero-length write is an error.
pub fn pwrite_all(fd: BorrowedFd<'_>, buf: &[u8], offset: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = pwrite(fd, &buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        done += n;
    }
    Ok(())
}

pub fn fsync(fd: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: plain system call on a live descriptor.
    check(unsafe { libc::fsync(fd.as_raw_fd()) }).map(drop)
}

pub fn ftruncate(fd: BorrowedFd<'_>, len: u64) -> io::Result<()> {
    // SAFETY: as above.
    check(unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) }).map(drop)
}

pub fn fchmod(fd: BorrowedFd<'_>, mode: u32) -> io::Result<()> {
    // SAFETY: as above.
    check(unsafe { libc::fchmod(fd.as_raw_fd(), mode as libc::mode_t) }).map(drop)
}

pub fn rename_at(dir: BorrowedFd<'_>, from: &str, to: &str) -> io::Result<()> {
    let (a, b) = (cstr(from)?, cstr(to)?);
    // SAFETY: valid names relative to a live directory descriptor.
    check(unsafe { libc::renameat(dir.as_raw_fd(), a.as_ptr(), dir.as_raw_fd(), b.as_ptr()) }).map(drop)
}

pub fn link_at(dir: BorrowedFd<'_>, from: &str, to: &str) -> io::Result<()> {
    let (a, b) = (cstr(from)?, cstr(to)?);
    // SAFETY: as above; flags 0 never follows a symlink source.
    check(unsafe { libc::linkat(dir.as_raw_fd(), a.as_ptr(), dir.as_raw_fd(), b.as_ptr(), 0) }).map(drop)
}

pub fn unlink_at(dir: BorrowedFd<'_>, name: &str) -> io::Result<()> {
    let c = cstr(name)?;
    // SAFETY: as above.
    check(unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), 0) }).map(drop)
}

pub fn mkdir_at(dir: BorrowedFd<'_>, name: &str, mode: u32) -> io::Result<()> {
    let c = cstr(name)?;
    // SAFETY: as above.
    check(unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), mode as libc::mode_t) }).map(drop)
}

/// Free bytes (`bavail * bsize`) of the filesystem holding `path`.
pub fn statfs_free(path: &Path) -> io::Result<u64> {
    let c = cpath(path)?;
    // SAFETY: statfs writes into a zeroed buffer.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    check(unsafe { libc::statfs(c.as_ptr(), &mut st) })?;
    Ok((st.f_bavail as u64).saturating_mul(st.f_bsize as u64))
}

/// `fs.accessSync(path, W_OK)`.
pub fn writable(path: &Path) -> bool {
    cpath(path).is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0)
}

/// `/proc/self/fd/<fd>`.
pub fn proc_fd(fd: BorrowedFd<'_>) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

/// `fdPath` (storage.ts:178-180): the real path of an open descriptor.
pub fn fd_path(fd: BorrowedFd<'_>) -> Result<PathBuf> {
    Ok(std::fs::canonicalize(proc_fd(fd))?)
}

/// `fs.mkdirSync(dir, { recursive: true, mode: 0o700 })`.
pub fn mkdir_p(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    Ok(())
}

/// `hashFdRange` (storage.ts:145-161): hash the first `length` bytes of a regular file.
pub fn hash_fd_range(fd: BorrowedFd<'_>, length: u64) -> Result<(String, u64)> {
    let st = fstat(fd)?;
    if !st.is_file() {
        return Err(fail("PERMISSION_DENIED", "Refusing a symlink or special file.", true));
    }
    if length > st.size {
        return Err(fail("STALE_TARGET", "Retained upload prefix changed.", true));
    }
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; COPY_BUF];
    let mut off = 0u64;
    while off < length {
        let want = (length - off).min(COPY_BUF as u64) as usize;
        let n = pread(fd, &mut buf[..want], off)?;
        if n == 0 {
            return Err(fail("STALE_TARGET", "Retained upload prefix changed.", true));
        }
        hash.update(&buf[..n]);
        off += n as u64;
    }
    Ok((hex(&hash.finalize()), off))
}

/// `hashFd` (storage.ts:140-143).
pub fn hash_fd(fd: BorrowedFd<'_>) -> Result<(String, u64)> {
    let size = fstat(fd)?.size;
    hash_fd_range(fd, size)
}

/// `readFdBytes` (storage.ts:163-176): at most `length` bytes from `offset`, bounded by the file.
pub fn read_fd_bytes(fd: BorrowedFd<'_>, offset: u64, length: u64) -> Result<Vec<u8>> {
    let st = fstat(fd)?;
    if !st.is_file() {
        return Err(fail("PERMISSION_DENIED", "Refusing a symlink or special file.", true));
    }
    let start = st.size.min(offset);
    let len = length.min(st.size - start) as usize;
    let mut buf = vec![0u8; len];
    let mut got = 0;
    while got < len {
        let n = pread(fd, &mut buf[got..], start + got as u64)?;
        if n == 0 {
            break;
        }
        got += n;
    }
    buf.truncate(got);
    Ok(buf)
}

/// Copy up to `limit` bytes from `src` (offset 0) into `dst` (offset 0), hashing
/// the bytes actually copied.
pub fn copy_hashing(src: BorrowedFd<'_>, dst: BorrowedFd<'_>, limit: u64) -> io::Result<(String, u64)> {
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; COPY_BUF];
    let mut off = 0u64;
    while off < limit {
        let want = (limit - off).min(COPY_BUF as u64) as usize;
        let n = pread(src, &mut buf[..want], off)?;
        if n == 0 {
            break;
        }
        pwrite_all(dst, &buf[..n], off)?;
        hash.update(&buf[..n]);
        off += n as u64;
    }
    Ok((hex(&hash.finalize()), off))
}

/// `writeAtomicInDir` (procedures.ts:334-349).
pub fn write_atomic_in_dir(dir: BorrowedFd<'_>, name: &str, data: &[u8], mode: u32) -> Result<()> {
    let id = crate::ids::id("w");
    let tmp = format!(".tmp.{}", &id[..24.min(id.len())]);
    {
        let fd = open_child(dir, &tmp, O_WRONLY | O_CREAT | O_EXCL, mode)??;
        pwrite_all(fd_ref(&fd), data, 0)?;
        fsync(fd_ref(&fd))?;
    }
    check_name(name)?;
    rename_at(dir, &tmp, name)?;
    fsync(dir)?;
    Ok(())
}

/// `parseRel` (storage.ts:182-194).
pub fn parse_rel(rel: Option<&str>, fallback: &str) -> Result<Vec<String>> {
    let raw = match rel {
        None | Some("") => fallback,
        Some(r) => r,
    };
    let bad = || fail("INVALID_ARGUMENT", "Workspace traversal is not a supported file path.", true);
    if raw != "." && !jsv::rel_re(raw) {
        return Err(bad());
    }
    if raw.contains('\0') || raw.contains('\\') || raw.starts_with('/') {
        return Err(bad());
    }
    if raw == "." || raw.is_empty() {
        return Ok(Vec::new());
    }
    let parts: Vec<String> = raw.split('/').filter(|p| !p.is_empty() && *p != ".").map(str::to_string).collect();
    if parts.iter().any(|p| p == ".." || p.contains('\0')) {
        return Err(bad());
    }
    Ok(parts)
}

/// `assertId` (storage.ts:196-199).
pub fn assert_id(value: &str, label: &str) -> Result<()> {
    if value.is_empty() || !jsv::id_re(value) || jsv::utf16_len(value) > 128 {
        return Err(fail("INVALID_ARGUMENT", format!("{label} is not a valid identifier."), true));
    }
    Ok(())
}

const MIME_BY_EXT: &[(&str, &str)] = &[
    (".txt", "text/plain"),
    (".md", "text/markdown"),
    (".json", "application/json"),
    (".csv", "text/csv"),
    (".html", "text/html"),
    (".css", "text/css"),
    (".js", "text/javascript"),
    (".ts", "text/plain"),
    (".png", "image/png"),
    (".jpg", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".gif", "image/gif"),
    (".webp", "image/webp"),
    (".pdf", "application/pdf"),
    (".zip", "application/zip"),
    (".gz", "application/gzip"),
];

/// Node `path.extname`.
fn extname(name: &str) -> &str {
    let base = jsv::basename(name);
    match base.rfind('.') {
        Some(0) | None => "",
        Some(i) => &base[i..],
    }
}

/// `mimeOf` (storage.ts:129-138).
pub fn mime_of(name: &str, head: Option<&[u8]>) -> &'static str {
    if let Some(h) = head
        && h.len() >= 4
    {
        if h[..4] == [0x89, 0x50, 0x4e, 0x47] {
            return "image/png";
        }
        if h[..2] == [0xff, 0xd8] {
            return "image/jpeg";
        }
        if h[..4] == [0x25, 0x50, 0x44, 0x46] {
            return "application/pdf";
        }
        if h[..2] == [0x1f, 0x8b] {
            return "application/gzip";
        }
        if h[..2] == [0x50, 0x4b] {
            return "application/zip";
        }
    }
    let ext = extname(name).to_lowercase();
    MIME_BY_EXT.iter().find(|(e, _)| *e == ext).map(|(_, m)| *m).unwrap_or("application/octet-stream")
}

/// Bind a JSON scalar the way better-sqlite3 binds a JavaScript value.
pub fn bind_json(v: Option<&Value>) -> rusqlite::types::Value {
    use rusqlite::types::Value as Sql;
    match v {
        None | Some(Value::Null) => Sql::Null,
        Some(Value::Bool(b)) => Sql::Integer(i64::from(*b)),
        Some(Value::Number(n)) => match n.as_i64() {
            Some(i) => Sql::Integer(i),
            None => Sql::Real(n.as_f64().unwrap_or(f64::NAN)),
        },
        Some(Value::String(s)) => Sql::Text(s.clone()),
        Some(other) => Sql::Text(other.to_string()),
    }
}

/// A SQLite column value as JSON (integers stay integers).
pub fn sql_to_json(v: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef as R;
    match v {
        R::Null => Value::Null,
        R::Integer(i) => json!(i),
        R::Real(f) => json!(f),
        R::Text(t) => json!(String::from_utf8_lossy(t)),
        R::Blob(b) => json!(String::from_utf8_lossy(b)),
    }
}

/// Read the directory entries of an open directory (`fs.readdirSync('/proc/self/fd/<fd>')`),
/// in system order.
pub fn read_dir_fd(fd: BorrowedFd<'_>) -> Result<std::fs::ReadDir> {
    Ok(std::fs::read_dir(proc_fd(fd))?)
}
