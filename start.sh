#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DB="${DB:-/tmp/sqlite-fs-manual.db}"
MP="${MP:-/Volumes/minfs-poc}"
LOG="${LOG:-/tmp/sqlite-fs-manual.log}"
PID="${PID:-/tmp/sqlite-fs-manual.pid}"

is_mounted() {
  mount | grep -q " on ${MP} ("
}

if [[ ! -d "$MP" ]]; then
  echo "Mountpoint does not exist: $MP" >&2
  echo "Create it with:" >&2
  echo "  sudo mkdir -p '$MP'" >&2
  echo "  sudo chown \"$USER\":staff '$MP'" >&2
  exit 1
fi

if is_mounted; then
  echo "Already mounted: $MP"
  exit 0
fi

rm -f "$DB"
sqlite3 "$DB" <<'SQL'
CREATE TABLE contacts (
  _slfs_path TEXT UNIQUE NOT NULL,
  _slfs_content TEXT NOT NULL DEFAULT '',
  _slfs_invalid_update TEXT NOT NULL DEFAULT '{}',

  first_name TEXT NOT NULL DEFAULT 'untitled' CHECK(length(first_name) >= 1),
  email TEXT UNIQUE CHECK(email IS NULL OR instr(email, '@') > 1)
);

CREATE TABLE books (
  _slfs_path TEXT UNIQUE NOT NULL,
  _slfs_content TEXT NOT NULL DEFAULT '',
  _slfs_invalid_update TEXT NOT NULL DEFAULT '{}',

  title TEXT NOT NULL DEFAULT 'untitled'
);

INSERT INTO contacts (_slfs_path, _slfs_content, first_name, email)
VALUES ('noorvir.md', 'Edit this body through the mounted filesystem.
', 'Noorvir', 'noorvir@example.com');
SQL

cargo build -p sqlite-fs --bin sqlite-fs

rm -f "$LOG"
"$ROOT/target/debug/sqlite-fs" --db "$DB" "$MP" >"$LOG" 2>&1 &
echo "$!" > "$PID"

for _ in {1..80}; do
  if ! kill -0 "$(cat "$PID")" 2>/dev/null; then
    echo "sqlite-fs exited while mounting. Log:" >&2
    cat "$LOG" >&2 || true
    exit 1
  fi
  if is_mounted; then
    echo "Mounted sqlite-fs"
    echo "  mountpoint: $MP"
    echo "  database:   $DB"
    echo "  log:        $LOG"
    echo "  pid:        $(cat "$PID")"
    echo
    echo "Try:"
    echo "  ls '$MP'"
    echo "  cat '$MP/contacts/noorvir.md'"
    echo "  open '$MP/contacts'"
    echo "  sqlite3 '$DB' 'SELECT _slfs_path, first_name, email, _slfs_content, _slfs_invalid_update FROM contacts;'"
    echo
    echo "Stop:"
    echo "  kill $(cat "$PID")"
    exit 0
  fi
  sleep 0.25
done

echo "Timed out waiting for mount. Log:" >&2
cat "$LOG" >&2 || true
exit 1
