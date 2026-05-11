#!/usr/bin/env python3
"""Build and run the minimal Rust macFUSE FSKit POC.

Prereqs:
  brew install --cask macfuse
  /Library/Filesystems/macfuse.fs/Contents/Resources/macfuse.app/Contents/MacOS/macfuse install --force
  # Enable macFUSE under System Settings > General > Login Items & Extensions > File System Extensions
  sudo mkdir -p /Volumes/minfs-poc
  sudo chown "$USER":staff /Volumes/minfs-poc

Run:
  python3 scripts/minfs_poc.py
"""

import argparse
import pathlib
import shlex
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
BIN = ROOT / "target" / "debug" / "sqlite-fs"
LOG = pathlib.Path("/tmp/minfs-poc.log")


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


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--mountpoint", default="/Volumes/minfs-poc")
    args = parser.parse_args()

    mountpoint = pathlib.Path(args.mountpoint)
    if not mountpoint.exists():
        raise SystemExit(
            f"Mountpoint does not exist: {mountpoint}\n"
            f"Create it with:\n"
            f"  sudo mkdir -p {mountpoint}\n"
            f"  sudo chown $USER:staff {mountpoint}"
        )
    if is_mounted(mountpoint):
        raise SystemExit(f"Already mounted: {mountpoint}")

    run(["cargo", "build", "-p", "sqlite-fs", "--bin", "sqlite-fs"], timeout=180)
    LOG.unlink(missing_ok=True)

    cmd = [str(BIN), str(mountpoint)]
    print("$", " ".join(shlex.quote(c) for c in cmd), flush=True)
    with LOG.open("w") as log:
        proc = subprocess.Popen(cmd, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT)

    try:
        for _ in range(80):
            if proc.poll() is not None:
                raise RuntimeError(f"mount exited rc={proc.returncode}\n{LOG.read_text(errors='ignore')}")
            if is_mounted(mountpoint):
                break
            time.sleep(0.25)
        else:
            raise RuntimeError(f"mount did not appear\n{LOG.read_text(errors='ignore')}")

        run(["cat", mountpoint / "hello.txt"])
        run(["sh", "-c", 'printf "written through mount\\n" > "$1/hello.txt"', "sh", mountpoint])
        got = run(["cat", mountpoint / "hello.txt"]).stdout
        if got != "written through mount\n":
            raise RuntimeError(f"unexpected content: {got!r}")
        print("PASS: Rust macFUSE FSKit read/write works without the macFUSE kext")
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
            print("--- minfs log ---")
            print(LOG.read_text(errors="ignore"))


if __name__ == "__main__":
    main()
