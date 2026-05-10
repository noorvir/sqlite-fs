# sqlite-fs spec

Implementation details not covered by the README.

## Text format

Typed primitive files are UTF-8 Markdown only.

Use an explicit encoding column:

```sql
encoding TEXT NOT NULL DEFAULT 'utf-8'
```

Binary files and attachments are out of scope for typed primitives.

## POSIX write lifecycle

Do not parse/validate on every `write(2)` chunk.

Use a per-open-handle staging buffer:

```txt
open/truncate/write chunks -> staged text
flush/fsync/release        -> parse once + SQLite transaction
```

The semantic commit boundary is `flush`, `fsync`, or `release/close`.

If commit fails, report the filesystem operation as failed and roll back.

## Efficient parsing

Only the Markdown properties/frontmatter block needs structured parsing.

The rest of the file is body `content`.

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

SQLite constraints remain guardrails for bugs/races.

## Constraint recovery

For uniqueness, foreign keys, and other DB-backed checks, preflight inside the transaction.

Example:

```txt
BEGIN
check attempted email uniqueness
if unique: update contacts.email
else: store attempted email in invalid_update
COMMIT
```

If a constraint is still hit unexpectedly, use a savepoint around the risky update:

```txt
SAVEPOINT apply_field
try canonical update
on SQLITE_CONSTRAINT:
  ROLLBACK TO apply_field
  write attempted value to invalid_update
RELEASE apply_field
```

The goal is to convert semantic conflicts into `invalid_update`, not fail the whole write.

## Concurrency

Use SQLite WAL mode and a busy timeout.

Keep write transactions short:

```txt
parse outside transaction
BEGIN IMMEDIATE
preflight DB-backed constraints
apply valid updates + invalid_update
COMMIT
```

If needed, serialize filesystem commits through a single writer queue.

## Future work

- More precise multi-field / row-level validation.
- Binary file and attachment storage.
- Platform-specific xattr behavior.
