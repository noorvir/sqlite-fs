use minfuse::{
    Attr, DirEntry, EntryKind, FileHandle, FileSystem, FsError, FsResult, NO_HANDLE, OpenOptions,
    RenameFlags, SetTimes, StatFs,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::SystemTime;

const HELLO_PATH: &str = "/hello.txt";
const DEFAULT_CAPACITY: usize = 4096;
const MAX_NAME_LEN: usize = 255;

pub struct MinFs {
    inner: Mutex<Inner>,
    capacity: usize,
}

struct Inner {
    files: BTreeMap<String, FileNode>,
    dirs: BTreeMap<String, DirNode>,
    handles: HashMap<FileHandle, OpenFile>,
    next_handle: FileHandle,
}

#[derive(Clone)]
struct FileNode {
    content: Vec<u8>,
    perm: u16,
    uid: u32,
    gid: u32,
    accessed: SystemTime,
    modified: SystemTime,
    changed: SystemTime,
    created: SystemTime,
}

#[derive(Clone)]
struct DirNode {
    perm: u16,
    uid: u32,
    gid: u32,
    accessed: SystemTime,
    modified: SystemTime,
    changed: SystemTime,
    created: SystemTime,
}

struct OpenFile {
    path: String,
    staged: Vec<u8>,
    dirty: bool,
    writable: bool,
    append: bool,
}

impl MinFs {
    pub fn new() -> Self {
        let mut dirs = BTreeMap::new();
        dirs.insert("/".to_string(), DirNode::new(0o755));

        let mut files = BTreeMap::new();
        files.insert(
            HELLO_PATH.to_string(),
            FileNode::new(0o644, b"hello from minfs\n".to_vec()),
        );

        Self {
            inner: Mutex::new(Inner {
                files,
                dirs,
                handles: HashMap::new(),
                next_handle: 1,
            }),
            capacity: DEFAULT_CAPACITY,
        }
    }

    fn alloc_handle(inner: &mut Inner, open: OpenFile) -> FileHandle {
        let handle = inner.next_handle;
        inner.next_handle += 1;
        inner.handles.insert(handle, open);
        handle
    }

    fn commit_handle(inner: &mut Inner, handle: FileHandle) -> FsResult<()> {
        if handle == NO_HANDLE {
            return Ok(());
        }

        let Some(open) = inner.handles.get(&handle) else {
            return Err(FsError::BadFileDescriptor);
        };
        if !open.dirty {
            return Ok(());
        }

        let path = open.path.clone();
        let staged = open.staged.clone();
        let Some(file) = inner.files.get_mut(&path) else {
            return Err(FsError::NotFound);
        };
        file.content = staged;
        file.touch_data();

        if let Some(open) = inner.handles.get_mut(&handle) {
            open.dirty = false;
        }
        Ok(())
    }

    fn read_from(data: &[u8], offset: u64, size: usize) -> FsResult<Vec<u8>> {
        let start = usize::try_from(offset).map_err(|_| FsError::InvalidInput)?;
        if start >= data.len() {
            return Ok(Vec::new());
        }
        let end = start.saturating_add(size).min(data.len());
        Ok(data[start..end].to_vec())
    }

    fn ensure_capacity(&self, offset: usize, len: usize) -> FsResult<usize> {
        let end = offset.checked_add(len).ok_or(FsError::FileTooLarge)?;
        if end > self.capacity {
            return Err(FsError::FileTooLarge);
        }
        Ok(end)
    }
}

impl Default for MinFs {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSystem for MinFs {
    fn getattr(&self, path: &str) -> FsResult<Attr> {
        let path = normalize(path)?;
        let inner = self.inner.lock().unwrap();
        if let Some(dir) = inner.dirs.get(&path) {
            return Ok(dir.attr());
        }
        if let Some(file) = inner.files.get(&path) {
            return Ok(file.attr());
        }
        Err(FsError::NotFound)
    }

    fn readdir(&self, path: &str) -> FsResult<Vec<DirEntry>> {
        let path = normalize(path)?;
        let inner = self.inner.lock().unwrap();
        if !inner.dirs.contains_key(&path) {
            return if inner.files.contains_key(&path) {
                Err(FsError::NotDirectory)
            } else {
                Err(FsError::NotFound)
            };
        }

        let mut entries = BTreeMap::new();
        for dir in inner.dirs.keys() {
            if dir != "/"
                && let Some(name) = child_name(&path, dir)
            {
                entries.insert(name, EntryKind::Directory);
            }
        }
        for file in inner.files.keys() {
            if let Some(name) = child_name(&path, file) {
                entries.insert(name, EntryKind::File);
            }
        }

        Ok(entries
            .into_iter()
            .map(|(name, kind)| match kind {
                EntryKind::File => DirEntry::file(name),
                EntryKind::Directory => DirEntry::directory(name),
            })
            .collect())
    }

    fn open(&self, path: &str, options: OpenOptions) -> FsResult<FileHandle> {
        let path = normalize(path)?;
        let mut inner = self.inner.lock().unwrap();
        if inner.dirs.contains_key(&path) {
            return Err(FsError::IsDirectory);
        }
        let file = inner.files.get(&path).ok_or(FsError::NotFound)?;
        let mut staged = file.content.clone();
        let mut dirty = false;
        if options.truncate() {
            if !options.writable() {
                return Err(FsError::InvalidInput);
            }
            staged.clear();
            dirty = true;
        }
        Ok(Self::alloc_handle(
            &mut inner,
            OpenFile {
                path,
                staged,
                dirty,
                writable: options.writable(),
                append: options.append(),
            },
        ))
    }

    fn create(&self, path: &str, mode: u32, options: OpenOptions) -> FsResult<FileHandle> {
        let path = normalize(path)?;
        let (parent, _) = parent_name(&path)?;
        let mut inner = self.inner.lock().unwrap();
        if !inner.dirs.contains_key(&parent) {
            return Err(FsError::NotDirectory);
        }
        if inner.dirs.contains_key(&path) {
            return Err(FsError::IsDirectory);
        }
        if inner.files.contains_key(&path) {
            return Err(FsError::Exists);
        }

        inner
            .files
            .insert(path.clone(), FileNode::new(mode as u16 & 0o777, Vec::new()));
        Ok(Self::alloc_handle(
            &mut inner,
            OpenFile {
                path,
                staged: Vec::new(),
                dirty: false,
                writable: options.writable(),
                append: options.append(),
            },
        ))
    }

    fn read(&self, path: &str, handle: FileHandle, offset: u64, size: usize) -> FsResult<Vec<u8>> {
        let path = normalize(path)?;
        let inner = self.inner.lock().unwrap();
        if handle != NO_HANDLE {
            let open = inner
                .handles
                .get(&handle)
                .ok_or(FsError::BadFileDescriptor)?;
            return Self::read_from(&open.staged, offset, size);
        }
        let file = inner.files.get(&path).ok_or(FsError::NotFound)?;
        Self::read_from(&file.content, offset, size)
    }

    fn write(&self, _path: &str, handle: FileHandle, offset: u64, input: &[u8]) -> FsResult<usize> {
        let mut inner = self.inner.lock().unwrap();
        let open = inner
            .handles
            .get_mut(&handle)
            .ok_or(FsError::BadFileDescriptor)?;
        if !open.writable {
            return Err(FsError::BadFileDescriptor);
        }

        let start = if open.append {
            open.staged.len()
        } else {
            usize::try_from(offset).map_err(|_| FsError::InvalidInput)?
        };
        let end = self.ensure_capacity(start, input.len())?;
        if end > open.staged.len() {
            open.staged.resize(end, 0);
        }
        open.staged[start..end].copy_from_slice(input);
        open.dirty = true;
        Ok(input.len())
    }

    fn flush(&self, _path: &str, handle: FileHandle) -> FsResult<()> {
        let mut inner = self.inner.lock().unwrap();
        Self::commit_handle(&mut inner, handle)
    }

    fn fsync(&self, _path: &str, handle: FileHandle, _datasync: bool) -> FsResult<()> {
        let mut inner = self.inner.lock().unwrap();
        Self::commit_handle(&mut inner, handle)
    }

    fn release(&self, _path: &str, handle: FileHandle) -> FsResult<()> {
        let mut inner = self.inner.lock().unwrap();
        let result = Self::commit_handle(&mut inner, handle);
        inner.handles.remove(&handle);
        result
    }

    fn truncate(&self, path: &str, handle: Option<FileHandle>, size: u64) -> FsResult<()> {
        let path = normalize(path)?;
        let size = usize::try_from(size).map_err(|_| FsError::FileTooLarge)?;
        self.ensure_capacity(0, size)?;
        let mut inner = self.inner.lock().unwrap();

        if let Some(handle) = handle {
            if let Some(open) = inner.handles.get_mut(&handle) {
                if !open.writable {
                    return Err(FsError::BadFileDescriptor);
                }
                open.staged.resize(size, 0);
                open.dirty = true;
                return Ok(());
            }
            return Err(FsError::BadFileDescriptor);
        }

        if inner.dirs.contains_key(&path) {
            return Err(FsError::IsDirectory);
        }
        let file = inner.files.get_mut(&path).ok_or(FsError::NotFound)?;
        file.content.resize(size, 0);
        file.touch_data();
        Ok(())
    }

    fn mknod(&self, path: &str, mode: u32, _rdev: u64) -> FsResult<()> {
        let path = normalize(path)?;
        let (parent, _) = parent_name(&path)?;
        let mut inner = self.inner.lock().unwrap();
        if !inner.dirs.contains_key(&parent) {
            return Err(FsError::NotDirectory);
        }
        if inner.files.contains_key(&path) || inner.dirs.contains_key(&path) {
            return Err(FsError::Exists);
        }
        inner
            .files
            .insert(path, FileNode::new(mode as u16 & 0o777, Vec::new()));
        Ok(())
    }

    fn mkdir(&self, path: &str, mode: u32) -> FsResult<()> {
        let path = normalize(path)?;
        let (parent, _) = parent_name(&path)?;
        let mut inner = self.inner.lock().unwrap();
        if !inner.dirs.contains_key(&parent) {
            return Err(FsError::NotDirectory);
        }
        if inner.files.contains_key(&path) || inner.dirs.contains_key(&path) {
            return Err(FsError::Exists);
        }
        inner.dirs.insert(path, DirNode::new(mode as u16 & 0o777));
        Ok(())
    }

    fn unlink(&self, path: &str) -> FsResult<()> {
        let path = normalize(path)?;
        let mut inner = self.inner.lock().unwrap();
        if inner.dirs.contains_key(&path) {
            return Err(FsError::IsDirectory);
        }
        inner
            .files
            .remove(&path)
            .map(|_| ())
            .ok_or(FsError::NotFound)
    }

    fn rmdir(&self, path: &str) -> FsResult<()> {
        let path = normalize(path)?;
        if path == "/" {
            return Err(FsError::InvalidInput);
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.files.contains_key(&path) {
            return Err(FsError::NotDirectory);
        }
        if !inner.dirs.contains_key(&path) {
            return Err(FsError::NotFound);
        }
        if has_children(&inner, &path) {
            return Err(FsError::DirectoryNotEmpty);
        }
        inner.dirs.remove(&path);
        Ok(())
    }

    fn rename(&self, from: &str, to: &str, _flags: RenameFlags) -> FsResult<()> {
        let from = normalize(from)?;
        let to = normalize(to)?;
        let (to_parent, _) = parent_name(&to)?;
        let mut inner = self.inner.lock().unwrap();
        if !inner.dirs.contains_key(&to_parent) {
            return Err(FsError::NotDirectory);
        }
        if inner.dirs.contains_key(&to) {
            return Err(FsError::IsDirectory);
        }

        if let Some(file) = inner.files.remove(&from) {
            inner.files.insert(to.clone(), file);
            for open in inner.handles.values_mut() {
                if open.path == from {
                    open.path = to.clone();
                }
            }
            return Ok(());
        }

        if inner.dirs.contains_key(&from) {
            if has_children(&inner, &from) {
                return Err(FsError::DirectoryNotEmpty);
            }
            let dir = inner.dirs.remove(&from).unwrap();
            inner.dirs.insert(to, dir);
            return Ok(());
        }

        Err(FsError::NotFound)
    }

    fn access(&self, path: &str, _mask: i32) -> FsResult<()> {
        self.getattr(path).map(|_| ())
    }

    fn chmod(&self, path: &str, _handle: Option<FileHandle>, mode: u32) -> FsResult<()> {
        let path = normalize(path)?;
        let mut inner = self.inner.lock().unwrap();
        if let Some(file) = inner.files.get_mut(&path) {
            file.perm = mode as u16 & 0o777;
            file.touch_metadata();
            return Ok(());
        }
        if let Some(dir) = inner.dirs.get_mut(&path) {
            dir.perm = mode as u16 & 0o777;
            dir.touch_metadata();
            return Ok(());
        }
        Err(FsError::NotFound)
    }

    fn chown(&self, path: &str, _handle: Option<FileHandle>, uid: u32, gid: u32) -> FsResult<()> {
        let path = normalize(path)?;
        let mut inner = self.inner.lock().unwrap();
        if let Some(file) = inner.files.get_mut(&path) {
            file.uid = uid;
            file.gid = gid;
            file.touch_metadata();
            return Ok(());
        }
        if let Some(dir) = inner.dirs.get_mut(&path) {
            dir.uid = uid;
            dir.gid = gid;
            dir.touch_metadata();
            return Ok(());
        }
        Err(FsError::NotFound)
    }

    fn utimens(&self, path: &str, _handle: Option<FileHandle>, times: SetTimes) -> FsResult<()> {
        let path = normalize(path)?;
        let mut inner = self.inner.lock().unwrap();
        if let Some(file) = inner.files.get_mut(&path) {
            file.accessed = times.accessed.resolve(file.accessed);
            file.modified = times.modified.resolve(file.modified);
            file.touch_metadata();
            return Ok(());
        }
        if let Some(dir) = inner.dirs.get_mut(&path) {
            dir.accessed = times.accessed.resolve(dir.accessed);
            dir.modified = times.modified.resolve(dir.modified);
            dir.touch_metadata();
            return Ok(());
        }
        Err(FsError::NotFound)
    }

    fn statfs(&self, _path: &str) -> FsResult<StatFs> {
        let inner = self.inner.lock().unwrap();
        Ok(StatFs {
            files: inner.files.len() as u64 + inner.dirs.len() as u64,
            files_free: 1024 * 1024,
            ..StatFs::default()
        })
    }
}

impl FileNode {
    fn new(perm: u16, content: Vec<u8>) -> Self {
        let now = SystemTime::now();
        Self {
            content,
            perm,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            accessed: now,
            modified: now,
            changed: now,
            created: now,
        }
    }

    fn attr(&self) -> Attr {
        Attr {
            kind: EntryKind::File,
            len: self.content.len() as u64,
            perm: self.perm,
            uid: self.uid,
            gid: self.gid,
            accessed: self.accessed,
            modified: self.modified,
            changed: self.changed,
            created: self.created,
        }
    }

    fn touch_data(&mut self) {
        let now = SystemTime::now();
        self.modified = now;
        self.changed = now;
    }

    fn touch_metadata(&mut self) {
        self.changed = SystemTime::now();
    }
}

impl DirNode {
    fn new(perm: u16) -> Self {
        let now = SystemTime::now();
        Self {
            perm,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            accessed: now,
            modified: now,
            changed: now,
            created: now,
        }
    }

    fn attr(&self) -> Attr {
        Attr {
            kind: EntryKind::Directory,
            len: 0,
            perm: self.perm,
            uid: self.uid,
            gid: self.gid,
            accessed: self.accessed,
            modified: self.modified,
            changed: self.changed,
            created: self.created,
        }
    }

    fn touch_metadata(&mut self) {
        self.changed = SystemTime::now();
    }
}

fn normalize(path: &str) -> FsResult<String> {
    if !path.starts_with('/') || path.contains('\0') {
        return Err(FsError::InvalidInput);
    }
    let path = if path == "/" {
        "/".to_string()
    } else {
        path.trim_end_matches('/').to_string()
    };
    if path.len() > 4096 {
        return Err(FsError::NameTooLong);
    }
    if let Some(name) = path.rsplit('/').next()
        && name.len() > MAX_NAME_LEN
    {
        return Err(FsError::NameTooLong);
    }
    Ok(path)
}

fn parent_name(path: &str) -> FsResult<(String, String)> {
    if path == "/" {
        return Err(FsError::InvalidInput);
    }
    let Some(index) = path.rfind('/') else {
        return Err(FsError::InvalidInput);
    };
    let parent = if index == 0 { "/" } else { &path[..index] };
    let name = &path[index + 1..];
    if name.is_empty() {
        return Err(FsError::InvalidInput);
    }
    Ok((parent.to_string(), name.to_string()))
}

fn child_name(parent: &str, child: &str) -> Option<String> {
    let rest = if parent == "/" {
        child.strip_prefix('/')?
    } else {
        child.strip_prefix(parent)?.strip_prefix('/')?
    };
    (!rest.is_empty() && !rest.contains('/')).then(|| rest.to_string())
}

fn has_children(inner: &Inner, dir: &str) -> bool {
    let prefix = format!("{dir}/");
    inner.files.keys().any(|path| path.starts_with(&prefix))
        || inner
            .dirs
            .keys()
            .any(|path| path != dir && path.starts_with(&prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rw() -> OpenOptions {
        OpenOptions::from_raw(libc::O_RDWR)
    }

    fn wo() -> OpenOptions {
        OpenOptions::from_raw(libc::O_WRONLY)
    }

    #[test]
    fn staged_write_commits_on_flush() {
        let fs = MinFs::new();
        let handle = fs.open(HELLO_PATH, rw()).unwrap();
        fs.write(HELLO_PATH, handle, 0, b"abc").unwrap();

        assert_eq!(
            fs.read(HELLO_PATH, NO_HANDLE, 0, 100).unwrap(),
            b"hello from minfs\n"
        );
        assert_eq!(
            fs.read(HELLO_PATH, handle, 0, 100).unwrap(),
            b"abclo from minfs\n"
        );

        fs.flush(HELLO_PATH, handle).unwrap();
        assert_eq!(
            fs.read(HELLO_PATH, NO_HANDLE, 0, 100).unwrap(),
            b"abclo from minfs\n"
        );
    }

    #[test]
    fn release_commits_and_closes_handle() {
        let fs = MinFs::new();
        let handle = fs.open(HELLO_PATH, rw()).unwrap();
        fs.truncate(HELLO_PATH, Some(handle), 0).unwrap();
        fs.write(HELLO_PATH, handle, 0, b"new").unwrap();
        fs.release(HELLO_PATH, handle).unwrap();

        assert_eq!(fs.read(HELLO_PATH, NO_HANDLE, 0, 100).unwrap(), b"new");
        assert!(matches!(
            fs.read(HELLO_PATH, handle, 0, 100),
            Err(FsError::BadFileDescriptor)
        ));
    }

    #[test]
    fn create_rename_unlink_file() {
        let fs = MinFs::new();
        let handle = fs.create("/draft.md", 0o644, wo()).unwrap();
        fs.write("/draft.md", handle, 0, b"body").unwrap();
        fs.release("/draft.md", handle).unwrap();
        fs.rename("/draft.md", "/final.md", RenameFlags::from_raw(0))
            .unwrap();

        assert_eq!(fs.read("/final.md", NO_HANDLE, 0, 100).unwrap(), b"body");
        fs.unlink("/final.md").unwrap();
        assert!(matches!(fs.getattr("/final.md"), Err(FsError::NotFound)));
    }

    #[test]
    fn directories_list_immediate_children() {
        let fs = MinFs::new();
        fs.mkdir("/Contacts", 0o755).unwrap();
        let handle = fs.create("/Contacts/Ada.md", 0o644, wo()).unwrap();
        fs.release("/Contacts/Ada.md", handle).unwrap();

        let root: Vec<_> = fs
            .readdir("/")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(root.contains(&"Contacts".to_string()));
        assert!(root.contains(&"hello.txt".to_string()));

        let contacts: Vec<_> = fs
            .readdir("/Contacts")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(contacts, vec!["Ada.md"]);
    }
}
