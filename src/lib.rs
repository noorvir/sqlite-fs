mod document;
mod error;
mod route;
mod schema;

pub use error::Error;

use document::{invalid_entry, parse_markdown, render_document};
use error::{Result, is_semantic_sql_error, to_fs_error};
use minfuse::{
    Attr, DirEntry, EntryKind, FileHandle, FileSystem, FsError, FsResult, NO_HANDLE, OpenOptions,
    RenameFlags, SetTimes, StatFs,
};
use route::{DocPath, Route, is_typed_file_name, path_to_str, route};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Transaction, params, params_from_iter};
#[cfg(test)]
use schema::short_suffix;
use schema::{
    Column, TableSchema, eligible_tables, generic_default, json_to_sql, quote_ident, table_schema,
};
use serde_json::{Map as JsonMap, Value as JsonValue};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions as StdOpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct SqliteFs {
    backing: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    conn: Connection,
    handles: HashMap<FileHandle, OpenFile>,
    next_handle: FileHandle,
    // Typed rows do not carry POSIX timestamps. Keep stable attrs so editors do
    // not see a file as externally modified on every getattr.
    file_times: HashMap<DocPath, SystemTime>,
    mount_time: SystemTime,
}

enum OpenFile {
    Typed(TypedOpenFile),
    Pass(PassOpenFile),
}

#[derive(Clone)]
struct TypedOpenFile {
    path: DocPath,
    staged: Vec<u8>,
    dirty: bool,
    writable: bool,
    append: bool,
}

struct PassOpenFile {
    path: PathBuf,
    file: File,
    writable: bool,
}

impl SqliteFs {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        Self::open_with_backing(path, default_backing_path(path))
    }

    pub fn open_with_backing(db: impl AsRef<Path>, backing: impl AsRef<Path>) -> Result<Self> {
        let backing = backing.as_ref().to_path_buf();
        fs::create_dir_all(&backing)?;
        let conn = Connection::open(db)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self {
            backing,
            inner: Mutex::new(Inner {
                conn,
                handles: HashMap::new(),
                next_handle: 1,
                file_times: HashMap::new(),
                mount_time: SystemTime::now(),
            }),
        })
    }

    fn alloc_handle(inner: &mut Inner, open: OpenFile) -> FileHandle {
        let handle = inner.next_handle;
        inner.next_handle += 1;
        inner.handles.insert(handle, open);
        handle
    }

    fn commit_handle(inner: &mut Inner, handle: FileHandle) -> Result<()> {
        if handle == NO_HANDLE {
            return Ok(());
        }
        let Some(open) = inner.handles.get(&handle) else {
            return Err(FsError::BadFileDescriptor.into());
        };
        let typed_commit = match open {
            OpenFile::Typed(open) if open.dirty => Some((open.path.clone(), open.staged.clone())),
            _ => None,
        };
        if let Some((path, staged)) = typed_commit {
            commit_document(&mut inner.conn, &path, &staged)?;
            mark_typed_modified(inner, &path);
            if let Some(OpenFile::Typed(open)) = inner.handles.get_mut(&handle) {
                open.dirty = false;
            }
        }
        if let Some(OpenFile::Pass(open)) = inner.handles.get_mut(&handle) {
            open.file.sync_data()?;
        }
        Ok(())
    }
}

impl FileSystem for SqliteFs {
    fn getattr(&self, path: &str) -> FsResult<Attr> {
        self.try_getattr(path).map_err(to_fs_error)
    }

    fn readdir(&self, path: &str) -> FsResult<Vec<DirEntry>> {
        self.try_readdir(path).map_err(to_fs_error)
    }

    fn open(&self, path: &str, options: OpenOptions) -> FsResult<FileHandle> {
        self.try_open(path, options).map_err(to_fs_error)
    }

    fn create(&self, path: &str, _mode: u32, options: OpenOptions) -> FsResult<FileHandle> {
        self.try_create(path, options).map_err(to_fs_error)
    }

    fn read(&self, path: &str, handle: FileHandle, offset: u64, size: usize) -> FsResult<Vec<u8>> {
        self.try_read(path, handle, offset, size)
            .map_err(to_fs_error)
    }

    fn write(&self, _path: &str, handle: FileHandle, offset: u64, data: &[u8]) -> FsResult<usize> {
        self.try_write(handle, offset, data).map_err(to_fs_error)
    }

    fn flush(&self, _path: &str, handle: FileHandle) -> FsResult<()> {
        let mut inner = self.inner.lock().unwrap();
        Self::commit_handle(&mut inner, handle).map_err(to_fs_error)
    }

    fn fsync(&self, _path: &str, handle: FileHandle, _datasync: bool) -> FsResult<()> {
        let mut inner = self.inner.lock().unwrap();
        Self::commit_handle(&mut inner, handle).map_err(to_fs_error)
    }

    fn release(&self, _path: &str, handle: FileHandle) -> FsResult<()> {
        let mut inner = self.inner.lock().unwrap();
        let result = Self::commit_handle(&mut inner, handle);
        inner.handles.remove(&handle);
        result.map_err(to_fs_error)
    }

    fn truncate(&self, path: &str, handle: Option<FileHandle>, size: u64) -> FsResult<()> {
        self.try_truncate(path, handle, size).map_err(to_fs_error)
    }

    fn mknod(&self, path: &str, _mode: u32, _rdev: u64) -> FsResult<()> {
        self.try_mknod(path).map_err(to_fs_error)
    }

    fn mkdir(&self, path: &str, _mode: u32) -> FsResult<()> {
        self.try_mkdir(path).map_err(to_fs_error)
    }

    fn unlink(&self, path: &str) -> FsResult<()> {
        self.try_unlink(path).map_err(to_fs_error)
    }

    fn rmdir(&self, path: &str) -> FsResult<()> {
        self.try_rmdir(path).map_err(to_fs_error)
    }

    fn rename(&self, from: &str, to: &str, flags: RenameFlags) -> FsResult<()> {
        self.try_rename(from, to, flags).map_err(to_fs_error)
    }

    fn chmod(&self, path: &str, handle: Option<FileHandle>, _mode: u32) -> FsResult<()> {
        self.try_metadata_noop(path, handle).map_err(to_fs_error)
    }

    fn chown(&self, path: &str, handle: Option<FileHandle>, _uid: u32, _gid: u32) -> FsResult<()> {
        self.try_metadata_noop(path, handle).map_err(to_fs_error)
    }

    fn utimens(&self, path: &str, handle: Option<FileHandle>, times: SetTimes) -> FsResult<()> {
        self.try_utimens(path, handle, times).map_err(to_fs_error)
    }

    fn statfs(&self, _path: &str) -> FsResult<StatFs> {
        Ok(StatFs::default())
    }
}

impl SqliteFs {
    fn try_getattr(&self, path: &str) -> Result<Attr> {
        let inner = self.inner.lock().unwrap();
        match route(&inner.conn, &self.backing, path)? {
            Route::Root | Route::TableDir(_) => Ok(Attr::directory()),
            Route::Typed(doc) => self.typed_attr(&inner, &doc),
            Route::Pass(path) => Ok(attr_from_metadata(&fs::metadata(path)?)),
        }
    }

    fn typed_attr(&self, inner: &Inner, doc: &DocPath) -> Result<Attr> {
        let rendered = render_document(&inner.conn, doc)?;
        let time = typed_time(inner, doc);
        let mut attr = Attr::file(rendered.len() as u64);
        attr.accessed = time;
        attr.modified = time;
        attr.changed = time;
        attr.created = time;
        Ok(attr)
    }

    fn try_readdir(&self, path: &str) -> Result<Vec<DirEntry>> {
        let inner = self.inner.lock().unwrap();
        match route(&inner.conn, &self.backing, path)? {
            Route::Root => {
                let mut entries = pass_dir_entries(&self.backing)?;
                for table in eligible_tables(&inner.conn)? {
                    entries.insert(table, EntryKind::Directory);
                }
                Ok(entries_to_dir_entries(entries))
            }
            Route::TableDir(table) => {
                let mut entries = pass_dir_entries(&self.backing.join(&table))?;
                for file in typed_files(&inner, &table)? {
                    entries.insert(file, EntryKind::File);
                }
                Ok(entries_to_dir_entries(entries))
            }
            Route::Pass(path) => Ok(entries_to_dir_entries(pass_dir_entries(&path)?)),
            Route::Typed(_) => Err(FsError::NotDirectory.into()),
        }
    }

    fn try_open(&self, path: &str, options: OpenOptions) -> Result<FileHandle> {
        let mut inner = self.inner.lock().unwrap();
        match route(&inner.conn, &self.backing, path)? {
            Route::Typed(doc) => {
                let mut staged = render_document(&inner.conn, &doc)?.into_bytes();
                let mut dirty = false;
                if options.truncate() {
                    if !options.writable() {
                        return Err(FsError::InvalidInput.into());
                    }
                    staged.clear();
                    dirty = true;
                }
                Ok(Self::alloc_handle(
                    &mut inner,
                    OpenFile::Typed(TypedOpenFile {
                        path: doc,
                        staged,
                        dirty,
                        writable: options.writable(),
                        append: options.append(),
                    }),
                ))
            }
            Route::Pass(path) => {
                let file = open_pass_file(&path, options, false)?;
                Ok(Self::alloc_handle(
                    &mut inner,
                    OpenFile::Pass(PassOpenFile {
                        path,
                        file,
                        writable: options.writable(),
                    }),
                ))
            }
            Route::Root | Route::TableDir(_) => Err(FsError::IsDirectory.into()),
        }
    }

    fn try_create(&self, path: &str, options: OpenOptions) -> Result<FileHandle> {
        let mut inner = self.inner.lock().unwrap();
        match route(&inner.conn, &self.backing, path)? {
            Route::Typed(doc) => {
                if document_exists(&inner.conn, &doc)? {
                    return Err(FsError::Exists.into());
                }
                Ok(Self::alloc_handle(
                    &mut inner,
                    OpenFile::Typed(TypedOpenFile {
                        path: doc,
                        staged: Vec::new(),
                        dirty: true,
                        writable: options.writable(),
                        append: options.append(),
                    }),
                ))
            }
            Route::Pass(path) => {
                ensure_virtual_parent(&inner.conn, &self.backing, path_to_str(&path)?)?;
                let file = open_pass_file(&path, options, true)?;
                Ok(Self::alloc_handle(
                    &mut inner,
                    OpenFile::Pass(PassOpenFile {
                        path,
                        file,
                        writable: options.writable(),
                    }),
                ))
            }
            Route::Root | Route::TableDir(_) => Err(FsError::IsDirectory.into()),
        }
    }

    fn try_read(
        &self,
        path: &str,
        handle: FileHandle,
        offset: u64,
        size: usize,
    ) -> Result<Vec<u8>> {
        if handle != NO_HANDLE {
            let mut inner = self.inner.lock().unwrap();
            let open = inner
                .handles
                .get_mut(&handle)
                .ok_or(FsError::BadFileDescriptor)?;
            return match open {
                OpenFile::Typed(open) => read_slice(&open.staged, offset, size),
                OpenFile::Pass(open) => read_pass_file(&mut open.file, offset, size),
            };
        }

        let inner = self.inner.lock().unwrap();
        match route(&inner.conn, &self.backing, path)? {
            Route::Typed(doc) => read_slice(
                &render_document(&inner.conn, &doc)?.into_bytes(),
                offset,
                size,
            ),
            Route::Pass(path) => {
                let mut file = File::open(path)?;
                read_pass_file(&mut file, offset, size)
            }
            Route::Root | Route::TableDir(_) => Err(FsError::IsDirectory.into()),
        }
    }

    fn try_write(&self, handle: FileHandle, offset: u64, input: &[u8]) -> Result<usize> {
        let mut inner = self.inner.lock().unwrap();
        let open = inner
            .handles
            .get_mut(&handle)
            .ok_or(FsError::BadFileDescriptor)?;
        match open {
            OpenFile::Typed(open) => {
                if !open.writable {
                    return Err(FsError::BadFileDescriptor.into());
                }
                let start = if open.append {
                    open.staged.len()
                } else {
                    usize::try_from(offset).map_err(|_| FsError::InvalidInput)?
                };
                let end = start
                    .checked_add(input.len())
                    .ok_or(FsError::FileTooLarge)?;
                if end > open.staged.len() {
                    open.staged.resize(end, 0);
                }
                open.staged[start..end].copy_from_slice(input);
                open.dirty = true;
                Ok(input.len())
            }
            OpenFile::Pass(open) => {
                if !open.writable {
                    return Err(FsError::BadFileDescriptor.into());
                }
                open.file.seek(SeekFrom::Start(offset))?;
                open.file.write_all(input)?;
                Ok(input.len())
            }
        }
    }

    fn try_truncate(&self, path: &str, handle: Option<FileHandle>, size: u64) -> Result<()> {
        let size_usize = usize::try_from(size).map_err(|_| FsError::FileTooLarge)?;
        let mut inner = self.inner.lock().unwrap();
        if let Some(handle) = handle {
            let open = inner
                .handles
                .get_mut(&handle)
                .ok_or(FsError::BadFileDescriptor)?;
            return match open {
                OpenFile::Typed(open) => {
                    if !open.writable {
                        return Err(FsError::BadFileDescriptor.into());
                    }
                    open.staged.resize(size_usize, 0);
                    open.dirty = true;
                    Ok(())
                }
                OpenFile::Pass(open) => {
                    if !open.writable {
                        return Err(FsError::BadFileDescriptor.into());
                    }
                    open.file.set_len(size)?;
                    Ok(())
                }
            };
        }

        match route(&inner.conn, &self.backing, path)? {
            Route::Typed(doc) => {
                let mut staged = render_document(&inner.conn, &doc)?.into_bytes();
                staged.resize(size_usize, 0);
                commit_document(&mut inner.conn, &doc, &staged)?;
                mark_typed_modified(&mut inner, &doc);
                Ok(())
            }
            Route::Pass(path) => {
                let file = StdOpenOptions::new().write(true).open(path)?;
                file.set_len(size)?;
                Ok(())
            }
            Route::Root | Route::TableDir(_) => Err(FsError::IsDirectory.into()),
        }
    }

    fn try_mknod(&self, path: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        match route(&inner.conn, &self.backing, path)? {
            Route::Typed(doc) => {
                if document_exists(&inner.conn, &doc)? {
                    return Err(FsError::Exists.into());
                }
                commit_document(&mut inner.conn, &doc, b"")?;
                mark_typed_modified(&mut inner, &doc);
                Ok(())
            }
            Route::Pass(path) => {
                ensure_virtual_parent(&inner.conn, &self.backing, path_to_str(&path)?)?;
                StdOpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)?;
                Ok(())
            }
            Route::Root | Route::TableDir(_) => Err(FsError::IsDirectory.into()),
        }
    }

    fn try_mkdir(&self, path: &str) -> Result<()> {
        let inner = self.inner.lock().unwrap();
        match route(&inner.conn, &self.backing, path)? {
            Route::Pass(path) => {
                ensure_virtual_parent(&inner.conn, &self.backing, path_to_str(&path)?)?;
                fs::create_dir(path)?;
                Ok(())
            }
            Route::Root | Route::TableDir(_) => Err(FsError::Exists.into()),
            Route::Typed(_) => Err(FsError::NotDirectory.into()),
        }
    }

    fn try_unlink(&self, path: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        match route(&inner.conn, &self.backing, path)? {
            Route::Typed(doc) => {
                if has_open_typed_handle(&inner, &doc) {
                    return Err(FsError::PermissionDenied.into());
                }
                delete_typed(&inner.conn, &doc)?;
                inner.file_times.remove(&doc);
                Ok(())
            }
            Route::Pass(path) => {
                fs::remove_file(path)?;
                Ok(())
            }
            Route::Root | Route::TableDir(_) => Err(FsError::IsDirectory.into()),
        }
    }

    fn try_rmdir(&self, path: &str) -> Result<()> {
        let inner = self.inner.lock().unwrap();
        match route(&inner.conn, &self.backing, path)? {
            Route::Pass(path) => {
                fs::remove_dir(path)?;
                Ok(())
            }
            Route::Root | Route::TableDir(_) => Err(FsError::PermissionDenied.into()),
            Route::Typed(_) => Err(FsError::NotDirectory.into()),
        }
    }

    fn try_metadata_noop(&self, path: &str, handle: Option<FileHandle>) -> Result<()> {
        let inner = self.inner.lock().unwrap();
        if let Some(handle) = handle {
            if inner.handles.contains_key(&handle) {
                return Ok(());
            }
            return Err(FsError::BadFileDescriptor.into());
        }
        match route(&inner.conn, &self.backing, path)? {
            Route::Root | Route::TableDir(_) => Ok(()),
            Route::Typed(doc) => document_exists(&inner.conn, &doc).and_then(|exists| {
                if exists {
                    Ok(())
                } else {
                    Err(FsError::NotFound.into())
                }
            }),
            Route::Pass(path) => fs::metadata(path).map(|_| ()).map_err(Into::into),
        }
    }

    fn try_utimens(&self, path: &str, handle: Option<FileHandle>, times: SetTimes) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(handle) = handle {
            return match inner.handles.get(&handle) {
                Some(OpenFile::Typed(open)) => {
                    let doc = open.path.clone();
                    let current = typed_time(&inner, &doc);
                    let modified = times.modified.resolve(current);
                    inner.file_times.insert(doc, modified);
                    Ok(())
                }
                Some(OpenFile::Pass(open)) => set_file_times_fd(&open.file, times),
                None => Err(FsError::BadFileDescriptor.into()),
            };
        }
        match route(&inner.conn, &self.backing, path)? {
            Route::Root | Route::TableDir(_) => Ok(()),
            Route::Typed(doc) => {
                if !document_exists(&inner.conn, &doc)? {
                    return Err(FsError::NotFound.into());
                }
                let current = typed_time(&inner, &doc);
                let modified = times.modified.resolve(current);
                inner.file_times.insert(doc, modified);
                Ok(())
            }
            Route::Pass(path) => set_file_times_path(&path, times),
        }
    }

    fn try_rename(&self, from: &str, to: &str, flags: RenameFlags) -> Result<()> {
        if flags.exchange() || flags.seclude() {
            return Err(FsError::Unsupported.into());
        }

        let mut inner = self.inner.lock().unwrap();
        // Renames are allowed to cross the overlay boundary. A typed path is a
        // SQLite row; a passthrough path is a backing-file path.
        match (
            route(&inner.conn, &self.backing, from)?,
            route(&inner.conn, &self.backing, to)?,
        ) {
            (Route::Typed(from), Route::Pass(to_path)) => {
                move_typed_to_pass(&mut inner, from, to_path, flags.no_replace())
            }
            (Route::Typed(from), Route::Typed(to)) => {
                rename_typed(&mut inner, from, to, flags.no_replace())
            }
            (Route::Pass(from_path), Route::Pass(to_path)) => {
                ensure_virtual_parent(&inner.conn, &self.backing, path_to_str(&to_path)?)?;
                if flags.no_replace() && to_path.exists() {
                    return Err(FsError::Exists.into());
                }
                fs::rename(from_path, to_path)?;
                Ok(())
            }
            (Route::Pass(from_path), Route::Typed(to)) => {
                if flags.no_replace() && document_exists(&inner.conn, &to)? {
                    return Err(FsError::Exists.into());
                }
                rename_pass_to_typed(&mut inner, from_path, to)
            }
            _ => Err(FsError::Unsupported.into()),
        }
    }
}

fn default_backing_path(db: &Path) -> PathBuf {
    PathBuf::from(format!("{}.files", db.display()))
}

fn ensure_virtual_parent(conn: &Connection, backing: &Path, path: &str) -> Result<()> {
    let relative = Path::new(path)
        .strip_prefix(backing)
        .map_err(|_| FsError::InvalidInput)?;
    let mut parts = relative.components();
    let Some(first) = parts.next() else {
        return Ok(());
    };
    if parts.next().is_none() {
        return Ok(());
    }
    let table = first.as_os_str().to_string_lossy();
    if table_schema(conn, &table).is_ok() {
        fs::create_dir_all(backing.join(table.as_ref()))?;
    }
    Ok(())
}

fn attr_from_metadata(metadata: &fs::Metadata) -> Attr {
    let kind = if metadata.is_dir() {
        EntryKind::Directory
    } else {
        EntryKind::File
    };
    Attr {
        kind,
        len: metadata.len(),
        perm: (metadata.mode() & 0o777) as u16,
        uid: metadata.uid(),
        gid: metadata.gid(),
        accessed: metadata.accessed().unwrap_or_else(|_| SystemTime::now()),
        modified: metadata.modified().unwrap_or_else(|_| SystemTime::now()),
        changed: SystemTime::now(),
        created: metadata.created().unwrap_or_else(|_| SystemTime::now()),
    }
}

fn pass_dir_entries(path: &Path) -> Result<BTreeMap<String, EntryKind>> {
    let mut entries = BTreeMap::new();
    match fs::read_dir(path) {
        Ok(read_dir) => {
            for entry in read_dir {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                let kind = if entry.file_type()?.is_dir() {
                    EntryKind::Directory
                } else {
                    EntryKind::File
                };
                entries.insert(name, kind);
            }
            Ok(entries)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(entries),
        Err(err) => Err(err.into()),
    }
}

fn typed_files(inner: &Inner, table: &str) -> Result<BTreeSet<String>> {
    let schema = table_schema(&inner.conn, table)?;
    let mut stmt = inner.conn.prepare(&format!(
        "SELECT {} FROM {} ORDER BY {}",
        quote_ident("_slfs_path"),
        quote_ident(schema.name()),
        quote_ident("_slfs_path")
    ))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter(|name| is_typed_file_name(name))
        .collect())
}

fn entries_to_dir_entries(entries: BTreeMap<String, EntryKind>) -> Vec<DirEntry> {
    entries
        .into_iter()
        .map(|(name, kind)| match kind {
            EntryKind::Directory => DirEntry::directory(name),
            EntryKind::File => DirEntry::file(name),
        })
        .collect()
}

fn open_pass_file(path: &Path, options: OpenOptions, create: bool) -> Result<File> {
    let mut open = StdOpenOptions::new();
    open.read(options.readable() || options.writable())
        .write(options.writable())
        .append(options.append())
        .truncate(options.truncate());
    if create {
        open.create_new(true).write(true).read(true);
    }
    Ok(open.open(path)?)
}

fn read_pass_file(file: &mut File, offset: u64, size: usize) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut out = vec![0; size];
    let len = file.read(&mut out)?;
    out.truncate(len);
    Ok(out)
}

fn set_file_times_fd(file: &File, times: SetTimes) -> Result<()> {
    let times = [
        time_to_timespec(times.accessed)?,
        time_to_timespec(times.modified)?,
    ];
    let rc = unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

fn set_file_times_path(path: &Path, times: SetTimes) -> Result<()> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| FsError::InvalidInput)?;
    let times = [
        time_to_timespec(times.accessed)?,
        time_to_timespec(times.modified)?,
    ];
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

fn time_to_timespec(time: minfuse::SetTime) -> Result<libc::timespec> {
    match time {
        minfuse::SetTime::Specific(time) => {
            let duration = time
                .duration_since(UNIX_EPOCH)
                .map_err(|_| FsError::InvalidInput)?;
            Ok(libc::timespec {
                tv_sec: duration.as_secs() as libc::time_t,
                tv_nsec: duration.subsec_nanos() as libc::c_long,
            })
        }
        minfuse::SetTime::Now => Ok(libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_NOW as libc::c_long,
        }),
        minfuse::SetTime::Omit => Ok(libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT as libc::c_long,
        }),
    }
}

fn delete_typed(conn: &Connection, doc: &DocPath) -> Result<()> {
    let schema = table_schema(conn, doc.table())?;
    let changed = conn.execute(
        &format!(
            "DELETE FROM {} WHERE {} = ?1",
            quote_ident(schema.name()),
            quote_ident("_slfs_path")
        ),
        params![doc.file()],
    )?;
    if changed == 0 {
        Err(FsError::NotFound.into())
    } else {
        Ok(())
    }
}

fn move_typed_to_pass(
    inner: &mut Inner,
    from: DocPath,
    to_path: PathBuf,
    no_replace: bool,
) -> Result<()> {
    // Export the rendered document, then remove the row. This is a real rename,
    // not a cache write; after success the typed path no longer exists.
    if has_writable_open_typed_handle(inner, &from) {
        return Err(FsError::PermissionDenied.into());
    }
    if no_replace && to_path.exists() {
        return Err(FsError::Exists.into());
    }
    if !document_exists(&inner.conn, &from)? {
        return Err(FsError::NotFound.into());
    }
    if let Some(parent) = to_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(to_path, render_document(&inner.conn, &from)?)?;
    delete_typed(&inner.conn, &from)?;
    inner.file_times.remove(&from);
    Ok(())
}

fn typed_time(inner: &Inner, doc: &DocPath) -> SystemTime {
    inner
        .file_times
        .get(doc)
        .copied()
        .unwrap_or(inner.mount_time)
}

fn mark_typed_modified(inner: &mut Inner, doc: &DocPath) {
    inner.file_times.insert(doc.clone(), SystemTime::now());
}

fn rename_pass_to_typed(inner: &mut Inner, from_path: PathBuf, to: DocPath) -> Result<()> {
    // Editors commonly rename an open temp file over the destination. Preserve
    // fd semantics by converting matching passthrough handles to typed handles.
    if has_writable_open_typed_handle(inner, &to) {
        return Err(FsError::PermissionDenied.into());
    }

    for open in inner.handles.values_mut() {
        if let OpenFile::Pass(open) = open
            && open.path == from_path
        {
            open.file.sync_data()?;
        }
    }

    let bytes = fs::read(&from_path)?;
    commit_document(&mut inner.conn, &to, &bytes)?;
    fs::remove_file(&from_path)?;

    for open in inner.handles.values_mut() {
        if let OpenFile::Pass(pass) = open
            && pass.path == from_path
        {
            *open = OpenFile::Typed(TypedOpenFile {
                path: to.clone(),
                staged: bytes.clone(),
                dirty: false,
                writable: pass.writable,
                append: false,
            });
        }
    }

    mark_typed_modified(inner, &to);
    Ok(())
}

fn rename_typed(inner: &mut Inner, from: DocPath, to: DocPath, no_replace: bool) -> Result<()> {
    if from.table() != to.table() {
        return Err(FsError::Unsupported.into());
    }
    if from.file() == to.file() {
        return if document_exists(&inner.conn, &from)? {
            Ok(())
        } else {
            Err(FsError::NotFound.into())
        };
    }
    if has_open_typed_handle(inner, &from) || has_open_typed_handle(inner, &to) {
        return Err(FsError::PermissionDenied.into());
    }

    let schema = table_schema(&inner.conn, from.table())?;
    let tx = inner.conn.transaction()?;
    if !row_exists(&tx, &schema, from.file())? {
        return Err(FsError::NotFound.into());
    }
    if no_replace && row_exists(&tx, &schema, to.file())? {
        return Err(FsError::Exists.into());
    }
    if !no_replace {
        tx.execute(
            &format!(
                "DELETE FROM {} WHERE {} = ?1",
                quote_ident(schema.name()),
                quote_ident("_slfs_path")
            ),
            params![to.file()],
        )?;
    }
    tx.execute(
        &format!(
            "UPDATE {} SET {} = ?1 WHERE {} = ?2",
            quote_ident(schema.name()),
            quote_ident("_slfs_path"),
            quote_ident("_slfs_path")
        ),
        params![to.file(), from.file()],
    )?;
    tx.commit()?;
    inner.file_times.remove(&from);
    mark_typed_modified(inner, &to);
    Ok(())
}

fn has_open_typed_handle(inner: &Inner, doc: &DocPath) -> bool {
    inner.handles.values().any(|open| match open {
        OpenFile::Typed(open) => &open.path == doc,
        OpenFile::Pass(_) => false,
    })
}

fn has_writable_open_typed_handle(inner: &Inner, doc: &DocPath) -> bool {
    inner.handles.values().any(|open| match open {
        OpenFile::Typed(open) => &open.path == doc && open.writable,
        OpenFile::Pass(_) => false,
    })
}

fn commit_document(conn: &mut Connection, doc: &DocPath, bytes: &[u8]) -> Result<()> {
    let text = std::str::from_utf8(bytes)?;
    let (props, body) = parse_markdown(text)?;
    let schema = table_schema(conn, doc.table())?;
    let tx = conn.transaction()?;

    let existed = row_exists(&tx, &schema, doc.file())?;
    let mut invalid = if existed {
        let mut invalid = load_invalid_update(&tx, &schema, doc.file())?;
        invalid.retain(|key, _| props.contains_key(key));
        invalid
    } else {
        JsonMap::new()
    };

    let mut updates = Vec::new();
    for (name, value) in props {
        if name.starts_with("_slfs_") {
            invalid.insert(name, invalid_entry(value, "reserved property"));
            continue;
        }
        let Some(column) = schema.column(&name) else {
            invalid.insert(name, invalid_entry(value, "unknown property"));
            continue;
        };
        if let Some(error) = column.property_type_error(&value) {
            invalid.insert(name, invalid_entry(value, error));
            continue;
        }
        updates.push((column, value));
    }

    if existed {
        tx.execute(
            &format!(
                "UPDATE {} SET {} = ?1 WHERE {} = ?2",
                quote_ident(schema.name()),
                quote_ident("_slfs_content"),
                quote_ident("_slfs_path")
            ),
            params![body, doc.file()],
        )?;
        apply_property_updates(&tx, &schema, doc.file(), &updates, &mut invalid)?;
    } else {
        let consumed =
            insert_document_row(&tx, &schema, doc.file(), &body, &updates, &mut invalid)?;
        let remaining = updates
            .iter()
            .filter(|(column, _)| !consumed.contains(column.name()))
            .map(|(column, value)| (*column, value.clone()))
            .collect::<Vec<_>>();
        apply_property_updates(&tx, &schema, doc.file(), &remaining, &mut invalid)?;
    }

    let invalid_json = serde_json::to_string(&invalid)?;
    tx.execute(
        &format!(
            "UPDATE {} SET {} = ?1 WHERE {} = ?2",
            quote_ident(schema.name()),
            quote_ident("_slfs_invalid_update"),
            quote_ident("_slfs_path")
        ),
        params![invalid_json, doc.file()],
    )?;
    tx.commit()?;
    Ok(())
}

fn insert_document_row(
    tx: &Transaction<'_>,
    schema: &TableSchema,
    file: &str,
    body: &str,
    updates: &[(&Column, JsonValue)],
    invalid: &mut JsonMap<String, JsonValue>,
) -> Result<BTreeSet<String>> {
    let supplied_required = updates
        .iter()
        .filter(|(column, _)| column.needs_insert_value())
        .map(|(column, value)| (*column, value.clone()))
        .collect::<Vec<_>>();
    let consumed = supplied_required
        .iter()
        .map(|(column, _)| column.name().to_string())
        .collect::<BTreeSet<_>>();

    match try_insert_document_row(tx, schema, file, body, &supplied_required, invalid) {
        Ok(()) => Ok(consumed),
        Err(err) if is_semantic_sql_error(&err) && !supplied_required.is_empty() => {
            mark_constraint_failed(invalid, &supplied_required);
            try_insert_document_row(tx, schema, file, body, &[], invalid)?;
            Ok(consumed)
        }
        Err(err) => Err(err),
    }
}

fn try_insert_document_row(
    tx: &Transaction<'_>,
    schema: &TableSchema,
    file: &str,
    body: &str,
    supplied_required: &[(&Column, JsonValue)],
    invalid: &JsonMap<String, JsonValue>,
) -> Result<()> {
    let mut columns = vec![
        "_slfs_path".to_string(),
        "_slfs_content".to_string(),
        "_slfs_invalid_update".to_string(),
    ];
    let mut values = vec![
        SqlValue::Text(file.to_string()),
        SqlValue::Text(body.to_string()),
        SqlValue::Text(serde_json::to_string(invalid)?),
    ];

    for (column, value) in supplied_required {
        columns.push(column.name().to_string());
        values.push(json_to_sql(value));
    }

    for column in schema.domain_columns() {
        let already_supplied = supplied_required
            .iter()
            .any(|(supplied, _)| supplied.name() == column.name());
        if !already_supplied && column.needs_insert_value() {
            columns.push(column.name().to_string());
            values.push(generic_default(column));
        }
    }

    let quoted_columns = columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders = (1..=values.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    tx.execute(
        &format!(
            "INSERT INTO {} ({}) VALUES ({})",
            quote_ident(schema.name()),
            quoted_columns,
            placeholders
        ),
        params_from_iter(values),
    )?;
    Ok(())
}

fn apply_property_updates(
    tx: &Transaction<'_>,
    schema: &TableSchema,
    file: &str,
    updates: &[(&Column, JsonValue)],
    invalid: &mut JsonMap<String, JsonValue>,
) -> Result<()> {
    match updates {
        [] => Ok(()),
        [(column, value)] => match try_update_field(tx, schema, column, file, value) {
            Ok(()) => {
                invalid.remove(column.name());
                Ok(())
            }
            Err(err) if is_semantic_sql_error(&err) => {
                invalid.insert(
                    column.name().to_string(),
                    invalid_entry(value.clone(), "constraint failed"),
                );
                Ok(())
            }
            Err(err) => Err(err),
        },
        updates => match try_update_fields(tx, schema, file, updates) {
            Ok(()) => {
                remove_invalid_entries(invalid, updates);
                Ok(())
            }
            Err(err) if is_semantic_sql_error(&err) => {
                mark_constraint_failed(invalid, updates);
                Ok(())
            }
            Err(err) => Err(err),
        },
    }
}

fn remove_invalid_entries(
    invalid: &mut JsonMap<String, JsonValue>,
    updates: &[(&Column, JsonValue)],
) {
    for (column, _) in updates {
        invalid.remove(column.name());
    }
}

fn mark_constraint_failed(
    invalid: &mut JsonMap<String, JsonValue>,
    updates: &[(&Column, JsonValue)],
) {
    for (column, value) in updates {
        invalid.insert(
            column.name().to_string(),
            invalid_entry(value.clone(), "constraint failed"),
        );
    }
}

fn run_in_savepoint<T>(
    tx: &Transaction<'_>,
    name: &str,
    op: impl FnOnce() -> Result<T>,
) -> Result<T> {
    tx.execute_batch(&format!("SAVEPOINT {name}"))?;
    match op() {
        Ok(value) => {
            tx.execute_batch(&format!("RELEASE {name}"))?;
            Ok(value)
        }
        Err(err) => {
            tx.execute_batch(&format!("ROLLBACK TO {name}"))?;
            tx.execute_batch(&format!("RELEASE {name}"))?;
            Err(err)
        }
    }
}

fn try_update_fields(
    tx: &Transaction<'_>,
    schema: &TableSchema,
    file: &str,
    updates: &[(&Column, JsonValue)],
) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }

    let assignments = updates
        .iter()
        .enumerate()
        .map(|(index, (column, _))| format!("{} = ?{}", quote_ident(column.name()), index + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let where_param = updates.len() + 1;
    let mut values = updates
        .iter()
        .map(|(_, value)| json_to_sql(value))
        .collect::<Vec<_>>();
    values.push(SqlValue::Text(file.to_string()));

    run_in_savepoint(tx, "slfs_apply_fields", || {
        tx.execute(
            &format!(
                "UPDATE {} SET {} WHERE {} = ?{}",
                quote_ident(schema.name()),
                assignments,
                quote_ident("_slfs_path"),
                where_param,
            ),
            params_from_iter(values),
        )
        .map(|_| ())
        .map_err(Into::into)
    })
}

fn try_update_field(
    tx: &Transaction<'_>,
    schema: &TableSchema,
    column: &Column,
    file: &str,
    value: &JsonValue,
) -> Result<()> {
    run_in_savepoint(tx, "slfs_apply_field", || {
        tx.execute(
            &format!(
                "UPDATE {} SET {} = ?1 WHERE {} = ?2",
                quote_ident(schema.name()),
                quote_ident(column.name()),
                quote_ident("_slfs_path")
            ),
            params![json_to_sql(value), file],
        )
        .map(|_| ())
        .map_err(Into::into)
    })
}

fn load_invalid_update(
    tx: &Transaction<'_>,
    schema: &TableSchema,
    file: &str,
) -> Result<JsonMap<String, JsonValue>> {
    let text: Option<String> = tx
        .query_row(
            &format!(
                "SELECT {} FROM {} WHERE {} = ?1",
                quote_ident("_slfs_invalid_update"),
                quote_ident(schema.name()),
                quote_ident("_slfs_path")
            ),
            params![file],
            |row| row.get(0),
        )
        .optional()?;
    Ok(text
        .and_then(|text| serde_json::from_str::<JsonValue>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default())
}

fn row_exists(tx: &Transaction<'_>, schema: &TableSchema, file: &str) -> Result<bool> {
    let exists: Option<i64> = tx
        .query_row(
            &format!(
                "SELECT 1 FROM {} WHERE {} = ?1",
                quote_ident(schema.name()),
                quote_ident("_slfs_path")
            ),
            params![file],
            |row| row.get(0),
        )
        .optional()?;
    Ok(exists.is_some())
}

fn document_exists(conn: &Connection, doc: &DocPath) -> Result<bool> {
    let schema = table_schema(conn, doc.table())?;
    let exists: Option<i64> = conn
        .query_row(
            &format!(
                "SELECT 1 FROM {} WHERE {} = ?1",
                quote_ident(schema.name()),
                quote_ident("_slfs_path")
            ),
            params![doc.file()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(exists.is_some())
}

fn read_slice(data: &[u8], offset: u64, size: usize) -> Result<Vec<u8>> {
    let start = usize::try_from(offset).map_err(|_| FsError::InvalidInput)?;
    if start >= data.len() {
        return Ok(Vec::new());
    }
    let end = start.saturating_add(size).min(data.len());
    Ok(data[start..end].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_fs(schema: &str) -> SqliteFs {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute_batch(schema).unwrap();
        let backing = std::env::temp_dir().join(format!("sqlite-fs-test-{}", short_suffix()));
        fs::create_dir_all(&backing).unwrap();
        SqliteFs {
            backing,
            inner: Mutex::new(Inner {
                conn,
                handles: HashMap::new(),
                next_handle: 1,
                file_times: HashMap::new(),
                mount_time: SystemTime::now(),
            }),
        }
    }

    fn create_doc(fs: &SqliteFs, path: &str, text: &str) {
        let handle = fs.create(path, 0o644, OpenOptions::from_raw(1)).unwrap();
        fs.write(path, handle, 0, text.as_bytes()).unwrap();
        fs.release(path, handle).unwrap();
    }

    fn overwrite_doc(fs: &SqliteFs, path: &str, text: &str) {
        let handle = fs.open(path, OpenOptions::from_raw(1)).unwrap();
        fs.truncate(path, Some(handle), 0).unwrap();
        fs.write(path, handle, 0, text.as_bytes()).unwrap();
        fs.release(path, handle).unwrap();
    }

    fn contact_schema() -> &'static str {
        "CREATE TABLE contacts (
            _slfs_path TEXT UNIQUE NOT NULL,
            _slfs_content TEXT NOT NULL DEFAULT '',
            _slfs_invalid_update TEXT NOT NULL DEFAULT '{}',
            first_name TEXT NOT NULL DEFAULT 'untitled' CHECK(length(first_name) >= 1),
            email TEXT UNIQUE,
            age INTEGER NOT NULL DEFAULT 0 CHECK(age >= 0)
        );"
    }

    fn docs_schema() -> &'static str {
        "CREATE TABLE docs (
            _slfs_path TEXT UNIQUE NOT NULL,
            _slfs_content TEXT NOT NULL DEFAULT '',
            _slfs_invalid_update TEXT NOT NULL DEFAULT '{}'
        );"
    }

    const O_WRONLY_RAW: i32 = 1;

    #[cfg(target_os = "macos")]
    const O_APPEND_RAW: i32 = 0x0008;
    #[cfg(not(target_os = "macos"))]
    const O_APPEND_RAW: i32 = 0x0400;

    #[test]
    fn routes_typed_files_and_passthrough_paths() {
        let fs = test_fs(contact_schema());
        let inner = fs.inner.lock().unwrap();
        assert!(matches!(
            route(&inner.conn, &fs.backing, "/contacts"),
            Ok(Route::TableDir(_))
        ));
        assert!(matches!(
            route(&inner.conn, &fs.backing, "/contacts/noorvir.md"),
            Ok(Route::Typed(_))
        ));
        assert!(matches!(
            route(&inner.conn, &fs.backing, "/contacts/noorvir.txt"),
            Ok(Route::Pass(_))
        ));
        assert!(matches!(
            route(&inner.conn, &fs.backing, "/contacts/friends/noorvir.md"),
            Ok(Route::Pass(_))
        ));
        assert!(matches!(
            route(&inner.conn, &fs.backing, "/.obsidian/app.json"),
            Ok(Route::Pass(_))
        ));
    }

    #[test]
    fn parses_frontmatter_and_body() {
        let (props, body) = parse_markdown("---\nname: Ada\nage: 37\n---\nBody\n").unwrap();
        assert_eq!(props.get("name"), Some(&JsonValue::from("Ada")));
        assert_eq!(props.get("age"), Some(&JsonValue::from(37)));
        assert_eq!(body, "Body\n");
    }

    #[test]
    fn parses_yaml_scalar_edge_cases() {
        let (props, _) = parse_markdown(
            "---\nempty: \"\"\nbare_null:\nnumber: 123\nquoted_number: \"123\"\n---\n",
        )
        .unwrap();
        assert_eq!(props.get("empty"), Some(&JsonValue::from("")));
        assert_eq!(props.get("bare_null"), Some(&JsonValue::Null));
        assert_eq!(props.get("number"), Some(&JsonValue::from(123)));
        assert_eq!(props.get("quoted_number"), Some(&JsonValue::from("123")));
    }

    #[test]
    fn generic_sqlite_write_records_invalid_updates() {
        let fs = test_fs(contact_schema());
        create_doc(
            &fs,
            "/contacts/noorvir.md",
            "---\nfirst_name: \"\"\nemail: noorvir@example.com\nunknown: value\n---\nBody\n",
        );

        let inner = fs.inner.lock().unwrap();
        let (first_name, email, content, invalid): (String, Option<String>, String, String) = inner
            .conn
            .query_row(
                "SELECT first_name, email, _slfs_content, _slfs_invalid_update FROM contacts WHERE _slfs_path = 'noorvir.md'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        let invalid: JsonValue = serde_json::from_str(&invalid).unwrap();
        assert_eq!(first_name, "untitled");
        assert_eq!(email, None);
        assert_eq!(content, "Body\n");
        assert_eq!(invalid["first_name"]["attempted"], JsonValue::from(""));
        assert_eq!(
            invalid["email"]["attempted"],
            JsonValue::from("noorvir@example.com")
        );
        assert_eq!(invalid["unknown"]["attempted"], JsonValue::from("value"));
    }

    #[test]
    fn text_columns_reject_unquoted_yaml_numbers() {
        let fs = test_fs(contact_schema());
        create_doc(
            &fs,
            "/contacts/noorvir.md",
            "---\nfirst_name: Noorvir\n---\nBody\n",
        );
        overwrite_doc(
            &fs,
            "/contacts/noorvir.md",
            "---\nfirst_name: 123\n---\nBody\n",
        );

        let inner = fs.inner.lock().unwrap();
        let (first_name, invalid): (String, String) = inner
            .conn
            .query_row(
                "SELECT first_name, _slfs_invalid_update FROM contacts WHERE _slfs_path = 'noorvir.md'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let invalid: JsonValue = serde_json::from_str(&invalid).unwrap();
        assert_eq!(first_name, "Noorvir");
        assert_eq!(invalid["first_name"]["attempted"], JsonValue::from(123));
        assert_eq!(
            invalid["first_name"]["error"],
            JsonValue::from("must be text")
        );
    }

    #[test]
    fn quoted_yaml_numbers_are_valid_text() {
        let fs = test_fs(contact_schema());
        create_doc(
            &fs,
            "/contacts/noorvir.md",
            "---\nfirst_name: \"123\"\n---\nBody\n",
        );

        let inner = fs.inner.lock().unwrap();
        let (first_name, invalid): (String, String) = inner
            .conn
            .query_row(
                "SELECT first_name, _slfs_invalid_update FROM contacts WHERE _slfs_path = 'noorvir.md'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let invalid: JsonValue = serde_json::from_str(&invalid).unwrap();
        assert_eq!(first_name, "123");
        assert!(invalid.get("first_name").is_none());
    }

    #[test]
    fn bare_null_and_empty_string_are_distinct_invalid_values() {
        let fs = test_fs(contact_schema());
        create_doc(
            &fs,
            "/contacts/noorvir.md",
            "---\nfirst_name: Noorvir\n---\nBody\n",
        );
        overwrite_doc(
            &fs,
            "/contacts/noorvir.md",
            "---\nfirst_name: \"\"\n---\nBody\n",
        );
        let empty_invalid: JsonValue = {
            let inner = fs.inner.lock().unwrap();
            serde_json::from_str(
                &inner
                    .conn
                    .query_row(
                        "SELECT _slfs_invalid_update FROM contacts WHERE _slfs_path = 'noorvir.md'",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .unwrap(),
            )
            .unwrap()
        };
        assert_eq!(
            empty_invalid["first_name"]["attempted"],
            JsonValue::from("")
        );
        assert_eq!(
            empty_invalid["first_name"]["error"],
            JsonValue::from("constraint failed")
        );

        overwrite_doc(&fs, "/contacts/noorvir.md", "---\nfirst_name:\n---\nBody\n");
        let null_invalid: JsonValue = {
            let inner = fs.inner.lock().unwrap();
            serde_json::from_str(
                &inner
                    .conn
                    .query_row(
                        "SELECT _slfs_invalid_update FROM contacts WHERE _slfs_path = 'noorvir.md'",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .unwrap(),
            )
            .unwrap()
        };
        assert_eq!(null_invalid["first_name"]["attempted"], JsonValue::Null);
        assert_eq!(
            null_invalid["first_name"]["error"],
            JsonValue::from("cannot be null")
        );
    }

    #[test]
    fn nullable_text_columns_accept_bare_null() {
        let fs = test_fs(contact_schema());
        create_doc(
            &fs,
            "/contacts/noorvir.md",
            "---\nfirst_name: Noorvir\nemail: noorvir@example.com\n---\nBody\n",
        );
        overwrite_doc(&fs, "/contacts/noorvir.md", "---\nemail:\n---\nBody\n");

        let inner = fs.inner.lock().unwrap();
        let (email, invalid): (Option<String>, String) = inner
            .conn
            .query_row(
                "SELECT email, _slfs_invalid_update FROM contacts WHERE _slfs_path = 'noorvir.md'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let invalid: JsonValue = serde_json::from_str(&invalid).unwrap();
        assert_eq!(email, None);
        assert!(invalid.get("email").is_none());
    }

    #[test]
    fn integer_columns_reject_quoted_yaml_numbers() {
        let fs = test_fs(contact_schema());
        create_doc(
            &fs,
            "/contacts/noorvir.md",
            "---\nfirst_name: Noorvir\nage: 37\n---\nBody\n",
        );
        overwrite_doc(&fs, "/contacts/noorvir.md", "---\nage: \"38\"\n---\nBody\n");

        let inner = fs.inner.lock().unwrap();
        let (age, invalid): (i64, String) = inner
            .conn
            .query_row(
                "SELECT age, _slfs_invalid_update FROM contacts WHERE _slfs_path = 'noorvir.md'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let invalid: JsonValue = serde_json::from_str(&invalid).unwrap();
        assert_eq!(age, 37);
        assert_eq!(invalid["age"]["attempted"], JsonValue::from("38"));
        assert_eq!(invalid["age"]["error"], JsonValue::from("must be integer"));
    }

    #[test]
    fn typed_create_is_handle_local_until_commit() {
        let fs = test_fs(docs_schema());
        let handle = fs
            .create("/docs/draft.md", 0o644, OpenOptions::from_raw(O_WRONLY_RAW))
            .unwrap();

        let entries = fs
            .readdir("/docs")
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        assert!(!entries.contains(&"draft.md".to_string()));

        fs.write("/docs/draft.md", handle, 0, b"draft").unwrap();
        assert!(matches!(
            fs.getattr("/docs/draft.md"),
            Err(FsError::NotFound)
        ));
        assert!(matches!(
            fs.read("/docs/draft.md", NO_HANDLE, 0, 100),
            Err(FsError::NotFound)
        ));

        fs.release("/docs/draft.md", handle).unwrap();
        assert_eq!(
            fs.read("/docs/draft.md", NO_HANDLE, 0, 100).unwrap(),
            b"draft"
        );
    }

    #[test]
    fn typed_append_honors_open_option() {
        let fs = test_fs(docs_schema());
        create_doc(&fs, "/docs/note.md", "one");

        let handle = fs
            .open(
                "/docs/note.md",
                OpenOptions::from_raw(O_WRONLY_RAW | O_APPEND_RAW),
            )
            .unwrap();
        fs.write("/docs/note.md", handle, 0, b"two").unwrap();
        fs.release("/docs/note.md", handle).unwrap();

        assert_eq!(
            fs.read("/docs/note.md", NO_HANDLE, 0, 100).unwrap(),
            b"onetwo"
        );
    }

    #[test]
    fn typed_rename_missing_source_does_not_delete_destination() {
        let fs = test_fs(docs_schema());
        create_doc(&fs, "/docs/existing.md", "keep");

        assert!(matches!(
            fs.rename(
                "/docs/missing.md",
                "/docs/existing.md",
                RenameFlags::from_raw(0),
            ),
            Err(FsError::NotFound)
        ));
        assert_eq!(
            fs.read("/docs/existing.md", NO_HANDLE, 0, 100).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn passthrough_temp_file_can_replace_typed_document() {
        let fs = test_fs(docs_schema());
        create_doc(&fs, "/docs/note.md", "old");

        let handle = fs
            .create(
                "/docs/.note.md.tmp",
                0o644,
                OpenOptions::from_raw(O_WRONLY_RAW),
            )
            .unwrap();
        fs.write("/docs/.note.md.tmp", handle, 0, b"new").unwrap();
        fs.release("/docs/.note.md.tmp", handle).unwrap();

        fs.rename(
            "/docs/.note.md.tmp",
            "/docs/note.md",
            RenameFlags::from_raw(0),
        )
        .unwrap();

        assert_eq!(fs.read("/docs/note.md", NO_HANDLE, 0, 100).unwrap(), b"new");
        assert!(matches!(
            fs.getattr("/docs/.note.md.tmp"),
            Err(FsError::NotFound)
        ));
    }

    #[test]
    fn pass_to_typed_rename_hands_open_source_handle_to_typed_document() {
        let fs = test_fs(docs_schema());
        create_doc(&fs, "/docs/note.md", "old");
        fs.rename(
            "/docs/note.md",
            "/docs/note.md.sb-test",
            RenameFlags::from_raw(0),
        )
        .unwrap();

        let handle = fs
            .create(
                "/docs/.note.md.tmp",
                0o644,
                OpenOptions::from_raw(O_WRONLY_RAW),
            )
            .unwrap();
        fs.write("/docs/.note.md.tmp", handle, 0, b"first").unwrap();

        fs.rename(
            "/docs/.note.md.tmp",
            "/docs/note.md",
            RenameFlags::from_raw(0),
        )
        .unwrap();

        fs.write("/docs/note.md", handle, 5, b"second").unwrap();
        fs.release("/docs/note.md", handle).unwrap();
        assert!(matches!(
            fs.getattr("/docs/.note.md.tmp"),
            Err(FsError::NotFound)
        ));
        assert_eq!(
            fs.read("/docs/note.md", NO_HANDLE, 0, 100).unwrap(),
            b"firstsecond"
        );
    }

    #[test]
    fn typed_rename_and_unlink_reject_open_handles() {
        let fs = test_fs(docs_schema());
        create_doc(&fs, "/docs/a.md", "a");
        create_doc(&fs, "/docs/b.md", "b");

        let handle = fs
            .open("/docs/a.md", OpenOptions::from_raw(O_WRONLY_RAW))
            .unwrap();
        fs.rename("/docs/a.md", "/docs/a.md", RenameFlags::from_raw(0))
            .unwrap();
        assert!(matches!(
            fs.unlink("/docs/a.md"),
            Err(FsError::PermissionDenied)
        ));
        assert!(matches!(
            fs.rename("/docs/a.md", "/docs/c.md", RenameFlags::from_raw(0)),
            Err(FsError::PermissionDenied)
        ));
        assert!(matches!(
            fs.rename("/docs/b.md", "/docs/a.md", RenameFlags::from_raw(0)),
            Err(FsError::PermissionDenied)
        ));
        fs.release("/docs/a.md", handle).unwrap();
    }

    #[test]
    fn truncate_through_read_only_typed_handle_fails() {
        let fs = test_fs(docs_schema());
        create_doc(&fs, "/docs/note.md", "body");

        let handle = fs.open("/docs/note.md", OpenOptions::from_raw(0)).unwrap();
        assert!(matches!(
            fs.truncate("/docs/note.md", Some(handle), 0),
            Err(FsError::BadFileDescriptor)
        ));
        fs.release("/docs/note.md", handle).unwrap();
    }

    #[test]
    fn typed_documents_preserve_trailing_nuls() {
        let fs = test_fs(docs_schema());
        let handle = fs
            .create("/docs/nuls.md", 0o644, OpenOptions::from_raw(O_WRONLY_RAW))
            .unwrap();
        fs.write("/docs/nuls.md", handle, 0, b"body\0\0").unwrap();
        fs.release("/docs/nuls.md", handle).unwrap();

        assert_eq!(
            fs.read("/docs/nuls.md", NO_HANDLE, 0, 100).unwrap(),
            b"body\0\0"
        );
    }

    #[test]
    fn tables_without_required_invariants_are_not_typed() {
        let fs = test_fs(
            "CREATE TABLE bad_docs (
                _slfs_path TEXT,
                _slfs_content TEXT,
                _slfs_invalid_update TEXT
            );",
        );
        let inner = fs.inner.lock().unwrap();

        assert!(matches!(
            route(&inner.conn, &fs.backing, "/bad_docs"),
            Ok(Route::Pass(_))
        ));
        assert!(eligible_tables(&inner.conn).unwrap().is_empty());
    }

    #[test]
    fn grouped_constraint_failure_is_recorded_conservatively() {
        let fs = test_fs(
            "CREATE TABLE events (
                _slfs_path TEXT UNIQUE NOT NULL,
                _slfs_content TEXT NOT NULL,
                _slfs_invalid_update TEXT NOT NULL,
                start INTEGER NOT NULL DEFAULT 1,
                end INTEGER NOT NULL DEFAULT 3,
                email TEXT UNIQUE,
                CHECK(start < end)
            );",
        );
        create_doc(
            &fs,
            "/events/other.md",
            "---\nstart: 10\nend: 11\nemail: dup@example.com\n---\nOther\n",
        );
        create_doc(
            &fs,
            "/events/main.md",
            "---\nstart: 1\nend: 3\nemail: old@example.com\n---\nOld\n",
        );

        overwrite_doc(
            &fs,
            "/events/main.md",
            "---\nstart: 4\nend: 5\nemail: dup@example.com\n---\nNew\n",
        );

        let inner = fs.inner.lock().unwrap();
        let (start, end, email, content, invalid): (i64, i64, String, String, String) = inner
            .conn
            .query_row(
                "SELECT start, end, email, _slfs_content, _slfs_invalid_update FROM events WHERE _slfs_path = 'main.md'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();
        let invalid: JsonValue = serde_json::from_str(&invalid).unwrap();
        assert_eq!(
            (start, end, email.as_str(), content.as_str()),
            (1, 3, "old@example.com", "New\n")
        );
        assert_eq!(invalid["start"]["attempted"], JsonValue::from(4));
        assert_eq!(invalid["end"]["attempted"], JsonValue::from(5));
        assert_eq!(
            invalid["email"]["attempted"],
            JsonValue::from("dup@example.com")
        );
    }

    #[test]
    fn new_row_insert_uses_supplied_properties_before_generic_defaults() {
        let fs = test_fs(
            "CREATE TABLE parents (id INTEGER PRIMARY KEY);
            INSERT INTO parents(id) VALUES (42);
            CREATE TABLE children (
                _slfs_path TEXT UNIQUE NOT NULL,
                _slfs_content TEXT NOT NULL,
                _slfs_invalid_update TEXT NOT NULL,
                parent_id INTEGER NOT NULL REFERENCES parents(id)
            );",
        );

        let handle = fs
            .create(
                "/children/kid.md",
                0o644,
                OpenOptions::from_raw(O_WRONLY_RAW),
            )
            .unwrap();
        fs.write(
            "/children/kid.md",
            handle,
            0,
            b"---\nparent_id: 42\n---\nKid\n",
        )
        .unwrap();
        fs.release("/children/kid.md", handle).unwrap();

        let inner = fs.inner.lock().unwrap();
        let (parent_id, content): (i64, String) = inner
            .conn
            .query_row(
                "SELECT parent_id, _slfs_content FROM children WHERE _slfs_path = 'kid.md'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(parent_id, 42);
        assert_eq!(content, "Kid\n");
    }

    #[test]
    fn typed_table_defaults_are_not_required() {
        let fs = test_fs(
            "CREATE TABLE docs (
                _slfs_path TEXT UNIQUE NOT NULL,
                _slfs_content TEXT NOT NULL,
                _slfs_invalid_update TEXT NOT NULL
            );",
        );

        create_doc(&fs, "/docs/no_defaults.md", "body");
        assert_eq!(
            fs.read("/docs/no_defaults.md", NO_HANDLE, 0, 100).unwrap(),
            b"body"
        );
    }

    #[test]
    fn partial_unique_path_index_is_not_enough_for_typed_table() {
        let fs = test_fs(
            "CREATE TABLE partial_docs (
                _slfs_path TEXT NOT NULL,
                _slfs_content TEXT NOT NULL,
                _slfs_invalid_update TEXT NOT NULL
            );
            CREATE UNIQUE INDEX partial_docs_path
                ON partial_docs(_slfs_path)
                WHERE _slfs_content <> '';",
        );
        let inner = fs.inner.lock().unwrap();

        assert!(matches!(
            route(&inner.conn, &fs.backing, "/partial_docs"),
            Ok(Route::Pass(_))
        ));
        assert!(eligible_tables(&inner.conn).unwrap().is_empty());
    }

    #[test]
    fn passthrough_supports_hidden_app_metadata() {
        let fs = test_fs(contact_schema());
        fs.mkdir("/.obsidian", 0o755).unwrap();
        create_doc(&fs, "/.obsidian/app.json", "{}\n");

        assert_eq!(
            fs.read("/.obsidian/app.json", NO_HANDLE, 0, 100).unwrap(),
            b"{}\n"
        );
        assert!(fs.backing.join(".obsidian/app.json").exists());
    }

    #[test]
    fn passthrough_works_inside_table_folders_for_non_typed_paths() {
        let fs = test_fs(contact_schema());
        create_doc(
            &fs,
            "/contacts/noorvir.md",
            "---\nfirst_name: Noorvir\n---\nBody\n",
        );
        fs.mkdir("/contacts/nested", 0o755).unwrap();
        create_doc(&fs, "/contacts/nested/pass.md", "pass\n");
        create_doc(&fs, "/contacts/ignored.txt", "x");

        let entries = fs
            .readdir("/contacts")
            .unwrap()
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        assert!(entries.contains(&"noorvir.md".to_string()));
        assert!(entries.contains(&"nested".to_string()));
        assert!(entries.contains(&"ignored.txt".to_string()));

        let inner = fs.inner.lock().unwrap();
        let row_count: i64 = inner
            .conn
            .query_row("SELECT COUNT(*) FROM contacts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(row_count, 1);
    }
}
