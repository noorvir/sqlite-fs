# sqlite-fs

A POSIX-like filesystem whose files are backed by SQLite rows.

## Core model

For selected typed primitives, store valid state and invalid attempted updates in the same row.

Example:

```sql
contacts(
  id TEXT PRIMARY KEY,
  _slfs_path TEXT UNIQUE NOT NULL,
  _slfs_content TEXT NOT NULL DEFAULT '',
  _slfs_invalid_update TEXT NOT NULL DEFAULT '{}',

  first_name TEXT NOT NULL CHECK(length(first_name) >= 1),
  last_name  TEXT NOT NULL CHECK(length(last_name) >= 1),
  email      TEXT
)
```

## Filesystem contract

```txt
read(path) = render(properties, _slfs_invalid_update, _slfs_content)
write(path, text) = atomic SQLite transaction
```

The filesystem does not need to preserve exact Markdown bytes. It must preserve the semantic fields and the body content.

Properties are parsed into canonical columns plus `_slfs_invalid_update`. The body of the document is stored separately in `_slfs_content` and can be preserved exactly.

There are no separate Markdown files on disk. The userspace filesystem stores file state in SQLite.

A successful filesystem write means the SQLite transaction committed. If SQLite cannot commit
(disk full, lock timeout, corruption, etc.), the filesystem write fails and the transaction rolls back.

Semantic validation failure is not a filesystem write failure:

```txt
invalid property value -> write succeeds, invalid_update updated, content preserved
SQLite persistence error -> write fails, no partial commit
```

## Write flow

On file write:

1. Begin SQLite transaction.
2. Parse Markdown properties and body content.
3. Store the body in `_slfs_content`.
4. Valid properties update canonical columns.
5. Invalid properties are stored in `_slfs_invalid_update`.
6. Fields that become valid are removed from `_slfs_invalid_update`.
7. Commit everything atomically.

Example attempted write:

```yaml
first_name: 1234
last_name: ""
email: ada@new.com
```

Result:

```txt
contacts.first_name = 'Ada'          -- unchanged
contacts.last_name  = 'Lovelace'     -- unchanged
contacts.email      = 'ada@new.com'  -- applied

contacts._slfs_invalid_update = {
  "first_name": { "attempted": 1234, "error": "must be text" },
  "last_name":  { "attempted": "",   "error": "cannot be empty" }
}
```

## New rows

New writes can use valid defaults so the row is always insertable.

For example, a new invalid contact can be inserted with placeholder canonical values plus `_slfs_invalid_update` containing the user’s attempted invalid fields.

If defaults are semantic placeholders rather than real data, track that explicitly, e.g. with `is_placeholder` or by keeping the relevant field in `_slfs_invalid_update` until the user supplies a valid value.

## Error visibility

Do not mutate file contents to show errors.

Expose validation state through SQLite, filesystem metadata, or sidecar virtual files, e.g.:

```txt
.dbfs/errors/Contacts/Ada.md.json
xattr user.sqlite_fs.validation Contacts/Ada.md
```
