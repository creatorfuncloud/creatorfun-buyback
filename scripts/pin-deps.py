#!/usr/bin/env python3
"""Produce a Cargo.lock that the Solana 2.1 toolchain (Rust 1.79) can build.

Some newer crates use Rust edition 2024 without declaring a rust-version, so Cargo's
MSRV-aware resolver cannot skip them. This script runs `cargo fetch`, and whenever a
crate manifest cannot be parsed it pins that crate (or, if needed, the crate that
depends on it) to the newest older version that is edition 2021 or older and supports
Rust 1.79. Only the lock file changes; Cargo.toml requirements stay as they are.

Run from the repository root with Cargo 1.84:  python3 scripts/pin-deps.py
"""
import json
import re
import subprocess
import sys
import tomllib
import urllib.request

MSRV = (1, 79)
UA = {"User-Agent": "creatorfun-buyback pin-deps (https://github.com/creatorfuncloud/creatorfun-buyback)"}
_cache = {}


def vkey(v):
    core = v.split("+")[0]
    if "-" in core:
        return None  # skip pre-releases
    return tuple(int(x) for x in core.split("."))


def good_versions(name):
    """Non-yanked releases of `name`, newest first, usable with Rust 1.79 / edition <= 2021."""
    if name not in _cache:
        req = urllib.request.Request(f"https://crates.io/api/v1/crates/{name}/versions", headers=UA)
        out = []
        for v in json.load(urllib.request.urlopen(req, timeout=30))["versions"]:
            if v["yanked"] or vkey(v["num"]) is None:
                continue
            if v.get("edition") == "2024":
                continue
            rv = v.get("rust_version")
            if rv and tuple(int(x) for x in (rv.split(".") + ["0"])[:2]) > MSRV:
                continue
            out.append(v["num"])
        out.sort(key=vkey, reverse=True)
        _cache[name] = out
    return _cache[name]


def cargo(*args):
    return subprocess.run(["cargo", *args], capture_output=True, text=True)


def downgrade(name, ver):
    """Pin name@ver to the newest usable older version. True on success."""
    for cand in good_versions(name):
        if vkey(cand) >= vkey(ver):
            continue
        r = cargo("update", "-p", f"{name}@{ver}", "--precise", cand)
        if r.returncode == 0:
            print(f"  pinned {name} {ver} -> {cand}")
            return True
    return False


def parents(name, ver):
    with open("Cargo.lock", "rb") as f:
        lock = tomllib.load(f)
    out = []
    for p in lock.get("package", []):
        for d in p.get("dependencies", []):
            parts = d.split(" ")
            if parts[0] == name and (len(parts) == 1 or parts[1] == ver):
                out.append((p["name"], p["version"]))
    return out


def main():
    if cargo("generate-lockfile").returncode != 0:
        sys.exit("cargo generate-lockfile failed")
    for _ in range(80):
        r = cargo("fetch")
        if r.returncode == 0:
            print("OK: every dependency can be read by Rust 1.79 tooling.")
            return
        m = re.search(r"registry/src/[^/]+/([A-Za-z0-9_\-]+?)-(\d+\.\d+\.\d+[^/]*)/Cargo\.toml", r.stderr)
        if not m:
            print(r.stderr[-3000:])
            sys.exit("Unexpected cargo error (shown above).")
        name, ver = m.group(1), m.group(2)
        print(f"{name} {ver} needs a newer Rust")
        if downgrade(name, ver):
            continue
        if not any(downgrade(pn, pv) for pn, pv in parents(name, ver)):
            sys.exit(f"Could not find usable versions around {name} {ver}.")
    sys.exit("Gave up after 80 steps.")


if __name__ == "__main__":
    main()
