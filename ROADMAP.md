# Filesystem operation roadmap

Implementation status for the sqlite-fs/macFUSE interface.

Legend:

- [x] implemented
- [~] partially implemented
- [ ] not implemented

## Core file operations

- [x] `getattr`
- [x] `readdir`
- [x] `open`
- [x] `create`
- [x] `read`
- [x] `write`
- [x] `truncate`
- [x] `flush`
- [x] `fsync`
- [x] `release`
- [x] `unlink`
- [x] `mkdir`
- [x] `rmdir`
- [x] `opendir`
- [x] `releasedir`
- [x] `fsyncdir`

Notes:

- Typed files are SQLite rows rendered as Markdown.
- Passthrough paths delegate to the backing filesystem.
- Typed writes are staged per open handle and committed on flush/fsync/release.

## Rename

sqlite-fs has two storage domains:

```txt
typed path       -> SQLite row
passthrough path -> backing filesystem path
```

### Rename matrix

- [x] typed -> typed
- [x] passthrough -> passthrough
- [x] typed -> passthrough
- [x] passthrough -> typed
- [x] typed <-> passthrough swap
- [ ] typed <-> typed swap
- [ ] passthrough <-> passthrough swap

Notes:

- Cross-domain rename translates between SQLite rows and backing files.
- Open-handle behavior matters for editor safe-save flows.
- `RENAME_EXCL` is supported.
- Unknown rename flags are rejected.

## Metadata

- [x] `chmod`
- [x] `chown`
- [x] `utimens`
- [~] persistent typed metadata

Notes:

- Typed mtimes are tracked while mounted, not persisted.
- Typed permissions and ownership are still fixed defaults.
- Passthrough timestamp updates use host filesystem calls.

## Extended attributes

- [ ] `getxattr`
- [ ] `listxattr`
- [ ] `setxattr`
- [ ] `removexattr`

Notes:

- macOS editors probe many `com.apple.*` xattrs.
- Typed validation state may eventually be exposed through xattrs or sidecar files.

## Links

- [ ] `link`
- [ ] `symlink`
- [ ] `readlink`

Notes:

- Hard links need explicit typed/passthrough semantics.
- Symlinks should probably start as passthrough-only.

## Access and filesystem stats

- [x] `access`
- [x] `statfs`
- [ ] realistic capacity reporting
- [ ] permission-aware `access`

## Passthrough overlay

- [x] root directory merge
- [x] table directory merge
- [x] typed entries win listing conflicts
- [x] passthrough paths inside table directories
- [x] app metadata directories, e.g. `.TemporaryItems`
- [~] AppleDouble files, e.g. `._*`

Notes:

- Passthrough storage keeps editor metadata and non-typed files out of SQLite.

## Validation and typed documents

- [x] Markdown frontmatter parsing
- [x] `_slfs_content` body storage
- [x] valid property updates
- [x] invalid property storage in `_slfs_invalid_update`
- [x] unknown property handling
- [x] new row creation from Markdown
- [x] SQLite defaults
- [x] generic defaults
- [x] uniqueness/constraint recovery
- [~] multi-column constraint attribution

## Known gaps

- [ ] full POSIX permission model
- [ ] persistent typed file metadata
- [ ] xattr storage and forwarding
- [ ] link/symlink semantics
- [ ] realistic `statfs`
- [ ] binary typed files
- [ ] complete platform-specific rename flags
- [ ] broader editor compatibility test matrix
