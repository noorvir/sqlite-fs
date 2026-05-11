use minfuse::{Attr, DirEntry, EntryKind, FileSystem, FsError, FsResult};
use std::sync::Mutex;
use std::time::SystemTime;

const FILE_PATH: &str = "/hello.txt";
const CAPACITY: usize = 4096;

pub struct MinFs {
    data: Mutex<Vec<u8>>,
    modified: Mutex<SystemTime>,
}

impl MinFs {
    pub fn new() -> Self {
        Self {
            data: Mutex::new(b"hello from minfs\n".to_vec()),
            modified: Mutex::new(SystemTime::now()),
        }
    }

    fn file_attr(&self) -> Attr {
        Attr {
            kind: EntryKind::File,
            len: self.data.lock().unwrap().len() as u64,
            perm: 0o644,
            modified: *self.modified.lock().unwrap(),
        }
    }
}

impl Default for MinFs {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSystem for MinFs {
    fn getattr(&self, path: &str) -> FsResult<Attr> {
        match path {
            "/" => Ok(Attr::directory()),
            FILE_PATH => Ok(self.file_attr()),
            _ => Err(FsError::NotFound),
        }
    }

    fn readdir(&self, path: &str) -> FsResult<Vec<DirEntry>> {
        if path == "/" {
            Ok(vec![DirEntry::file("hello.txt")])
        } else {
            Err(FsError::NotDirectory)
        }
    }

    fn read(&self, path: &str, offset: u64, size: usize) -> FsResult<Vec<u8>> {
        if path != FILE_PATH {
            return Err(FsError::NotFound);
        }
        let data = self.data.lock().unwrap();
        let start = offset as usize;
        let end = (start + size).min(data.len());
        Ok(if start < data.len() {
            data[start..end].to_vec()
        } else {
            Vec::new()
        })
    }

    fn write(&self, path: &str, offset: u64, input: &[u8]) -> FsResult<usize> {
        if path != FILE_PATH {
            return Err(FsError::NotFound);
        }
        let start = offset as usize;
        if start > CAPACITY || input.len() > CAPACITY - start {
            return Err(FsError::FileTooLarge);
        }
        let mut data = self.data.lock().unwrap();
        let end = start + input.len();
        if end > data.len() {
            data.resize(end, 0);
        }
        data[start..end].copy_from_slice(input);
        *self.modified.lock().unwrap() = SystemTime::now();
        Ok(input.len())
    }

    fn truncate(&self, path: &str, size: u64) -> FsResult<()> {
        if path != FILE_PATH {
            return Err(FsError::NotFound);
        }
        if size as usize > CAPACITY {
            return Err(FsError::FileTooLarge);
        }
        self.data.lock().unwrap().resize(size as usize, 0);
        *self.modified.lock().unwrap() = SystemTime::now();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write_round_trip() {
        let fs = MinFs::new();
        fs.write(FILE_PATH, 0, b"abc").unwrap();
        assert_eq!(fs.read(FILE_PATH, 0, 10).unwrap(), b"abclo from minfs\n");
        fs.truncate(FILE_PATH, 3).unwrap();
        assert_eq!(fs.read(FILE_PATH, 0, 10).unwrap(), b"abc");
    }
}
