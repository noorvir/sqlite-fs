use minfuse::{
    Attr, DirEntry, FileHandle, FileSystem, FsError, FsResult, NO_HANDLE, OpenOptions, RenameFlags,
    SetTimes, StatFs,
};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Transaction, params, params_from_iter};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

static PLACEHOLDER_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct SqliteFs {
    inner: Mutex<Inner>,
}

struct Inner {
    conn: Connection,
    handles: HashMap<FileHandle, OpenFile>,
    next_handle: FileHandle,
}

#[derive(Clone)]
struct OpenFile {
    path: DocPath,
    staged: Vec<u8>,
    dirty: bool,
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
    has_default: bool,
    pk: bool,
}

#[derive(Debug)]
pub enum Error {
    Fs(FsError),
    Sql(rusqlite::Error),
    Json(serde_json::Error),
    Yaml(serde_yaml::Error),
    Utf8(std::str::Utf8Error),
}

type Result<T> = std::result::Result<T, Error>;

impl SqliteFs {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self {
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
        let Some(open) = inner.handles.get(&handle).cloned() else {
            return Err(FsError::BadFileDescriptor.into());
        };
        if open.dirty {
            commit_document(&mut inner.conn, &open.path, &open.staged)?;
            if let Some(open) = inner.handles.get_mut(&handle) {
                open.dirty = false;
            }
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
        let doc = parse_doc_path(path).map_err(to_fs_error)?;
        let mut inner = self.inner.lock().unwrap();
        if document_exists(&inner.conn, &doc).map_err(to_fs_error)? {
            return Err(FsError::Exists);
        }
        commit_document(&mut inner.conn, &doc, b"").map_err(to_fs_error)
    }

    fn unlink(&self, path: &str) -> FsResult<()> {
        let doc = parse_doc_path(path).map_err(to_fs_error)?;
        let inner = self.inner.lock().unwrap();
        let schema = table_schema(&inner.conn, &doc.table).map_err(to_fs_error)?;
        let changed = inner
            .conn
            .execute(
                &format!(
                    "DELETE FROM {} WHERE {} = ?1",
                    quote_ident(&schema.name),
                    quote_ident("_slfs_path")
                ),
                params![doc.file],
            )
            .map_err(|err| to_fs_error(err.into()))?;
        if changed == 0 {
            Err(FsError::NotFound)
        } else {
            Ok(())
        }
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
        match parse_path(path)? {
            VPath::Root => Ok(Attr::directory()),
            VPath::Table(table) => {
                table_schema(&inner.conn, &table)?;
                Ok(Attr::directory())
            }
            VPath::File(doc) => {
                if let Some(open) = inner
                    .handles
                    .values()
                    .find(|open| open.path == doc && open.dirty)
                {
                    return Ok(Attr::file(open.staged.len() as u64));
                }
                match render_document(&inner.conn, &doc) {
                    Ok(rendered) => Ok(Attr::file(rendered.len() as u64)),
                    Err(Error::Fs(FsError::NotFound)) => inner
                        .handles
                        .values()
                        .find(|open| open.path == doc)
                        .map(|open| Attr::file(open.staged.len() as u64))
                        .ok_or_else(|| FsError::NotFound.into()),
                    Err(err) => Err(err),
                }
            }
        }
    }

    fn try_readdir(&self, path: &str) -> Result<Vec<DirEntry>> {
        let inner = self.inner.lock().unwrap();
        match parse_path(path)? {
            VPath::Root => eligible_tables(&inner.conn)
                .map(|tables| tables.into_iter().map(DirEntry::directory).collect()),
            VPath::Table(table) => {
                let schema = table_schema(&inner.conn, &table)?;
                let mut stmt = inner.conn.prepare(&format!(
                    "SELECT {} FROM {} WHERE {} LIKE '%.md' ORDER BY {}",
                    quote_ident("_slfs_path"),
                    quote_ident(&schema.name),
                    quote_ident("_slfs_path"),
                    quote_ident("_slfs_path")
                ))?;
                let mut rows = stmt
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<std::result::Result<BTreeSet<_>, _>>()?;
                for open in inner.handles.values() {
                    if open.path.table == table {
                        rows.insert(open.path.file.clone());
                    }
                }
                Ok(rows.into_iter().map(DirEntry::file).collect())
            }
            VPath::File(_) => Err(FsError::NotDirectory.into()),
        }
    }

    fn try_open(&self, path: &str, options: OpenOptions) -> Result<FileHandle> {
        let doc = parse_doc_path(path)?;
        let mut inner = self.inner.lock().unwrap();
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
            OpenFile {
                path: doc,
                staged,
                dirty,
                writable: options.writable(),
            },
        ))
    }

    fn try_create(&self, path: &str, options: OpenOptions) -> Result<FileHandle> {
        let doc = parse_doc_path(path)?;
        let mut inner = self.inner.lock().unwrap();
        table_schema(&inner.conn, &doc.table)?;
        if document_exists(&inner.conn, &doc)? {
            return Err(FsError::Exists.into());
        }
        Ok(Self::alloc_handle(
            &mut inner,
            OpenFile {
                path: doc,
                staged: Vec::new(),
                dirty: true,
                writable: options.writable(),
            },
        ))
    }

    fn try_read(
        &self,
        path: &str,
        handle: FileHandle,
        offset: u64,
        size: usize,
    ) -> Result<Vec<u8>> {
        let inner = self.inner.lock().unwrap();
        let data = if handle != NO_HANDLE {
            inner
                .handles
                .get(&handle)
                .ok_or(FsError::BadFileDescriptor)?
                .staged
                .clone()
        } else {
            render_document(&inner.conn, &parse_doc_path(path)?)?.into_bytes()
        };
        read_slice(&data, offset, size)
    }

    fn try_write(&self, handle: FileHandle, offset: u64, input: &[u8]) -> Result<usize> {
        let mut inner = self.inner.lock().unwrap();
        let open = inner
            .handles
            .get_mut(&handle)
            .ok_or(FsError::BadFileDescriptor)?;
        if !open.writable {
            return Err(FsError::BadFileDescriptor.into());
        }
        let start = usize::try_from(offset).map_err(|_| FsError::InvalidInput)?;
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

    fn try_truncate(&self, path: &str, handle: Option<FileHandle>, size: u64) -> Result<()> {
        let size = usize::try_from(size).map_err(|_| FsError::FileTooLarge)?;
        let mut inner = self.inner.lock().unwrap();
        if let Some(handle) = handle {
            let open = inner
                .handles
                .get_mut(&handle)
                .ok_or(FsError::BadFileDescriptor)?;
            open.staged.resize(size, 0);
            open.dirty = true;
            return Ok(());
        }

        let doc = parse_doc_path(path)?;
        let mut staged = render_document(&inner.conn, &doc)?.into_bytes();
        staged.resize(size, 0);
        commit_document(&mut inner.conn, &doc, &staged)
    }

    fn try_metadata_noop(&self, path: &str, handle: Option<FileHandle>) -> Result<()> {
        let inner = self.inner.lock().unwrap();
        if let Some(handle) = handle {
            if inner.handles.contains_key(&handle) {
                return Ok(());
            }
            return Err(FsError::BadFileDescriptor.into());
        }
        match parse_path(path)? {
            VPath::Root => Ok(()),
            VPath::Table(table) => table_schema(&inner.conn, &table).map(|_| ()),
            VPath::File(doc) => document_exists(&inner.conn, &doc).and_then(|exists| {
                if exists {
                    Ok(())
                } else {
                    Err(FsError::NotFound.into())
                }
            }),
        }
    }

    fn try_rename(&self, from: &str, to: &str) -> Result<()> {
        let from = parse_doc_path(from)?;
        let to = parse_doc_path(to)?;
        if from.table != to.table {
            return Err(FsError::Unsupported.into());
        }
        let mut inner = self.inner.lock().unwrap();
        let schema = table_schema(&inner.conn, &from.table)?;
        let tx = inner.conn.transaction()?;
        tx.execute(
            &format!(
                "DELETE FROM {} WHERE {} = ?1",
                quote_ident(&schema.name),
                quote_ident("_slfs_path")
            ),
            params![to.file],
        )?;
        let changed = tx.execute(
            &format!(
                "UPDATE {} SET {} = ?1 WHERE {} = ?2",
                quote_ident(&schema.name),
                quote_ident("_slfs_path"),
                quote_ident("_slfs_path")
            ),
            params![to.file, from.file],
        )?;
        tx.commit()?;
        if changed == 0 {
            Err(FsError::NotFound.into())
        } else {
            for open in inner.handles.values_mut() {
                if open.path == from {
                    open.path = to.clone();
                }
            }
            Ok(())
        }
    }
}

fn commit_document(conn: &mut Connection, doc: &DocPath, bytes: &[u8]) -> Result<()> {
    let bytes = bytes.strip_suffix(&[0]).map_or(bytes, |mut trimmed| {
        while let Some(next) = trimmed.strip_suffix(&[0]) {
            trimmed = next;
        }
        trimmed
    });
    let text = std::str::from_utf8(bytes)?;
    let (props, body) = parse_markdown(text)?;
    let schema = table_schema(conn, &doc.table)?;
    let tx = conn.transaction()?;

    let existed = row_exists(&tx, &schema, &doc.file)?;
    if !existed {
        insert_base_row(&tx, &schema, &doc.file)?;
    }

    let mut invalid = load_invalid_update(&tx, &schema, &doc.file)?;
    invalid.retain(|key, _| props.contains_key(key));

    tx.execute(
        &format!(
            "UPDATE {} SET {} = ?1 WHERE {} = ?2",
            quote_ident(&schema.name),
            quote_ident("_slfs_content"),
            quote_ident("_slfs_path")
        ),
        params![body, doc.file],
    )?;

    let domain: BTreeMap<_, _> = schema
        .domain_columns()
        .into_iter()
        .map(|column| (column.name.clone(), column))
        .collect();

    for (name, value) in props {
        if name.starts_with("_slfs_") {
            invalid.insert(name, invalid_entry(value, "reserved property"));
            continue;
        }
        let Some(column) = domain.get(&name) else {
            invalid.insert(name, invalid_entry(value, "unknown property"));
            continue;
        };
        match try_update_field(&tx, &schema, column, &doc.file, &value) {
            Ok(()) => {
                invalid.remove(&name);
            }
            Err(err) if is_semantic_sql_error(&err) => {
                invalid.insert(name, invalid_entry(value, "constraint failed"));
            }
            Err(err) => return Err(err),
        }
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

fn insert_base_row(tx: &Transaction<'_>, schema: &TableSchema, file: &str) -> Result<()> {
    let mut columns = vec!["_slfs_path".to_string(), "_slfs_invalid_update".to_string()];
    let mut values = vec![
        SqlValue::Text(file.to_string()),
        SqlValue::Text("{}".to_string()),
    ];

    for column in schema.domain_columns() {
        if column.needs_insert_value() {
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

fn try_update_field(
    tx: &Transaction<'_>,
    schema: &TableSchema,
    column: &Column,
    file: &str,
    value: &JsonValue,
) -> Result<()> {
    tx.execute_batch("SAVEPOINT slfs_apply_field")?;
    let result = tx.execute(
        &format!(
            "UPDATE {} SET {} = ?1 WHERE {} = ?2",
            quote_ident(&schema.name),
            quote_ident(&column.name),
            quote_ident("_slfs_path")
        ),
        params![json_to_sql(value), file],
    );
    match result {
        Ok(_) => {
            tx.execute_batch("RELEASE slfs_apply_field")?;
            Ok(())
        }
        Err(err) => {
            tx.execute_batch("ROLLBACK TO slfs_apply_field")?;
            tx.execute_batch("RELEASE slfs_apply_field")?;
            Err(err.into())
        }
    }
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
    names
        .into_iter()
        .filter_map(|name| table_schema(conn, &name).ok().map(|_| name))
        .collect::<Vec<_>>()
        .pipe(Ok)
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
                has_default: row.get::<_, Option<String>>(4)?.is_some(),
                pk: row.get::<_, i64>(5)? != 0,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let schema = TableSchema {
        name: table.to_string(),
        columns,
    };
    if schema.has_required_columns() {
        Ok(schema)
    } else {
        Err(FsError::NotFound.into())
    }
}

impl TableSchema {
    fn has_required_columns(&self) -> bool {
        ["_slfs_path", "_slfs_content", "_slfs_invalid_update"]
            .into_iter()
            .all(|name| self.columns.iter().any(|column| column.name == name))
    }

    fn domain_columns(&self) -> Vec<&Column> {
        self.columns
            .iter()
            .filter(|column| !column.name.starts_with("_slfs_"))
            .collect()
    }
}

impl Column {
    fn needs_insert_value(&self) -> bool {
        (self.not_null || (self.pk && !self.is_integer())) && !self.has_default
    }

    fn is_integer(&self) -> bool {
        self.decl_type.to_ascii_uppercase().contains("INT")
    }
}

enum VPath {
    Root,
    Table(String),
    File(DocPath),
}

fn parse_path(path: &str) -> Result<VPath> {
    if path == "/" {
        return Ok(VPath::Root);
    }
    let trimmed = path.strip_prefix('/').ok_or(FsError::InvalidInput)?;
    if trimmed.is_empty() || trimmed.ends_with('/') {
        return Err(FsError::NotFound.into());
    }
    let parts = trimmed.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        [table] => {
            validate_ident(table)?;
            Ok(VPath::Table((*table).to_string()))
        }
        [table, file] if file.ends_with(".md") => {
            validate_ident(table)?;
            validate_file_name(file)?;
            Ok(VPath::File(DocPath {
                table: (*table).to_string(),
                file: (*file).to_string(),
            }))
        }
        [_, _] => Err(FsError::NotFound.into()),
        _ => Err(FsError::NotDirectory.into()),
    }
}

fn parse_doc_path(path: &str) -> Result<DocPath> {
    match parse_path(path)? {
        VPath::File(doc) => Ok(doc),
        VPath::Root | VPath::Table(_) => Err(FsError::IsDirectory.into()),
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

fn validate_file_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 255
        || name.contains('/')
        || name.contains('\0')
        || name.starts_with("._")
        || name == ".DS_Store"
    {
        Err(FsError::NotFound.into())
    } else {
        Ok(())
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
    let ty = column.decl_type.to_ascii_uppercase();
    if column.is_integer() {
        SqlValue::Integer(0)
    } else if ty.contains("REAL") || ty.contains("FLOA") || ty.contains("DOUB") {
        SqlValue::Real(0.0)
    } else if ty.contains("BLOB") {
        SqlValue::Blob(Vec::new())
    } else {
        SqlValue::Text(format!("untitled-{}", short_suffix()))
    }
}

fn short_suffix() -> String {
    let count = PLACEHOLDER_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    base36(nanos ^ count).chars().take(5).collect()
}

fn base36(mut value: u64) -> String {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "00000".to_string();
    }
    let mut out = Vec::new();
    while value > 0 {
        out.push(ALPHABET[(value % 36) as usize] as char);
        value /= 36;
    }
    out.iter().rev().collect()
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn to_fs_error(err: Error) -> FsError {
    match err {
        Error::Fs(err) => err,
        Error::Yaml(_) | Error::Utf8(_) => FsError::InvalidInput,
        Error::Sql(_) | Error::Json(_) => FsError::Io,
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

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_top_level_markdown_paths() {
        assert!(matches!(parse_path("/contacts"), Ok(VPath::Table(_))));
        assert!(matches!(
            parse_path("/contacts/noorvir.md"),
            Ok(VPath::File(_))
        ));
        assert!(matches!(
            parse_path("/contacts/noorvir.txt"),
            Err(Error::Fs(FsError::NotFound))
        ));
        assert!(matches!(
            parse_path("/contacts/friends/noorvir.md"),
            Err(Error::Fs(FsError::NotDirectory))
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
    fn generic_sqlite_write_records_invalid_updates() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE contacts (
                id INTEGER PRIMARY KEY,
                _slfs_path TEXT UNIQUE NOT NULL,
                _slfs_content TEXT NOT NULL DEFAULT '',
                _slfs_invalid_update TEXT NOT NULL DEFAULT '{}',
                first_name TEXT NOT NULL DEFAULT 'untitled' CHECK(length(first_name) >= 1),
                email TEXT UNIQUE
            );",
        )
        .unwrap();
        let fs = SqliteFs {
            inner: Mutex::new(Inner {
                conn,
                handles: HashMap::new(),
                next_handle: 1,
            }),
        };

        let text =
            b"---\nfirst_name: \"\"\nemail: noorvir@example.com\nunknown: value\n---\nBody\n";
        let handle = fs
            .create("/contacts/noorvir.md", 0o644, OpenOptions::from_raw(1))
            .unwrap();
        fs.write("/contacts/noorvir.md", handle, 0, text).unwrap();
        fs.release("/contacts/noorvir.md", handle).unwrap();

        let inner = fs.inner.lock().unwrap();
        let (first_name, email, content, invalid): (String, String, String, String) = inner
            .conn
            .query_row(
                "SELECT first_name, email, _slfs_content, _slfs_invalid_update FROM contacts WHERE _slfs_path = 'noorvir.md'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        let invalid: JsonValue = serde_json::from_str(&invalid).unwrap();
        assert_eq!(first_name, "untitled");
        assert_eq!(email, "noorvir@example.com");
        assert_eq!(content, "Body\n");
        assert_eq!(invalid["first_name"]["attempted"], JsonValue::from(""));
        assert_eq!(invalid["unknown"]["attempted"], JsonValue::from("value"));
    }
}
