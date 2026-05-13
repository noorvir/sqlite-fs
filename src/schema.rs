use crate::error::Result;
use minfuse::FsError;
use rusqlite::Connection;
use rusqlite::types::Value as SqlValue;
use serde_json::Value as JsonValue;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

static PLACEHOLDER_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub(crate) struct TableSchema {
    name: String,
    columns: Vec<Column>,
}

#[derive(Debug, Clone)]
pub(crate) struct Column {
    name: String,
    decl_type: String,
    not_null: bool,
    default_value: Option<String>,
    pk: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnAffinity {
    Text,
    Integer,
    Real,
    Numeric,
    Blob,
}

pub(crate) fn eligible_tables(conn: &Connection) -> Result<Vec<String>> {
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

pub(crate) fn table_schema(conn: &Connection, table: &str) -> Result<TableSchema> {
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
    // NotFound here means either the table is absent or it is not an sqlite-fs typed table.
    if schema.has_required_columns() && path_is_unique(conn, schema.name())? {
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

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub(crate) fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|column| column.name == name)
    }

    pub(crate) fn domain_columns(&self) -> Vec<&Column> {
        self.columns
            .iter()
            .filter(|column| !column.name.starts_with("_slfs_"))
            .collect()
    }
}

impl Column {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn needs_insert_value(&self) -> bool {
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

    pub(crate) fn property_type_error(&self, value: &JsonValue) -> Option<&'static str> {
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

pub(crate) fn validate_ident(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 255
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if valid {
        Ok(())
    } else {
        Err(FsError::InvalidInput.into())
    }
}

fn is_json_integer(value: &JsonValue) -> bool {
    value
        .as_i64()
        .map(|_| true)
        .or_else(|| value.as_u64().map(|n| i64::try_from(n).is_ok()))
        .unwrap_or(false)
}

pub(crate) fn json_to_sql(value: &JsonValue) -> SqlValue {
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

pub(crate) fn generic_default(column: &Column) -> SqlValue {
    match column.affinity() {
        ColumnAffinity::Integer => SqlValue::Integer(0),
        ColumnAffinity::Real | ColumnAffinity::Numeric => SqlValue::Real(0.0),
        ColumnAffinity::Blob => SqlValue::Blob(Vec::new()),
        ColumnAffinity::Text => SqlValue::Text(format!("untitled-{}", short_suffix())),
    }
}

pub(crate) fn short_suffix() -> String {
    let count = PLACEHOLDER_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}-{:x}-{count:x}", std::process::id(), nanos)
}

pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
