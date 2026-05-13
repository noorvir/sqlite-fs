#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DB="${DB:-/tmp/sqlite-fs-manual.db}"
MP="${MP:-/Volumes/minfs-poc}"
BACKING="${BACKING:-/tmp/sqlite-fs-manual.files}"
LOG="${LOG:-/tmp/sqlite-fs-manual.log}"
PID="${PID:-/tmp/sqlite-fs-manual.pid}"
CLEAN=0
KILL_MOUNT_USERS="${KILL_MOUNT_USERS:-0}"

for arg in "$@"; do
  case "$arg" in
    --clean) CLEAN=1 ;;
    --kill-users) KILL_MOUNT_USERS=1 ;;
    *)
      echo "usage: $0 [--clean] [--kill-users]" >&2
      exit 2
      ;;
  esac
done

is_mounted() {
  mount | grep -q " on ${MP} ("
}

kill_pid() {
  local pid="$1"
  [[ -n "$pid" ]] || return 0
  [[ "$pid" =~ ^[0-9]+$ ]] || return 0
  kill "$pid" 2>/dev/null || true
}

kill_sqlite_fs() {
  if [[ -f "$PID" ]]; then
    kill_pid "$(cat "$PID" 2>/dev/null || true)"
  fi

  local escaped_root escaped_db escaped_mp pids
  escaped_root="$(printf '%s' "$ROOT" | sed 's/[.[\*^$()+?{}|]/\\&/g')"
  escaped_db="$(printf '%s' "$DB" | sed 's/[.[\*^$()+?{}|]/\\&/g')"
  escaped_mp="$(printf '%s' "$MP" | sed 's/[.[\*^$()+?{}|]/\\&/g')"
  pids="$(pgrep -f "${escaped_root}/target/debug/sqlite-fs.*(${escaped_db}|${escaped_mp})" || true)"
  if [[ -n "$pids" ]]; then
    kill $pids 2>/dev/null || true
  fi

  for _ in {1..20}; do
    pids="$(pgrep -f "${escaped_root}/target/debug/sqlite-fs.*(${escaped_db}|${escaped_mp})" || true)"
    [[ -z "$pids" ]] && return 0
    sleep 0.25
  done

  pids="$(pgrep -f "${escaped_root}/target/debug/sqlite-fs.*(${escaped_db}|${escaped_mp})" || true)"
  if [[ -n "$pids" ]]; then
    kill -9 $pids 2>/dev/null || true
  fi
}

mount_users() {
  command -v lsof >/dev/null 2>&1 || return 0
  lsof -t +D "$MP" 2>/dev/null | sort -u || true
}

unmount_mp() {
  is_mounted || return 0

  for _ in {1..8}; do
    diskutil unmount force "$MP" >/dev/null 2>&1 || umount -f "$MP" >/dev/null 2>&1 || true
    is_mounted || return 0
    sleep 0.5
  done

  if [[ "$KILL_MOUNT_USERS" == "1" ]]; then
    local users
    users="$(mount_users | grep -v "^$$$" || true)"
    if [[ -n "$users" ]]; then
      echo "Killing processes holding $MP:" >&2
      ps -p "$(echo "$users" | paste -sd, -)" -o pid=,comm= 2>/dev/null >&2 || true
      kill $users 2>/dev/null || true
      sleep 1
      kill -9 $users 2>/dev/null || true
    fi
    for _ in {1..8}; do
      diskutil unmount force "$MP" >/dev/null 2>&1 || umount -f "$MP" >/dev/null 2>&1 || true
      is_mounted || return 0
      sleep 0.5
    done
  fi

  if is_mounted; then
    echo "Still mounted: $MP" >&2
    if command -v lsof >/dev/null 2>&1; then
      echo "Processes still using mount:" >&2
      lsof +D "$MP" 2>/dev/null | head -50 >&2 || true
    fi
    return 1
  fi
}

kill_sqlite_fs
unmount_mp
rm -f "$PID"

if [[ "$CLEAN" == "1" ]]; then
  rm -f "$DB" "$DB-wal" "$DB-shm" "$LOG"
  rm -rf "$BACKING"
fi

echo "Stopped sqlite-fs"
if [[ "$CLEAN" == "1" ]]; then
  echo "Deleted runtime data"
fi
