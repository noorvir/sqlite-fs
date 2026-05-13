use libc::{c_char, c_int, c_uint, c_void, dev_t, gid_t, mode_t, off_t, size_t, uid_t};
use std::ffi::{CStr, CString};
use std::io;
use std::path::Path;
use std::ptr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub type FileHandle = u64;
pub type FsResult<T> = Result<T, FsError>;

pub const NO_HANDLE: FileHandle = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone)]
pub struct Attr {
    pub kind: EntryKind,
    pub len: u64,
    pub perm: u16,
    pub uid: u32,
    pub gid: u32,
    pub accessed: SystemTime,
    pub modified: SystemTime,
    pub changed: SystemTime,
    pub created: SystemTime,
}

impl Attr {
    pub fn file(len: u64) -> Self {
        let now = SystemTime::now();
        Self {
            kind: EntryKind::File,
            len,
            perm: 0o644,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            accessed: now,
            modified: now,
            changed: now,
            created: now,
        }
    }

    pub fn directory() -> Self {
        let now = SystemTime::now();
        Self {
            kind: EntryKind::Directory,
            len: 0,
            perm: 0o755,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            accessed: now,
            modified: now,
            changed: now,
            created: now,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
}

impl DirEntry {
    pub fn file(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: EntryKind::File,
        }
    }

    pub fn directory(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: EntryKind::Directory,
        }
    }
}

#[derive(Debug, Clone)]
pub enum FsError {
    NotFound,
    Exists,
    NotDirectory,
    IsDirectory,
    DirectoryNotEmpty,
    InvalidInput,
    FileTooLarge,
    PermissionDenied,
    ReadOnly,
    BadFileDescriptor,
    NoSpace,
    NameTooLong,
    Range,
    Unsupported,
    Io,
}

impl FsError {
    fn errno(&self) -> i32 {
        match self {
            FsError::NotFound => libc::ENOENT,
            FsError::Exists => libc::EEXIST,
            FsError::NotDirectory => libc::ENOTDIR,
            FsError::IsDirectory => libc::EISDIR,
            FsError::DirectoryNotEmpty => libc::ENOTEMPTY,
            FsError::InvalidInput => libc::EINVAL,
            FsError::FileTooLarge => libc::EFBIG,
            FsError::PermissionDenied => libc::EACCES,
            FsError::ReadOnly => libc::EROFS,
            FsError::BadFileDescriptor => libc::EBADF,
            FsError::NoSpace => libc::ENOSPC,
            FsError::NameTooLong => libc::ENAMETOOLONG,
            FsError::Range => libc::ERANGE,
            FsError::Unsupported => libc::ENOSYS,
            FsError::Io => libc::EIO,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    ReadOnly,
    WriteOnly,
    ReadWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenOptions {
    raw: i32,
}

impl OpenOptions {
    pub fn from_raw(raw: i32) -> Self {
        Self { raw }
    }

    pub fn raw(self) -> i32 {
        self.raw
    }

    pub fn access_mode(self) -> AccessMode {
        match self.raw & libc::O_ACCMODE {
            libc::O_WRONLY => AccessMode::WriteOnly,
            libc::O_RDWR => AccessMode::ReadWrite,
            _ => AccessMode::ReadOnly,
        }
    }

    pub fn readable(self) -> bool {
        self.access_mode() != AccessMode::WriteOnly
    }

    pub fn writable(self) -> bool {
        self.access_mode() != AccessMode::ReadOnly
    }

    pub fn append(self) -> bool {
        self.raw & libc::O_APPEND != 0
    }

    pub fn truncate(self) -> bool {
        self.raw & libc::O_TRUNC != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenameFlags {
    raw: u32,
}

impl RenameFlags {
    pub fn from_raw(raw: u32) -> Self {
        Self { raw }
    }

    pub fn raw(self) -> u32 {
        self.raw
    }

    pub fn is_empty(self) -> bool {
        self.raw == 0
    }
}

#[derive(Debug, Clone, Copy)]
pub enum SetTime {
    Specific(SystemTime),
    Now,
    Omit,
}

impl SetTime {
    pub fn resolve(self, current: SystemTime) -> SystemTime {
        match self {
            SetTime::Specific(time) => time,
            SetTime::Now => SystemTime::now(),
            SetTime::Omit => current,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SetTimes {
    pub accessed: SetTime,
    pub modified: SetTime,
}

#[derive(Debug, Clone, Copy)]
pub struct StatFs {
    pub block_size: u64,
    pub blocks: u64,
    pub blocks_free: u64,
    pub blocks_available: u64,
    pub files: u64,
    pub files_free: u64,
    pub name_max: u64,
}

impl Default for StatFs {
    fn default() -> Self {
        Self {
            block_size: 512,
            blocks: 1024 * 1024,
            blocks_free: 1024 * 1024,
            blocks_available: 1024 * 1024,
            files: 1024 * 1024,
            files_free: 1024 * 1024,
            name_max: 255,
        }
    }
}

/// Safe filesystem interface exposed by the macFUSE/FSKit adapter.
///
/// Implementors see typed Rust values only. The libfuse C ABI, raw pointers,
/// `struct stat` filling, and negative errno returns stay inside this crate.
///
/// The open-handle methods (`open`/`create`, `write`, `flush`, `fsync`,
/// `release`) are intentionally part of the interface so a SQLite-backed
/// filesystem can stage chunked writes per handle and commit at the semantic
/// boundary described in `SPEC.md`.
pub trait FileSystem: Send + Sync + 'static {
    fn getattr(&self, path: &str) -> FsResult<Attr>;
    fn readdir(&self, path: &str) -> FsResult<Vec<DirEntry>>;

    fn readlink(&self, _path: &str) -> FsResult<String> {
        Err(FsError::Unsupported)
    }

    fn opendir(&self, path: &str, _options: OpenOptions) -> FsResult<FileHandle> {
        match self.getattr(path)?.kind {
            EntryKind::Directory => Ok(NO_HANDLE),
            EntryKind::File => Err(FsError::NotDirectory),
        }
    }

    fn releasedir(&self, _path: &str, _handle: FileHandle) -> FsResult<()> {
        Ok(())
    }

    fn fsyncdir(&self, _path: &str, _handle: FileHandle, _datasync: bool) -> FsResult<()> {
        Ok(())
    }

    fn open(&self, path: &str, _options: OpenOptions) -> FsResult<FileHandle> {
        match self.getattr(path)?.kind {
            EntryKind::File => Ok(NO_HANDLE),
            EntryKind::Directory => Err(FsError::IsDirectory),
        }
    }

    fn create(&self, path: &str, mode: u32, options: OpenOptions) -> FsResult<FileHandle> {
        self.mknod(path, mode, 0)?;
        self.open(path, options)
    }

    fn read(&self, path: &str, handle: FileHandle, offset: u64, size: usize) -> FsResult<Vec<u8>>;

    fn write(&self, path: &str, handle: FileHandle, offset: u64, data: &[u8]) -> FsResult<usize>;

    fn flush(&self, _path: &str, _handle: FileHandle) -> FsResult<()> {
        Ok(())
    }

    fn fsync(&self, path: &str, handle: FileHandle, _datasync: bool) -> FsResult<()> {
        self.flush(path, handle)
    }

    fn release(&self, _path: &str, _handle: FileHandle) -> FsResult<()> {
        Ok(())
    }

    fn truncate(&self, path: &str, _handle: Option<FileHandle>, size: u64) -> FsResult<()>;

    fn mknod(&self, _path: &str, _mode: u32, _rdev: u64) -> FsResult<()> {
        Err(FsError::Unsupported)
    }

    fn mkdir(&self, _path: &str, _mode: u32) -> FsResult<()> {
        Err(FsError::Unsupported)
    }

    fn unlink(&self, _path: &str) -> FsResult<()> {
        Err(FsError::Unsupported)
    }

    fn rmdir(&self, _path: &str) -> FsResult<()> {
        Err(FsError::Unsupported)
    }

    fn symlink(&self, _target: &str, _linkpath: &str) -> FsResult<()> {
        Err(FsError::Unsupported)
    }

    fn rename(&self, _from: &str, _to: &str, _flags: RenameFlags) -> FsResult<()> {
        Err(FsError::Unsupported)
    }

    fn link(&self, _from: &str, _to: &str) -> FsResult<()> {
        Err(FsError::Unsupported)
    }

    fn access(&self, path: &str, _mask: i32) -> FsResult<()> {
        self.getattr(path).map(|_| ())
    }

    fn chmod(&self, _path: &str, _handle: Option<FileHandle>, _mode: u32) -> FsResult<()> {
        Err(FsError::Unsupported)
    }

    fn chown(
        &self,
        _path: &str,
        _handle: Option<FileHandle>,
        _uid: u32,
        _gid: u32,
    ) -> FsResult<()> {
        Err(FsError::Unsupported)
    }

    fn utimens(&self, _path: &str, _handle: Option<FileHandle>, _times: SetTimes) -> FsResult<()> {
        Err(FsError::Unsupported)
    }

    fn getxattr(&self, _path: &str, _name: &str) -> FsResult<Vec<u8>> {
        Err(FsError::Unsupported)
    }

    fn listxattr(&self, _path: &str) -> FsResult<Vec<String>> {
        Err(FsError::Unsupported)
    }

    fn setxattr(&self, _path: &str, _name: &str, _value: &[u8], _flags: i32) -> FsResult<()> {
        Err(FsError::Unsupported)
    }

    fn removexattr(&self, _path: &str, _name: &str) -> FsResult<()> {
        Err(FsError::Unsupported)
    }

    fn statfs(&self, _path: &str) -> FsResult<StatFs> {
        Ok(StatFs::default())
    }
}

#[derive(Debug, Clone)]
pub struct MountOptions {
    pub foreground: bool,
    pub single_threaded: bool,
    pub debug: bool,
    pub local: bool,
    pub volname: String,
}

impl Default for MountOptions {
    fn default() -> Self {
        Self {
            foreground: true,
            single_threaded: true,
            debug: false,
            local: true,
            volname: "minfs".to_string(),
        }
    }
}

struct MountState {
    fs: Box<dyn FileSystem>,
}

type FuseFillDir = unsafe extern "C" fn(
    buf: *mut c_void,
    name: *const c_char,
    stbuf: *const libc::stat,
    off: off_t,
    flags: u32,
) -> c_int;

#[repr(C)]
struct Callbacks {
    getattr: extern "C" fn(*mut c_void, *const c_char, *mut libc::stat) -> c_int,
    readlink: extern "C" fn(*mut c_void, *const c_char, *mut c_char, size_t) -> c_int,
    mknod: extern "C" fn(*mut c_void, *const c_char, mode_t, dev_t) -> c_int,
    mkdir: extern "C" fn(*mut c_void, *const c_char, mode_t) -> c_int,
    unlink: extern "C" fn(*mut c_void, *const c_char) -> c_int,
    rmdir: extern "C" fn(*mut c_void, *const c_char) -> c_int,
    symlink: extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int,
    rename: extern "C" fn(*mut c_void, *const c_char, *const c_char, c_uint) -> c_int,
    link: extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int,
    chmod: extern "C" fn(*mut c_void, *const c_char, mode_t, c_int, FileHandle) -> c_int,
    chown: extern "C" fn(*mut c_void, *const c_char, uid_t, gid_t, c_int, FileHandle) -> c_int,
    truncate: extern "C" fn(*mut c_void, *const c_char, off_t, c_int, FileHandle) -> c_int,
    open: extern "C" fn(*mut c_void, *const c_char, c_int, *mut FileHandle) -> c_int,
    create: extern "C" fn(*mut c_void, *const c_char, mode_t, c_int, *mut FileHandle) -> c_int,
    read:
        extern "C" fn(*mut c_void, *const c_char, FileHandle, *mut c_char, size_t, off_t) -> c_int,
    write: extern "C" fn(
        *mut c_void,
        *const c_char,
        FileHandle,
        *const c_char,
        size_t,
        off_t,
    ) -> c_int,
    statfs: extern "C" fn(*mut c_void, *const c_char, *mut libc::statvfs) -> c_int,
    flush: extern "C" fn(*mut c_void, *const c_char, FileHandle) -> c_int,
    release: extern "C" fn(*mut c_void, *const c_char, FileHandle) -> c_int,
    fsync: extern "C" fn(*mut c_void, *const c_char, FileHandle, c_int) -> c_int,
    setxattr: extern "C" fn(
        *mut c_void,
        *const c_char,
        *const c_char,
        *const c_char,
        size_t,
        c_int,
    ) -> c_int,
    getxattr:
        extern "C" fn(*mut c_void, *const c_char, *const c_char, *mut c_char, size_t) -> c_int,
    listxattr: extern "C" fn(*mut c_void, *const c_char, *mut c_char, size_t) -> c_int,
    removexattr: extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int,
    opendir: extern "C" fn(*mut c_void, *const c_char, c_int, *mut FileHandle) -> c_int,
    readdir: extern "C" fn(*mut c_void, *const c_char, *mut c_void, FuseFillDir, off_t) -> c_int,
    releasedir: extern "C" fn(*mut c_void, *const c_char, FileHandle) -> c_int,
    fsyncdir: extern "C" fn(*mut c_void, *const c_char, FileHandle, c_int) -> c_int,
    access: extern "C" fn(*mut c_void, *const c_char, c_int) -> c_int,
    utimens: extern "C" fn(
        *mut c_void,
        *const c_char,
        *const libc::timespec,
        c_int,
        FileHandle,
    ) -> c_int,
}

unsafe extern "C" {
    fn minfuse_mount(
        argc: c_int,
        argv: *mut *mut c_char,
        callbacks: *mut Callbacks,
        user_data: *mut c_void,
    ) -> c_int;
}

pub fn mount(
    fs: impl FileSystem,
    mountpoint: impl AsRef<Path>,
    options: MountOptions,
) -> io::Result<()> {
    mount_boxed(Box::new(fs), mountpoint, options)
}

pub fn mount_boxed(
    fs: Box<dyn FileSystem>,
    mountpoint: impl AsRef<Path>,
    options: MountOptions,
) -> io::Result<()> {
    let mut state = Box::new(MountState { fs });
    let state_ptr = (&mut *state) as *mut MountState as *mut c_void;

    let mut args = vec!["minfuse".to_string()];
    if options.foreground {
        args.push("-f".to_string());
    }
    if options.single_threaded {
        args.push("-s".to_string());
    }
    if options.debug {
        args.push("-d".to_string());
    }
    args.extend(["-o".to_string(), "backend=fskit".to_string()]);
    if options.local {
        args.extend(["-o".to_string(), "local".to_string()]);
    }
    args.extend(["-o".to_string(), "noappledouble".to_string()]);
    args.extend(["-o".to_string(), "noapplexattr".to_string()]);
    args.extend(["-o".to_string(), format!("volname={}", options.volname)]);
    args.push(mountpoint.as_ref().to_string_lossy().to_string());

    let cstrings: Vec<CString> = args
        .into_iter()
        .map(|arg| CString::new(arg).map_err(|_| io::ErrorKind::InvalidInput))
        .collect::<Result<_, _>>()?;
    let mut argv: Vec<*mut c_char> = cstrings.iter().map(|s| s.as_ptr() as *mut c_char).collect();
    let mut callbacks = Callbacks {
        getattr: cb_getattr,
        readlink: cb_readlink,
        mknod: cb_mknod,
        mkdir: cb_mkdir,
        unlink: cb_unlink,
        rmdir: cb_rmdir,
        symlink: cb_symlink,
        rename: cb_rename,
        link: cb_link,
        chmod: cb_chmod,
        chown: cb_chown,
        truncate: cb_truncate,
        open: cb_open,
        create: cb_create,
        read: cb_read,
        write: cb_write,
        statfs: cb_statfs,
        flush: cb_flush,
        release: cb_release,
        fsync: cb_fsync,
        setxattr: cb_setxattr,
        getxattr: cb_getxattr,
        listxattr: cb_listxattr,
        removexattr: cb_removexattr,
        opendir: cb_opendir,
        readdir: cb_readdir,
        releasedir: cb_releasedir,
        fsyncdir: cb_fsyncdir,
        access: cb_access,
        utimens: cb_utimens,
    };

    let rc = unsafe {
        minfuse_mount(
            argv.len() as c_int,
            argv.as_mut_ptr(),
            &mut callbacks,
            state_ptr,
        )
    };
    drop(state);

    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "mount failed with exit code {rc}"
        )))
    }
}

fn state<'a>(user_data: *mut c_void) -> &'a MountState {
    unsafe { &*(user_data as *const MountState) }
}

fn c_string(ptr: *const c_char) -> FsResult<String> {
    if ptr.is_null() {
        return Err(FsError::InvalidInput);
    }
    Ok(unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned())
}

fn path(path: *const c_char) -> FsResult<String> {
    c_string(path)
}

fn xattr_name(name: *const c_char) -> FsResult<String> {
    c_string(name)
}

fn optional_handle(has_handle: c_int, handle: FileHandle) -> Option<FileHandle> {
    (has_handle != 0).then_some(handle)
}

fn to_rc(result: FsResult<c_int>) -> c_int {
    match result {
        Ok(value) => value,
        Err(err) => -err.errno(),
    }
}

fn to_count(count: usize) -> FsResult<c_int> {
    c_int::try_from(count).map_err(|_| FsError::Io)
}

fn copy_sized_result(bytes: &[u8], out: *mut c_char, size: size_t) -> FsResult<c_int> {
    let len = to_count(bytes.len())?;
    if size == 0 {
        return Ok(len);
    }
    if out.is_null() {
        return Err(FsError::InvalidInput);
    }
    if size < bytes.len() {
        return Err(FsError::Range);
    }
    unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), out as *mut u8, bytes.len()) };
    Ok(len)
}

fn catch(callback: impl FnOnce() -> c_int) -> c_int {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback)) {
        Ok(rc) => rc,
        Err(_) => -libc::EIO,
    }
}

extern "C" fn cb_getattr(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    st: *mut libc::stat,
) -> c_int {
    catch(|| {
        to_rc((|| {
            let attr = state(user_data).fs.getattr(&path(path_ptr)?)?;
            fill_stat(st, &attr)?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_readlink(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    out: *mut c_char,
    size: size_t,
) -> c_int {
    catch(|| {
        to_rc((|| {
            let target = state(user_data).fs.readlink(&path(path_ptr)?)?;
            if size == 0 || out.is_null() {
                return Err(FsError::InvalidInput);
            }
            let target = CString::new(target).map_err(|_| FsError::InvalidInput)?;
            let bytes = target.as_bytes();
            let len = bytes.len().min(size - 1);
            unsafe {
                ptr::copy_nonoverlapping(bytes.as_ptr(), out as *mut u8, len);
                *out.add(len) = 0;
            }
            Ok(0)
        })())
    })
}

extern "C" fn cb_readdir(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    buf: *mut c_void,
    filler: FuseFillDir,
    _off: off_t,
) -> c_int {
    catch(|| {
        to_rc((|| {
            let entries = state(user_data).fs.readdir(&path(path_ptr)?)?;
            fill_dir(buf, filler, ".")?;
            fill_dir(buf, filler, "..")?;
            for entry in entries {
                fill_dir(buf, filler, &entry.name)?;
            }
            Ok(0)
        })())
    })
}

extern "C" fn cb_mknod(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    mode: mode_t,
    rdev: dev_t,
) -> c_int {
    catch(|| {
        to_rc((|| {
            state(user_data)
                .fs
                .mknod(&path(path_ptr)?, mode.into(), rdev as u64)?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_mkdir(user_data: *mut c_void, path_ptr: *const c_char, mode: mode_t) -> c_int {
    catch(|| {
        to_rc((|| {
            state(user_data).fs.mkdir(&path(path_ptr)?, mode.into())?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_unlink(user_data: *mut c_void, path_ptr: *const c_char) -> c_int {
    catch(|| {
        to_rc((|| {
            state(user_data).fs.unlink(&path(path_ptr)?)?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_rmdir(user_data: *mut c_void, path_ptr: *const c_char) -> c_int {
    catch(|| {
        to_rc((|| {
            state(user_data).fs.rmdir(&path(path_ptr)?)?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_symlink(
    user_data: *mut c_void,
    target_ptr: *const c_char,
    linkpath_ptr: *const c_char,
) -> c_int {
    catch(|| {
        to_rc((|| {
            state(user_data)
                .fs
                .symlink(&path(target_ptr)?, &path(linkpath_ptr)?)?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_rename(
    user_data: *mut c_void,
    from_ptr: *const c_char,
    to_ptr: *const c_char,
    flags: c_uint,
) -> c_int {
    catch(|| {
        to_rc((|| {
            state(user_data).fs.rename(
                &path(from_ptr)?,
                &path(to_ptr)?,
                RenameFlags::from_raw(flags),
            )?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_link(
    user_data: *mut c_void,
    from_ptr: *const c_char,
    to_ptr: *const c_char,
) -> c_int {
    catch(|| {
        to_rc((|| {
            state(user_data).fs.link(&path(from_ptr)?, &path(to_ptr)?)?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_chmod(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    mode: mode_t,
    has_handle: c_int,
    handle: FileHandle,
) -> c_int {
    catch(|| {
        to_rc((|| {
            state(user_data).fs.chmod(
                &path(path_ptr)?,
                optional_handle(has_handle, handle),
                mode.into(),
            )?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_chown(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    uid: uid_t,
    gid: gid_t,
    has_handle: c_int,
    handle: FileHandle,
) -> c_int {
    catch(|| {
        to_rc((|| {
            state(user_data).fs.chown(
                &path(path_ptr)?,
                optional_handle(has_handle, handle),
                uid,
                gid,
            )?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_truncate(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    size: off_t,
    has_handle: c_int,
    handle: FileHandle,
) -> c_int {
    catch(|| {
        to_rc((|| {
            let size = u64::try_from(size).map_err(|_| FsError::InvalidInput)?;
            state(user_data).fs.truncate(
                &path(path_ptr)?,
                optional_handle(has_handle, handle),
                size,
            )?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_open(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    flags: c_int,
    out_handle: *mut FileHandle,
) -> c_int {
    catch(|| {
        to_rc((|| {
            if out_handle.is_null() {
                return Err(FsError::InvalidInput);
            }
            let handle = state(user_data)
                .fs
                .open(&path(path_ptr)?, OpenOptions::from_raw(flags))?;
            unsafe { *out_handle = handle };
            Ok(0)
        })())
    })
}

extern "C" fn cb_create(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    mode: mode_t,
    flags: c_int,
    out_handle: *mut FileHandle,
) -> c_int {
    catch(|| {
        to_rc((|| {
            if out_handle.is_null() {
                return Err(FsError::InvalidInput);
            }
            let handle = state(user_data).fs.create(
                &path(path_ptr)?,
                mode.into(),
                OpenOptions::from_raw(flags),
            )?;
            unsafe { *out_handle = handle };
            Ok(0)
        })())
    })
}

extern "C" fn cb_read(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    handle: FileHandle,
    out: *mut c_char,
    size: size_t,
    off: off_t,
) -> c_int {
    catch(|| {
        to_rc((|| {
            let offset = u64::try_from(off).map_err(|_| FsError::InvalidInput)?;
            let data = state(user_data)
                .fs
                .read(&path(path_ptr)?, handle, offset, size)?;
            let len = data.len().min(size);
            if len != 0 {
                if out.is_null() {
                    return Err(FsError::InvalidInput);
                }
                unsafe { ptr::copy_nonoverlapping(data.as_ptr(), out as *mut u8, len) };
            }
            to_count(len)
        })())
    })
}

extern "C" fn cb_write(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    handle: FileHandle,
    input: *const c_char,
    size: size_t,
    off: off_t,
) -> c_int {
    catch(|| {
        to_rc((|| {
            let offset = u64::try_from(off).map_err(|_| FsError::InvalidInput)?;
            let input = if size == 0 {
                &[][..]
            } else if input.is_null() {
                return Err(FsError::InvalidInput);
            } else {
                unsafe { std::slice::from_raw_parts(input as *const u8, size) }
            };
            let written = state(user_data)
                .fs
                .write(&path(path_ptr)?, handle, offset, input)?;
            to_count(written)
        })())
    })
}

extern "C" fn cb_statfs(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    st: *mut libc::statvfs,
) -> c_int {
    catch(|| {
        to_rc((|| {
            let stat = state(user_data).fs.statfs(&path(path_ptr)?)?;
            fill_statvfs(st, &stat)?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_flush(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    handle: FileHandle,
) -> c_int {
    catch(|| {
        to_rc((|| {
            state(user_data).fs.flush(&path(path_ptr)?, handle)?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_release(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    handle: FileHandle,
) -> c_int {
    catch(|| {
        to_rc((|| {
            state(user_data).fs.release(&path(path_ptr)?, handle)?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_fsync(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    handle: FileHandle,
    datasync: c_int,
) -> c_int {
    catch(|| {
        to_rc((|| {
            state(user_data)
                .fs
                .fsync(&path(path_ptr)?, handle, datasync != 0)?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_setxattr(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    name_ptr: *const c_char,
    value: *const c_char,
    size: size_t,
    flags: c_int,
) -> c_int {
    catch(|| {
        to_rc((|| {
            let value = if size == 0 {
                &[][..]
            } else if value.is_null() {
                return Err(FsError::InvalidInput);
            } else {
                unsafe { std::slice::from_raw_parts(value as *const u8, size) }
            };
            state(user_data)
                .fs
                .setxattr(&path(path_ptr)?, &xattr_name(name_ptr)?, value, flags)?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_getxattr(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    name_ptr: *const c_char,
    out: *mut c_char,
    size: size_t,
) -> c_int {
    catch(|| {
        to_rc((|| {
            let value = state(user_data)
                .fs
                .getxattr(&path(path_ptr)?, &xattr_name(name_ptr)?)?;
            copy_sized_result(&value, out, size)
        })())
    })
}

extern "C" fn cb_listxattr(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    out: *mut c_char,
    size: size_t,
) -> c_int {
    catch(|| {
        to_rc((|| {
            let names = state(user_data).fs.listxattr(&path(path_ptr)?)?;
            let mut bytes = Vec::new();
            for name in names {
                let name = CString::new(name).map_err(|_| FsError::InvalidInput)?;
                bytes.extend_from_slice(name.as_bytes_with_nul());
            }
            copy_sized_result(&bytes, out, size)
        })())
    })
}

extern "C" fn cb_removexattr(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    name_ptr: *const c_char,
) -> c_int {
    catch(|| {
        to_rc((|| {
            state(user_data)
                .fs
                .removexattr(&path(path_ptr)?, &xattr_name(name_ptr)?)?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_opendir(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    flags: c_int,
    out_handle: *mut FileHandle,
) -> c_int {
    catch(|| {
        to_rc((|| {
            if out_handle.is_null() {
                return Err(FsError::InvalidInput);
            }
            let handle = state(user_data)
                .fs
                .opendir(&path(path_ptr)?, OpenOptions::from_raw(flags))?;
            unsafe { *out_handle = handle };
            Ok(0)
        })())
    })
}

extern "C" fn cb_releasedir(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    handle: FileHandle,
) -> c_int {
    catch(|| {
        to_rc((|| {
            state(user_data).fs.releasedir(&path(path_ptr)?, handle)?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_fsyncdir(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    handle: FileHandle,
    datasync: c_int,
) -> c_int {
    catch(|| {
        to_rc((|| {
            state(user_data)
                .fs
                .fsyncdir(&path(path_ptr)?, handle, datasync != 0)?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_access(user_data: *mut c_void, path_ptr: *const c_char, mask: c_int) -> c_int {
    catch(|| {
        to_rc((|| {
            state(user_data).fs.access(&path(path_ptr)?, mask)?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_utimens(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    times: *const libc::timespec,
    has_handle: c_int,
    handle: FileHandle,
) -> c_int {
    catch(|| {
        to_rc((|| {
            if times.is_null() {
                return Err(FsError::InvalidInput);
            }
            let times = unsafe { std::slice::from_raw_parts(times, 2) };
            let times = SetTimes {
                accessed: from_timespec(times[0]),
                modified: from_timespec(times[1]),
            };
            state(user_data).fs.utimens(
                &path(path_ptr)?,
                optional_handle(has_handle, handle),
                times,
            )?;
            Ok(0)
        })())
    })
}

fn fill_dir(buf: *mut c_void, filler: FuseFillDir, name: &str) -> FsResult<()> {
    let name = CString::new(name).map_err(|_| FsError::InvalidInput)?;
    let full = unsafe { filler(buf, name.as_ptr(), ptr::null(), 0, 0) } != 0;
    if full { Err(FsError::Io) } else { Ok(()) }
}

fn fill_stat(st: *mut libc::stat, attr: &Attr) -> FsResult<()> {
    if st.is_null() {
        return Err(FsError::InvalidInput);
    }
    unsafe { ptr::write_bytes(st, 0, 1) };
    unsafe {
        (*st).st_uid = attr.uid;
        (*st).st_gid = attr.gid;
        (*st).st_atime = seconds(attr.accessed);
        (*st).st_mtime = seconds(attr.modified);
        (*st).st_ctime = seconds(attr.changed);
        #[cfg(target_os = "macos")]
        {
            (*st).st_birthtime = seconds(attr.created);
        }
        (*st).st_blksize = 512;
        (*st).st_mode = match attr.kind {
            EntryKind::Directory => libc::S_IFDIR | attr.perm as libc::mode_t,
            EntryKind::File => libc::S_IFREG | attr.perm as libc::mode_t,
        } as libc::mode_t;
        (*st).st_nlink = if attr.kind == EntryKind::Directory {
            2
        } else {
            1
        };
        (*st).st_size = attr.len as i64;
        (*st).st_blocks = attr.len.div_ceil(512) as i64;
    }
    Ok(())
}

fn fill_statvfs(st: *mut libc::statvfs, stat: &StatFs) -> FsResult<()> {
    if st.is_null() {
        return Err(FsError::InvalidInput);
    }
    unsafe { ptr::write_bytes(st, 0, 1) };
    unsafe {
        (*st).f_bsize = stat.block_size as _;
        (*st).f_frsize = stat.block_size as _;
        (*st).f_blocks = stat.blocks as _;
        (*st).f_bfree = stat.blocks_free as _;
        (*st).f_bavail = stat.blocks_available as _;
        (*st).f_files = stat.files as _;
        (*st).f_ffree = stat.files_free as _;
        (*st).f_favail = stat.files_free as _;
        (*st).f_namemax = stat.name_max as _;
    }
    Ok(())
}

fn seconds(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn from_timespec(ts: libc::timespec) -> SetTime {
    match ts.tv_nsec {
        libc::UTIME_NOW => SetTime::Now,
        libc::UTIME_OMIT => SetTime::Omit,
        nsec if ts.tv_sec >= 0 && (0..1_000_000_000).contains(&nsec) => {
            SetTime::Specific(UNIX_EPOCH + Duration::new(ts.tv_sec as u64, nsec as u32))
        }
        _ => SetTime::Specific(UNIX_EPOCH),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_mapping_is_negative_at_boundary() {
        assert_eq!(to_rc(Err(FsError::NotFound)), -libc::ENOENT);
        assert_eq!(to_rc(Err(FsError::FileTooLarge)), -libc::EFBIG);
        assert_eq!(to_rc(Err(FsError::Unsupported)), -libc::ENOSYS);
    }

    #[test]
    fn parses_open_flags() {
        assert_eq!(
            OpenOptions::from_raw(libc::O_RDWR | libc::O_APPEND).access_mode(),
            AccessMode::ReadWrite
        );
        assert!(OpenOptions::from_raw(libc::O_WRONLY | libc::O_TRUNC).writable());
        assert!(OpenOptions::from_raw(libc::O_WRONLY | libc::O_TRUNC).truncate());
    }
}
