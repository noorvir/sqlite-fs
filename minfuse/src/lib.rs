use libc::{c_char, c_int, c_void, off_t, size_t};
use std::ffi::{CStr, CString};
use std::io;
use std::path::Path;
use std::ptr;
use std::time::{SystemTime, UNIX_EPOCH};

pub type FsResult<T> = Result<T, FsError>;

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
    pub modified: SystemTime,
}

impl Attr {
    pub fn file(len: u64) -> Self {
        Self {
            kind: EntryKind::File,
            len,
            perm: 0o644,
            modified: SystemTime::now(),
        }
    }

    pub fn directory() -> Self {
        Self {
            kind: EntryKind::Directory,
            len: 0,
            perm: 0o755,
            modified: SystemTime::now(),
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
    NotDirectory,
    IsDirectory,
    InvalidInput,
    FileTooLarge,
    PermissionDenied,
    ReadOnly,
    Io,
}

impl FsError {
    fn errno(&self) -> i32 {
        match self {
            FsError::NotFound => libc::ENOENT,
            FsError::NotDirectory => libc::ENOTDIR,
            FsError::IsDirectory => libc::EISDIR,
            FsError::InvalidInput => libc::EINVAL,
            FsError::FileTooLarge => libc::EFBIG,
            FsError::PermissionDenied => libc::EACCES,
            FsError::ReadOnly => libc::EROFS,
            FsError::Io => libc::EIO,
        }
    }
}

pub trait FileSystem: Send + Sync + 'static {
    fn getattr(&self, path: &str) -> FsResult<Attr>;
    fn readdir(&self, path: &str) -> FsResult<Vec<DirEntry>>;

    fn open(&self, path: &str) -> FsResult<()> {
        match self.getattr(path)?.kind {
            EntryKind::File => Ok(()),
            EntryKind::Directory => Err(FsError::IsDirectory),
        }
    }

    fn read(&self, path: &str, offset: u64, size: usize) -> FsResult<Vec<u8>>;
    fn write(&self, path: &str, offset: u64, data: &[u8]) -> FsResult<usize>;
    fn truncate(&self, path: &str, size: u64) -> FsResult<()>;
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

#[repr(C)]
struct FuseFileInfo {
    _private: [u8; 0],
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
    readdir: extern "C" fn(*mut c_void, *const c_char, *mut c_void, FuseFillDir, off_t) -> c_int,
    open: extern "C" fn(*mut c_void, *const c_char, *mut FuseFileInfo) -> c_int,
    read: extern "C" fn(
        *mut c_void,
        *const c_char,
        *mut c_char,
        size_t,
        off_t,
        *mut FuseFileInfo,
    ) -> c_int,
    write: extern "C" fn(
        *mut c_void,
        *const c_char,
        *const c_char,
        size_t,
        off_t,
        *mut FuseFileInfo,
    ) -> c_int,
    truncate: extern "C" fn(*mut c_void, *const c_char, off_t) -> c_int,
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
    let mut state = Box::new(MountState { fs: Box::new(fs) });
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
    args.extend(["-o".to_string(), format!("volname={}", options.volname)]);
    args.push(mountpoint.as_ref().to_string_lossy().to_string());

    let cstrings: Vec<CString> = args
        .into_iter()
        .map(|arg| CString::new(arg).map_err(|_| io::ErrorKind::InvalidInput))
        .collect::<Result<_, _>>()?;
    let mut argv: Vec<*mut c_char> = cstrings.iter().map(|s| s.as_ptr() as *mut c_char).collect();
    let mut callbacks = Callbacks {
        getattr: cb_getattr,
        readdir: cb_readdir,
        open: cb_open,
        read: cb_read,
        write: cb_write,
        truncate: cb_truncate,
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

fn path(path: *const c_char) -> FsResult<String> {
    if path.is_null() {
        return Err(FsError::InvalidInput);
    }
    Ok(unsafe { CStr::from_ptr(path) }
        .to_string_lossy()
        .into_owned())
}

fn to_rc(result: FsResult<c_int>) -> c_int {
    match result {
        Ok(value) => value,
        Err(err) => -err.errno(),
    }
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
            fill_stat(st, &attr);
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

extern "C" fn cb_open(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    _fi: *mut FuseFileInfo,
) -> c_int {
    catch(|| {
        to_rc((|| {
            state(user_data).fs.open(&path(path_ptr)?)?;
            Ok(0)
        })())
    })
}

extern "C" fn cb_read(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    out: *mut c_char,
    size: size_t,
    off: off_t,
    _fi: *mut FuseFileInfo,
) -> c_int {
    catch(|| {
        to_rc((|| {
            let offset = u64::try_from(off).map_err(|_| FsError::InvalidInput)?;
            let data = state(user_data).fs.read(&path(path_ptr)?, offset, size)?;
            let len = data.len().min(size);
            unsafe { ptr::copy_nonoverlapping(data.as_ptr(), out as *mut u8, len) };
            Ok(len as c_int)
        })())
    })
}

extern "C" fn cb_write(
    user_data: *mut c_void,
    path_ptr: *const c_char,
    input: *const c_char,
    size: size_t,
    off: off_t,
    _fi: *mut FuseFileInfo,
) -> c_int {
    catch(|| {
        to_rc((|| {
            let offset = u64::try_from(off).map_err(|_| FsError::InvalidInput)?;
            let input = unsafe { std::slice::from_raw_parts(input as *const u8, size) };
            let written = state(user_data).fs.write(&path(path_ptr)?, offset, input)?;
            Ok(written as c_int)
        })())
    })
}

extern "C" fn cb_truncate(user_data: *mut c_void, path_ptr: *const c_char, size: off_t) -> c_int {
    catch(|| {
        to_rc((|| {
            let size = u64::try_from(size).map_err(|_| FsError::InvalidInput)?;
            state(user_data).fs.truncate(&path(path_ptr)?, size)?;
            Ok(0)
        })())
    })
}

fn fill_dir(buf: *mut c_void, filler: FuseFillDir, name: &str) -> FsResult<()> {
    let name = CString::new(name).map_err(|_| FsError::InvalidInput)?;
    let full = unsafe { filler(buf, name.as_ptr(), ptr::null(), 0, 0) } != 0;
    if full { Err(FsError::Io) } else { Ok(()) }
}

fn fill_stat(st: *mut libc::stat, attr: &Attr) {
    unsafe { ptr::write_bytes(st, 0, 1) };
    let mtime = attr
        .modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    unsafe {
        (*st).st_uid = libc::getuid();
        (*st).st_gid = libc::getgid();
        (*st).st_atime = mtime;
        (*st).st_mtime = mtime;
        (*st).st_ctime = mtime;
        #[cfg(target_os = "macos")]
        {
            (*st).st_birthtime = mtime;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_mapping_is_negative_at_boundary() {
        assert_eq!(to_rc(Err(FsError::NotFound)), -libc::ENOENT);
        assert_eq!(to_rc(Err(FsError::FileTooLarge)), -libc::EFBIG);
    }
}
