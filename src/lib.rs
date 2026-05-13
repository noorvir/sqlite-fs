use minfuse::{
    Attr, DirEntry, EntryKind, FileHandle, FileSystem, FsError, FsResult, NO_HANDLE, OpenOptions,
    RenameFlags, SetTimes, StatFs,
};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Transaction, params, params_from_iter};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions as StdOpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

static PLACEHOLDER_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct SqliteFs {
    backing: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    conn: Connection,
    handles: HashMap<FileHandle, OpenFile>,
    next_handle: FileHandle,
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
    file: File,
    writable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DocPath {
    table: String,
    file: String,
}

#[derive(Debug, Clone)]
struct TableSchema {
    name: String,
    columns: Vec<Column>,
}

#[derive(Debug, Clone)]
struct Column {
    name: String,
    decl_type: String,
    not_null: bool,
    default_value: Option<String>,
    pk: bool,
}

#[derive(Debug)]
pub enum Error {
    Fs(FsError),
    Sql(rusqlite::Error),
    Json(serde_json::Error),
    Yaml(serde_yaml::Error),
    Utf8(std::str::Utf8Error),
    Io(std::io::Error),
}

type Result<T> = std::result::Result<T, Error>;

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

    fn rename(&self, from: &str, to: &str, _flags: RenameFlags) -> FsResult<()> {
        self.try_rename(from, to).map_err(to_fs_error)
    }

    fn chmod(&self, path: &str, handle: Option<FileHandle>, _mode: u32) -> FsResult<()> {
        self.try_metadata_noop(path, handle).map_err(to_fs_error)
    }

    fn chown(&self, path: &str, handle: Option<FileHandle>, _uid: u32, _gid: u32) -> FsResult<()> {
        self.try_metadata_noop(path, handle).map_err(to_fs_error)
    }

    fn utimens(&self, path: &str, handle: Option<FileHandle>, _times: SetTimes) -> FsResult<()> {
        self.try_metadata_noop(path, handle).map_err(to_fs_error)
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
        Ok(Attr::file(rendered.len() as u64))
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
                commit_document(&mut inner.conn, &doc, &staged)
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
                commit_document(&mut inner.conn, &doc, b"")
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
        let inner = self.inner.lock().unwrap();
        match route(&inner.conn, &self.backing, path)? {
            Route::Typed(doc) => {
                if has_open_typed_handle(&inner, &doc) {
                    return Err(FsError::PermissionDenied.into());
                }
                delete_typed(&inner.conn, &doc)
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

    fn try_rename(&self, from: &str, to: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        match (
            route(&inner.conn, &self.backing, from)?,
            route(&inner.conn, &self.backing, to)?,
        ) {
            (Route::Typed(from), Route::Typed(to)) => rename_typed(&mut inner, from, to),
            (Route::Pass(from_path), Route::Pass(to_path)) => {
                ensure_virtual_parent(&inner.conn, &self.backing, path_to_str(&to_path)?)?;
                fs::rename(from_path, to_path)?;
                Ok(())
            }
            _ => Err(FsError::Unsupported.into()),
        }
    }
}

fn default_backing_path(db: &Path) -> PathBuf {
    PathBuf::from(format!("{}.files", db.display()))
}

#[derive(Debug)]
enum Route {
    Root,
    TableDir(String),
    Typed(DocPath),
    Pass(PathBuf),
}

fn route(conn: &Connection, backing: &Path, path: &str) -> Result<Route> {
    if path == "/" {
        return Ok(Route::Root);
    }
    let parts = path_components(path)?;
    let typed_table = match parts.as_slice() {
        [table] => table_schema(conn, table).is_ok(),
        [table, file] if is_typed_file_name(file) => table_schema(conn, table).is_ok(),
        _ => false,
    };
    if parts.len() == 1 && typed_table {
        return Ok(Route::TableDir(parts[0].clone()));
    }
    if parts.len() == 2 && typed_table {
        return Ok(Route::Typed(DocPath {
            table: parts[0].clone(),
            file: parts[1].clone(),
        }));
    }
    Ok(Route::Pass(backing.join(parts.iter().collect::<PathBuf>())))
}

fn path_components(path: &str) -> Result<Vec<String>> {
    let trimmed = path.strip_prefix('/').ok_or(FsError::InvalidInput)?;
    if trimmed.is_empty() || trimmed.contains('\0') {
        return Err(FsError::InvalidInput.into());
    }
    let mut parts = Vec::new();
    for part in trimmed.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(FsError::InvalidInput.into());
        }
        parts.push(part.to_string());
    }
    Ok(parts)
}

fn is_typed_file_name(name: &str) -> bool {
    name.ends_with(".md")
        && !name.starts_with("._")
        && name != ".DS_Store"
        && !name.contains('/')
        && !name.contains('\0')
        && name.len() <= 255
}

fn path_to_str(path: &Path) -> Result<&str> {
    path.to_str().ok_or(FsError::InvalidInput.into())
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
        quote_ident(&schema.name),
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

fn delete_typed(conn: &Connection, doc: &DocPath) -> Result<()> {
    let schema = table_schema(conn, &doc.table)?;
    let changed = conn.execute(
        &format!(
            "DELETE FROM {} WHERE {} = ?1",
            quote_ident(&schema.name),
            quote_ident("_slfs_path")
        ),
        params![doc.file],
    )?;
    if changed == 0 {
        Err(FsError::NotFound.into())
    } else {
        Ok(())
    }
}

fn rename_typed(inner: &mut Inner, from: DocPath, to: DocPath) -> Result<()> {
    if from.table != to.table {
        return Err(FsError::Unsupported.into());
    }
    if from.file == to.file {
        return if document_exists(&inner.conn, &from)? {
            Ok(())
        } else {
            Err(FsError::NotFound.into())
        };
    }
    if has_open_typed_handle(inner, &from) || has_open_typed_handle(inner, &to) {
        return Err(FsError::PermissionDenied.into());
    }

    let schema = table_schema(&inner.conn, &from.table)?;
    let tx = inner.conn.transaction()?;
    if !row_exists(&tx, &schema, &from.file)? {
        return Err(FsError::NotFound.into());
    }
    tx.execute(
        &format!(
            "DELETE FROM {} WHERE {} = ?1",
            quote_ident(&schema.name),
            quote_ident("_slfs_path")
        ),
        params![to.file],
    )?;
    tx.execute(
        &format!(
            "UPDATE {} SET {} = ?1 WHERE {} = ?2",
            quote_ident(&schema.name),
            quote_ident("_slfs_path"),
            quote_ident("_slfs_path")
        ),
        params![to.file, from.file],
    )?;
    tx.commit()?;
    Ok(())
}

fn has_open_typed_handle(inner: &Inner, doc: &DocPath) -> bool {
    inner.handles.values().any(|open| match open {
        OpenFile::Typed(open) => &open.path == doc,
        OpenFile::Pass(_) => false,
    })
}

fn commit_document(conn: &mut Connection, doc: &DocPath, bytes: &[u8]) -> Result<()> {
    let text = std::str::from_utf8(bytes)?;
    let (props, body) = parse_markdown(text)?;
    let schema = table_schema(conn, &doc.table)?;
    let tx = conn.transaction()?;

    let existed = row_exists(&tx, &schema, &doc.file)?;
    let mut invalid = if existed {
        let mut invalid = load_invalid_update(&tx, &schema, &doc.file)?;
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
                quote_ident(&schema.name),
                quote_ident("_slfs_content"),
                quote_ident("_slfs_path")
            ),
            params![body, doc.file],
        )?;
        apply_property_updates(&tx, &schema, &doc.file, &updates, &mut invalid)?;
    } else {
        let consumed = insert_document_row(&tx, &schema, &doc.file, &body, &updates, &mut invalid)?;
        let remaining = updates
            .iter()
            .filter(|(column, _)| !consumed.contains(&column.name))
            .map(|(column, value)| (*column, value.clone()))
            .collect::<Vec<_>>();
        apply_property_updates(&tx, &schema, &doc.file, &remaining, &mut invalid)?;
    }

    let invalid_json = serde_json::to_string(&invalid)?;
    tx.execute(
        &format!(
            "UPDATE {} SET {} = ?1 WHERE {} = ?2",
            quote_ident(&schema.name),
            quote_ident("_slfs_invalid_update"),
            quote_ident("_slfs_path")
        ),
        params![invalid_json, doc.file],
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
        .map(|(column, _)| column.name.clone())
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
        columns.push(column.name.clone());
        values.push(json_to_sql(value));
    }

    for column in schema.domain_columns() {
        let already_supplied = supplied_required
            .iter()
            .any(|(supplied, _)| supplied.name == column.name);
        if !already_supplied && column.needs_insert_value() {
            columns.push(column.name.clone());
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
            quote_ident(&schema.name),
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
                invalid.remove(&column.name);
                Ok(())
            }
            Err(err) if is_semantic_sql_error(&err) => {
                invalid.insert(
                    column.name.clone(),
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
        invalid.remove(&column.name);
    }
}

fn mark_constraint_failed(
    invalid: &mut JsonMap<String, JsonValue>,
    updates: &[(&Column, JsonValue)],
) {
    for (column, value) in updates {
        invalid.insert(
            column.name.clone(),
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
        .map(|(index, (column, _))| format!("{} = ?{}", quote_ident(&column.name), index + 1))
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
                quote_ident(&schema.name),
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
                quote_ident(&schema.name),
                quote_ident(&column.name),
                quote_ident("_slfs_path")
            ),
            params![json_to_sql(value), file],
        )
        .map(|_| ())
        .map_err(Into::into)
    })
}

fn render_document(conn: &Connection, doc: &DocPath) -> Result<String> {
    let schema = table_schema(conn, &doc.table)?;
    let row = load_row(conn, &schema, &doc.file)?.ok_or(FsError::NotFound)?;
    let mut props = JsonMap::new();
    for column in schema.domain_columns() {
        if let Some(value) = row.values.get(&column.name) {
            props.insert(column.name.clone(), sql_to_json(value));
        }
    }
    for (key, value) in row.invalid {
        if let Some(attempted) = value.get("attempted") {
            props.insert(key, attempted.clone());
        }
    }

    let mut out = String::new();
    if !props.is_empty() {
        out.push_str("---\n");
        out.push_str(&serde_yaml::to_string(&props)?);
        out.push_str("---\n");
    }
    out.push_str(&row.content);
    Ok(out)
}

struct LoadedRow {
    values: HashMap<String, SqlValue>,
    invalid: JsonMap<String, JsonValue>,
    content: String,
}

fn load_row(conn: &Connection, schema: &TableSchema, file: &str) -> Result<Option<LoadedRow>> {
    let selected = schema
        .columns
        .iter()
        .map(|column| quote_ident(&column.name))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT {} FROM {} WHERE {} = ?1",
        selected,
        quote_ident(&schema.name),
        quote_ident("_slfs_path")
    );
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_row(params![file], |row| {
        let mut values = HashMap::new();
        for (index, column) in schema.columns.iter().enumerate() {
            values.insert(column.name.clone(), row.get::<_, SqlValue>(index)?);
        }
        let content = match values.get("_slfs_content") {
            Some(SqlValue::Text(text)) => text.clone(),
            _ => String::new(),
        };
        let invalid = match values.get("_slfs_invalid_update") {
            Some(SqlValue::Text(text)) => serde_json::from_str::<JsonValue>(text)
                .ok()
                .and_then(|value| value.as_object().cloned())
                .unwrap_or_default(),
            _ => JsonMap::new(),
        };
        Ok(LoadedRow {
            values,
            invalid,
            content,
        })
    })
    .optional()
    .map_err(Into::into)
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
                quote_ident(&schema.name),
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
                quote_ident(&schema.name),
                quote_ident("_slfs_path")
            ),
            params![file],
            |row| row.get(0),
        )
        .optional()?;
    Ok(exists.is_some())
}

fn document_exists(conn: &Connection, doc: &DocPath) -> Result<bool> {
    let schema = table_schema(conn, &doc.table)?;
    let exists: Option<i64> = conn
        .query_row(
            &format!(
                "SELECT 1 FROM {} WHERE {} = ?1",
                quote_ident(&schema.name),
                quote_ident("_slfs_path")
            ),
            params![doc.file],
            |row| row.get(0),
        )
        .optional()?;
    Ok(exists.is_some())
}

fn eligible_tables(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_schema \
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
         ORDER BY name",
    )?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(names
        .into_iter()
        .filter_map(|name| table_schema(conn, &name).ok().map(|_| name))
        .collect())
}

fn table_schema(conn: &Connection, table: &str) -> Result<TableSchema> {
    validate_ident(table)?;
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", quote_ident(table)))?;
    let columns = stmt
        .query_map([], |row| {
            Ok(Column {
                name: row.get(1)?,
                decl_type: row.get::<_, String>(2)?,
                not_null: row.get::<_, i64>(3)? != 0,
                default_value: row.get(4)?,
                pk: row.get::<_, i64>(5)? != 0,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let schema = TableSchema {
        name: table.to_string(),
        columns,
    };
    if schema.has_required_columns() && path_is_unique(conn, &schema.name)? {
        Ok(schema)
    } else {
        Err(FsError::NotFound.into())
    }
}

fn path_is_unique(conn: &Connection, table: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA index_list({})", quote_ident(table)))?;
    let indexes = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? != 0,
                row.get::<_, i64>(4)? != 0,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    for (index, unique, partial) in indexes {
        if !unique || partial {
            continue;
        }
        let mut stmt = conn.prepare(&format!("PRAGMA index_info({})", quote_ident(&index)))?;
        let columns = stmt
            .query_map([], |row| row.get::<_, Option<String>>(2))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if columns.len() == 1 && columns[0].as_deref() == Some("_slfs_path") {
            return Ok(true);
        }
    }
    Ok(false)
}

impl TableSchema {
    fn has_required_columns(&self) -> bool {
        let Some(path) = self.column("_slfs_path") else {
            return false;
        };
        let Some(content) = self.column("_slfs_content") else {
            return false;
        };
        let Some(invalid_update) = self.column("_slfs_invalid_update") else {
            return false;
        };

        path.is_required_text() && content.is_required_text() && invalid_update.is_required_text()
    }

    fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|column| column.name == name)
    }

    fn domain_columns(&self) -> Vec<&Column> {
        self.columns
            .iter()
            .filter(|column| !column.name.starts_with("_slfs_"))
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnAffinity {
    Text,
    Integer,
    Real,
    Numeric,
    Blob,
}

impl Column {
    fn needs_insert_value(&self) -> bool {
        (self.not_null || (self.pk && self.affinity() != ColumnAffinity::Integer))
            && self.default_value.is_none()
    }

    fn is_required_text(&self) -> bool {
        self.affinity() == ColumnAffinity::Text && self.not_null
    }

    fn affinity(&self) -> ColumnAffinity {
        let ty = self.decl_type.to_ascii_uppercase();
        if ty.contains("INT") {
            ColumnAffinity::Integer
        } else if ty.contains("CHAR") || ty.contains("CLOB") || ty.contains("TEXT") {
            ColumnAffinity::Text
        } else if ty.contains("REAL") || ty.contains("FLOA") || ty.contains("DOUB") {
            ColumnAffinity::Real
        } else if ty.contains("BLOB") || ty.is_empty() {
            ColumnAffinity::Blob
        } else {
            ColumnAffinity::Numeric
        }
    }

    fn property_type_error(&self, value: &JsonValue) -> Option<&'static str> {
        if value.is_null() {
            return self.not_null.then_some("cannot be null");
        }
        match self.affinity() {
            ColumnAffinity::Text => value.as_str().is_none().then_some("must be text"),
            ColumnAffinity::Integer => (!is_json_integer(value)).then_some("must be integer"),
            ColumnAffinity::Real => (!value.is_number()).then_some("must be number"),
            ColumnAffinity::Numeric => value
                .is_array()
                .then_some("must be scalar")
                .or_else(|| value.is_object().then_some("must be scalar")),
            ColumnAffinity::Blob => Some("blob properties are unsupported"),
        }
    }
}

fn validate_ident(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 255
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if valid {
        Ok(())
    } else {
        Err(FsError::InvalidInput.into())
    }
}

fn parse_markdown(text: &str) -> Result<(JsonMap<String, JsonValue>, String)> {
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return Ok((JsonMap::new(), text.to_string()));
    };
    let Some((yaml, body)) = split_frontmatter(rest) else {
        return Ok((JsonMap::new(), text.to_string()));
    };
    let yaml_value = if yaml.trim().is_empty() {
        JsonValue::Object(JsonMap::new())
    } else {
        serde_json::to_value(serde_yaml::from_str::<serde_yaml::Value>(yaml)?)?
    };
    let JsonValue::Object(map) = yaml_value else {
        return Err(FsError::InvalidInput.into());
    };
    Ok((map, body.to_string()))
}

fn split_frontmatter(rest: &str) -> Option<(&str, &str)> {
    for delimiter in ["\n---\n", "\n---\r\n"] {
        if let Some(index) = rest.find(delimiter) {
            return Some((&rest[..index], &rest[index + delimiter.len()..]));
        }
    }
    None
}

fn read_slice(data: &[u8], offset: u64, size: usize) -> Result<Vec<u8>> {
    let start = usize::try_from(offset).map_err(|_| FsError::InvalidInput)?;
    if start >= data.len() {
        return Ok(Vec::new());
    }
    let end = start.saturating_add(size).min(data.len());
    Ok(data[start..end].to_vec())
}

fn invalid_entry(attempted: JsonValue, error: &str) -> JsonValue {
    json!({ "attempted": attempted, "error": error })
}

fn is_semantic_sql_error(err: &Error) -> bool {
    matches!(
        err,
        Error::Sql(rusqlite::Error::SqliteFailure(code, _))
            if code.code == rusqlite::ErrorCode::ConstraintViolation
                || code.code == rusqlite::ErrorCode::TypeMismatch
    )
}

fn is_json_integer(value: &JsonValue) -> bool {
    value
        .as_i64()
        .map(|_| true)
        .or_else(|| value.as_u64().map(|n| i64::try_from(n).is_ok()))
        .unwrap_or(false)
}

fn json_to_sql(value: &JsonValue) -> SqlValue {
    match value {
        JsonValue::Null => SqlValue::Null,
        JsonValue::Bool(value) => SqlValue::Integer(i64::from(*value)),
        JsonValue::Number(value) => value
            .as_i64()
            .map(SqlValue::Integer)
            .or_else(|| {
                value
                    .as_u64()
                    .and_then(|n| i64::try_from(n).ok())
                    .map(SqlValue::Integer)
            })
            .or_else(|| value.as_f64().map(SqlValue::Real))
            .unwrap_or(SqlValue::Null),
        JsonValue::String(value) => SqlValue::Text(value.clone()),
        JsonValue::Array(_) | JsonValue::Object(_) => SqlValue::Text(value.to_string()),
    }
}

fn sql_to_json(value: &SqlValue) -> JsonValue {
    match value {
        SqlValue::Null => JsonValue::Null,
        SqlValue::Integer(value) => JsonValue::from(*value),
        SqlValue::Real(value) => JsonValue::from(*value),
        SqlValue::Text(value) => JsonValue::from(value.clone()),
        SqlValue::Blob(value) => JsonValue::from(String::from_utf8_lossy(value).to_string()),
    }
}

fn generic_default(column: &Column) -> SqlValue {
    match column.affinity() {
        ColumnAffinity::Integer => SqlValue::Integer(0),
        ColumnAffinity::Real | ColumnAffinity::Numeric => SqlValue::Real(0.0),
        ColumnAffinity::Blob => SqlValue::Blob(Vec::new()),
        ColumnAffinity::Text => SqlValue::Text(format!("untitled-{}", short_suffix())),
    }
}

fn short_suffix() -> String {
    let count = PLACEHOLDER_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}-{:x}-{count:x}", std::process::id(), nanos)
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn to_fs_error(err: Error) -> FsError {
    match err {
        Error::Fs(err) => err,
        Error::Yaml(_) | Error::Utf8(_) => FsError::InvalidInput,
        Error::Sql(_) | Error::Json(_) => FsError::Io,
        Error::Io(err) => io_to_fs_error(err),
    }
}

fn io_to_fs_error(err: std::io::Error) -> FsError {
    match err.kind() {
        std::io::ErrorKind::NotFound => FsError::NotFound,
        std::io::ErrorKind::AlreadyExists => FsError::Exists,
        std::io::ErrorKind::PermissionDenied => FsError::PermissionDenied,
        std::io::ErrorKind::InvalidInput => FsError::InvalidInput,
        _ => FsError::Io,
    }
}

impl From<FsError> for Error {
    fn from(value: FsError) -> Self {
        Error::Fs(value)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(value: rusqlite::Error) -> Self {
        Error::Sql(value)
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Error::Json(value)
    }
}

impl From<serde_yaml::Error> for Error {
    fn from(value: serde_yaml::Error) -> Self {
        Error::Yaml(value)
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(value: std::str::Utf8Error) -> Self {
        Error::Utf8(value)
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Error::Io(value)
    }
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
