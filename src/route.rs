use crate::error::Result;
use crate::schema::table_schema;
use minfuse::FsError;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DocPath {
    table: String,
    file: String,
}

impl DocPath {
    pub(crate) fn table(&self) -> &str {
        &self.table
    }

    pub(crate) fn file(&self) -> &str {
        &self.file
    }
}

#[derive(Debug)]
pub(crate) enum Route {
    Root,
    TableDir(String),
    Typed(DocPath),
    Pass(PathBuf),
}

pub(crate) fn route(conn: &rusqlite::Connection, backing: &Path, path: &str) -> Result<Route> {
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

pub(crate) fn is_typed_file_name(name: &str) -> bool {
    name.ends_with(".md")
        && !name.starts_with("._")
        && name != ".DS_Store"
        && !name.contains('/')
        && !name.contains('\0')
        && name.len() <= 255
}

pub(crate) fn path_to_str(path: &Path) -> Result<&str> {
    path.to_str().ok_or(FsError::InvalidInput.into())
}
