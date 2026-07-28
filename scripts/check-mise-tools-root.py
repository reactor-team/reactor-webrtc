#!/usr/bin/env python3
"""Enforce a single [tools] table: only the repo-root mise.toml may pin tools.

Every tool version lives in the repo-root mise.toml (single source of truth,
lock-verified); the monorepo module configs (config_roots) must stay tool-free.

`mise ls --local -J` only surfaces a module's tools when it is run from *inside*
that module's directory, so this finds every tracked mise.toml and, for each
non-root one, runs mise from its directory and flags any tool whose source is
that module config. Uses mise's own resolution -- no TOML parsing.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path


def die(msg: str) -> int:
    print(f"ERROR: {msg}", file=sys.stderr)
    return 1


def git_out(args: list[str], cwd: Path | None = None) -> str:
    return subprocess.run(
        ["git", *args], cwd=cwd, capture_output=True, text=True, check=True
    ).stdout


def tracked_mise_configs(root: Path) -> list[Path]:
    raw = git_out(["ls-files", "-z", "*mise.toml"], cwd=root)
    paths = {
        (root / rel).resolve()
        for rel in raw.split("\0")
        if rel and Path(rel).name == "mise.toml"
    }
    return sorted(paths)


def tools_sourced_from(config: Path) -> list[str]:
    """Tools mise resolves from `config`, seen by running mise in its directory."""
    r = subprocess.run(
        ["mise", "ls", "--local", "-J"],
        cwd=config.parent, capture_output=True, text=True, check=False,
    )
    if r.returncode != 0:
        raise SystemExit(die(f"`mise ls --local -J` failed in {config.parent}: {r.stderr.strip()}"))
    data = json.loads(r.stdout)
    hits: list[str] = []
    for name, entries in data.items():
        for e in entries:
            src = e.get("source") or {}
            if src.get("type") == "mise.toml" and Path(src.get("path", "")).resolve() == config:
                hits.append(name)
                break
    return hits


def main() -> int:
    root = Path(git_out(["rev-parse", "--show-toplevel"]).strip())
    root_cfg = (root / "mise.toml").resolve()
    if not root_cfg.is_file():
        return die("no mise.toml at repo root")

    modules = [c for c in tracked_mise_configs(root) if c != root_cfg]

    offenders: list[tuple[Path, str]] = []
    for cfg in modules:
        for tool in tools_sourced_from(cfg):
            offenders.append((cfg.relative_to(root), tool))

    if offenders:
        print("ERROR: [tools] must be declared only in the repo-root mise.toml.",
              file=sys.stderr)
        for rel, tool in offenders:
            print(f"  - {rel} pins '{tool}'", file=sys.stderr)
        print("\nMove those pins to ./mise.toml and re-run `mise lock`.",
              file=sys.stderr)
        return 1

    print(f"✓ tools-at-root OK: {len(modules)} module config(s) clean")
    return 0


if __name__ == "__main__":
    sys.exit(main())
