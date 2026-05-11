#!/usr/bin/env python3
"""Mounted e2e test for the generic SQLite-backed filesystem."""

import argparse
import json
import pathlib
import shlex
import sqlite3
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
BIN = ROOT / "target" / "debug" / "sqlite-fs"
DB = pathlib.Path("/tmp/sqlite-fs-poc.db")
LOG = pathlib.Path("/tmp/sqlite-fs-poc.log")


def run(cmd, *, check=True, quiet=False, timeout=60):
    cmd = [str(c) for c in cmd]
    if not quiet:
        print("$", " ".join(shlex.quote(c) for c in cmd), flush=True)
    p = subprocess.run(cmd, cwd=ROOT, text=True, capture_output=True, timeout=timeout)
    if not quiet:
        if p.stdout:
            print(p.stdout, end="")
        if p.stderr:
            print(p.stderr, end="", file=sys.stderr)
        print(f"rc {p.returncode}")
    if check and p.returncode != 0:
        raise RuntimeError("command failed: " + " ".join(cmd))
    return p


def is_mounted(mountpoint):
    lines = run(["mount"], check=False, quiet=True, timeout=5).stdout.splitlines()
    return any(f" on {mountpoint} (" in line for line in lines)


def init_db(path):
    path.unlink(missing_ok=True)
    db = sqlite3.connect(path)
    db.executescript(
        """
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
        """
    )
    db.commit()
    db.close()


def db_row(path):
    db = sqlite3.connect(path)
    row = db.execute(
        "SELECT first_name, email, _slfs_content, _slfs_invalid_update "
        "FROM contacts WHERE _slfs_path = 'noorvir.md'"
    ).fetchone()
    db.close()
    return row


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--mountpoint", default="/Volumes/minfs-poc")
    parser.add_argument("--db", default=str(DB))
    args = parser.parse_args()

    mountpoint = pathlib.Path(args.mountpoint)
    db_path = pathlib.Path(args.db)
    if not mountpoint.exists():
        raise SystemExit(f"Mountpoint does not exist: {mountpoint}")
    if is_mounted(mountpoint):
        raise SystemExit(f"Already mounted: {mountpoint}")

    init_db(db_path)
    run(["cargo", "build", "-p", "sqlite-fs", "--bin", "sqlite-fs"], timeout=180)
    LOG.unlink(missing_ok=True)

    cmd = [str(BIN), "--db", db_path, mountpoint]
    print("$", " ".join(shlex.quote(str(c)) for c in cmd), flush=True)
    with LOG.open("w") as log:
        proc = subprocess.Popen([str(c) for c in cmd], cwd=ROOT, stdout=log, stderr=subprocess.STDOUT)

    try:
        for _ in range(80):
            if proc.poll() is not None:
                raise RuntimeError(f"mount exited rc={proc.returncode}\n{LOG.read_text(errors='ignore')}")
            if is_mounted(mountpoint):
                break
            time.sleep(0.25)
        else:
            raise RuntimeError(f"mount did not appear\n{LOG.read_text(errors='ignore')}")

        root = run(["ls", mountpoint]).stdout.split()
        if root != ["books", "contacts"]:
            raise RuntimeError(f"unexpected root listing: {root!r}")

        run([
            "sh",
            "-c",
            'cat > "$1/contacts/noorvir.md" <<EOF\n---\nfirst_name: Noorvir\nemail: noorvir@example.com\nunknown: value\n---\nHello\nEOF',
            "sh",
            mountpoint,
        ])
        first_name, email, content, invalid = db_row(db_path)
        invalid = json.loads(invalid)
        assert first_name == "Noorvir"
        assert email == "noorvir@example.com"
        assert content == "Hello\n"
        assert invalid["unknown"]["attempted"] == "value"

        run([
            "sh",
            "-c",
            'cat > "$1/contacts/noorvir.md" <<EOF\n---\nfirst_name: ""\nemail: noorvir2@example.com\n---\nUpdated\nEOF',
            "sh",
            mountpoint,
        ])
        first_name, email, content, invalid = db_row(db_path)
        invalid = json.loads(invalid)
        assert first_name == "Noorvir"
        assert email == "noorvir2@example.com"
        assert content == "Updated\n"
        assert invalid["first_name"]["attempted"] == ""
        assert "unknown" not in invalid

        rendered = run(["cat", mountpoint / "contacts" / "noorvir.md"]).stdout
        if "Updated\n" not in rendered or "noorvir2@example.com" not in rendered:
            raise RuntimeError(f"unexpected rendered document: {rendered!r}")

        result = run(["sh", "-c", 'printf x > "$1/contacts/ignored.txt"', "sh", mountpoint], check=False)
        if result.returncode == 0:
            raise RuntimeError("non-Markdown file unexpectedly succeeded")

        print("PASS: generic SQLite-backed Markdown filesystem works through mounted FSKit")
    finally:
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
        for _ in range(80):
            if not is_mounted(mountpoint):
                break
            time.sleep(0.25)
        if LOG.exists() and LOG.read_text(errors="ignore"):
            print("--- sqlite-fs log ---")
            print(LOG.read_text(errors="ignore"))


if __name__ == "__main__":
    main()
