#!/usr/bin/env python3
"""Fail if any non-root mise.toml declares a [tools] table.

All tool versions must live in the repo-root mise.toml (single source of truth,
lock-verified). This scans the root config's [monorepo].config_roots (plus any
mise.toml nested under them) and errors on a [tools] or [tools.*] table anywhere
but the root. Run as //:lint:mise-tools-root.
"""

from __future__ import annotations

import sys
import tomllib
from pathlib import Path


def has_tools_table(text: str) -> bool:
    # tomllib flattens [tools.ruff] into {"tools": {"ruff": {...}}}, so a top-level
    # "tools" key catches both [tools] and [tools.*].
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        print(f"ERROR: failed to parse TOML: {e}", file=sys.stderr)
        raise
    return isinstance(data.get("tools"), dict) and bool(data["tools"])


def main() -> int:
    root = Path.cwd()
    root_cfg = root / "mise.toml"
    if not root_cfg.is_file():
        print("ERROR: no mise.toml at repo root", file=sys.stderr)
        return 1

    cfg = tomllib.loads(root_cfg.read_text())
    config_roots = cfg.get("monorepo", {}).get("config_roots", [])

    # Every mise.toml under a config_root (and the config_root itself), excluding
    # the repo-root config.
    seen: set[Path] = set()
    module_configs: list[Path] = []
    for cr in config_roots:
        base = root / cr
        for p in [base / "mise.toml", *base.rglob("mise.toml")]:
            rp = p.resolve()
            if p.is_file() and rp != root_cfg.resolve() and rp not in seen:
                seen.add(rp)
                module_configs.append(p)

    offenders = [
        p.relative_to(root)
        for p in module_configs
        if has_tools_table(p.read_text())
    ]
    if offenders:
        print("ERROR: [tools] must live only in the repo-root mise.toml.",
              file=sys.stderr)
        for o in offenders:
            print(f"  - {o} declares a [tools] table", file=sys.stderr)
        print("\nMove those pins to ./mise.toml and re-run `mise lock`.",
              file=sys.stderr)
        return 1

    print(f"✓ tools-at-root OK: {len(module_configs)} module config(s) clean")
    return 0


if __name__ == "__main__":
    sys.exit(main())
