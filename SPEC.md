# sqlite-fs spec

Implementation details not covered by the README.

## Text format

Typed primitive files are UTF-8 Markdown only.

Expose only top-level Markdown files:

```txt
/{table}/{file}.md
```

Nested typed paths and non-`.md` typed files are out of scope and should route to passthrough storage.

The root lists eligible tables merged with passthrough root entries. A table folder lists rows by `_slfs_path` merged with passthrough entries.

A table is exposed as a folder when it has:

```sql
_slfs_path TEXT UNIQUE NOT NULL,
_slfs_content TEXT NOT NULL,
_slfs_invalid_update TEXT NOT NULL
```

`_slfs_path` must be globally unique; partial unique indexes do not qualify. Defaults for `_slfs_content` and `_slfs_invalid_update` are optional because sqlite-fs writes both internal values explicitly when it creates a row.

The folder name is the table name. `_slfs_path` is the filename inside it.

`_slfs_` is reserved for sqlite-fs metadata. Do not expose those columns as editable properties.

All non-`_slfs_` columns are domain properties. Rust must not contain domain-specific names.

## POSIX write lifecycle

Do not parse/validate on every `write(2)` chunk.

Use a per-open-handle staging buffer:

```txt
open/truncate/write chunks -> staged text
flush/fsync/release        -> parse once + SQLite transaction
```

The semantic commit boundary is `flush`, `fsync`, or `release/close`.

Reads through the same open handle may see staged bytes. Other handles see committed state.

If commit fails, report the filesystem operation as failed and roll back.

## Efficient parsing

Only the Markdown properties/frontmatter block needs structured parsing.

The rest of the file is body `_slfs_content`.

Future optimization:

```txt
if property_hash unchanged:
  update content only
  skip validation
```

## Validation before constraints

Do not rely on SQLite constraints as the main validation path.

Before updating canonical columns:

```txt
candidate = current row + attempted properties
run validation hooks
split into valid properties + invalid properties
```

Only valid values should be written to constrained canonical columns.

Invalid values are stored in `_slfs_invalid_update`.

Unknown properties are invalid updates too.

SQLite constraints remain guardrails for bugs/races.

## Constraint recovery

For uniqueness, foreign keys, and other DB-backed checks, preflight inside the transaction.

Example:

```txt
BEGIN
check attempted unique field
if unique: update canonical column
else: store attempted value in _slfs_invalid_update
COMMIT
```

If a single-column constraint is still hit unexpectedly, use a savepoint around that update and write the attempted value to `_slfs_invalid_update`.

For multiple canonical columns, apply the candidate as one row update. If that grouped update hits a DB-backed semantic constraint that cannot be attributed safely, roll back the grouped update, leave canonical columns unchanged, and record the attempted fields in `_slfs_invalid_update` conservatively.

The goal is to convert semantic conflicts into `_slfs_invalid_update`, not fail the whole write or apply fields through invalid intermediate rows.

## New rows

Use provided Markdown values first, then SQLite defaults, then generic type defaults.

Generic type defaults are best effort: text -> `untitled-a1b2c`, number -> `0`, blob -> empty.

Visible generated placeholders should be human-ish, never `_slfs_`-prefixed.

Schemas with CHECK constraints should provide compatible defaults.

If SQL constraints still reject insertion, the filesystem write fails.

## Concurrency

Use SQLite WAL mode and a busy timeout.

Keep write transactions short:

```txt
parse outside transaction
BEGIN IMMEDIATE
preflight DB-backed constraints
apply valid updates + _slfs_invalid_update
COMMIT
```

If needed, serialize filesystem commits through a single writer queue.

## Unsupported operations

Unsupported POSIX operations return ordinary errors such as `ENOSYS`; the mount must not crash.

Panics and malformed callback input must be converted to ordinary filesystem errors.

## Passthrough storage

The mount is an overlay of typed SQLite rows and a normal backing directory.

Typed route:

```txt
/{table}/{file}.md
```

A path is typed only when `{table}` is an eligible table with the required `_slfs_` columns.

All other paths use passthrough storage, including:

```txt
/.obsidian/**
/assets/**
/{table}/nested/**
/{table}/non-md-file
```

Passthrough should use host filesystem semantics where possible: directories, binary files, app metadata, rename, delete, and arbitrary nesting.

Directory listings merge both sources. If both sources contain the same visible path, the typed SQLite row wins.

Creates/writes to typed paths must update SQLite, not the passthrough directory.

Creates/writes to non-typed paths must update passthrough storage, not SQLite.

## Future work

- More precise multi-field / row-level validation.
- Binary file and attachment storage.
- Platform-specific xattr behavior.
