use crate::error::Result;
use crate::route::DocPath;
use crate::schema::{TableSchema, quote_ident, table_schema};
use minfuse::FsError;
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use std::collections::HashMap;

pub(crate) fn render_document(conn: &Connection, doc: &DocPath) -> Result<String> {
    let schema = table_schema(conn, doc.table())?;
    let row = load_row(conn, &schema, doc.file())?.ok_or(FsError::NotFound)?;
    let mut props = JsonMap::new();
    for column in schema.domain_columns() {
        if let Some(value) = row.values.get(column.name()) {
            props.insert(column.name().to_string(), sql_to_json(value));
        }
    }
    // Invalid attempted frontmatter intentionally shadows stored canonical values.
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
        .columns()
        .iter()
        .map(|column| quote_ident(column.name()))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT {} FROM {} WHERE {} = ?1",
        selected,
        quote_ident(schema.name()),
        quote_ident("_slfs_path")
    );
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_row(params![file], |row| {
        let mut values = HashMap::new();
        for (index, column) in schema.columns().iter().enumerate() {
            values.insert(column.name().to_string(), row.get::<_, SqlValue>(index)?);
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

pub(crate) fn parse_markdown(text: &str) -> Result<(JsonMap<String, JsonValue>, String)> {
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

pub(crate) fn invalid_entry(attempted: JsonValue, error: &str) -> JsonValue {
    json!({ "attempted": attempted, "error": error })
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
