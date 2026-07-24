#!/usr/bin/env python3
"""Verify mise.lock is complete against mise.toml.

For every tool in mise.toml [tools], assert mise.lock has a version-matching
[[tools.<name>]] entry covering every platform in [settings].lockfile_platforms
(falling back to the lockfile's own platform union when the setting is absent).

A `latest` spec (only rustup uses one) is matched by presence, not version — the
lock still pins a concrete version, and platform coverage is still enforced.

Tools whose backend has no per-platform binary artifacts (go:, npm:, cargo:,
pipx:) get a lock entry with no platforms.* sub-tables; skip their coverage.

MISE_LOCK_CHECK_FROM selects the source: "worktree" (default, files on disk),
"index" (staged, pre-commit), "head" (committed, pre-push/CI). The check is
skipped when the selected source matches the merge-base with BASE_REF
(default origin/main), so unrelated commits aren't blocked by local edits.
"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - py<3.11
    import tomli as tomllib  # type: ignore

BASE_REF = os.environ.get("BASE_REF", "origin/main")
SOURCE = os.environ.get("MISE_LOCK_CHECK_FROM", "worktree")
_VALID_SOURCES = ("worktree", "index", "head")
# Backends with no per-platform binary artifacts (mise emits no platforms.*).
_NO_PLATFORM_BACKENDS = ("go:", "npm:", "cargo:", "pipx:")

# Tools that legitimately ship no binary for a given platform, so `mise lock`
# cannot (and never will) produce an entry for it. Exempt the specific pair
# rather than dropping the platform for every tool. Keep this list tiny and
# justified; a stale exemption silently hides a real coverage gap.
_PLATFORM_EXEMPTIONS: dict[str, set[str]] = {
    # hk publishes only aarch64-apple-darwin for macOS (no x86_64); an Intel Mac
    # falls back to `cargo install hk`. https://github.com/jdx/hk/releases
    "hk": {"macos-x64"},
}


def die(msg: str) -> None:
    print(f"ERROR: {msg}", file=sys.stderr)
    sys.exit(1)


def git_show(spec: str) -> bytes | None:
    r = subprocess.run(["git", "show", spec], capture_output=True, check=False)
    return r.stdout if r.returncode == 0 else None


def read_source(path: str) -> bytes:
    if SOURCE == "worktree":
        return Path(path).read_bytes()
    spec = f":{path}" if SOURCE == "index" else f"HEAD:{path}"
    content = git_show(spec)
    if content is None:
        die(f"git show {spec}: file not found or not a git repo")
    return content


def parse_toml(data: bytes, label: str) -> dict:
    try:
        return tomllib.loads(data.decode("utf-8"))
    except tomllib.TOMLDecodeError as e:
        die(f"failed to parse {label}: {e}")


def platforms_in(entry: dict) -> set[str]:
    return {k.removeprefix("platforms.") for k in entry if k.startswith("platforms.")}


def version_matches(spec: str, locked: str) -> bool:
    return (locked + ".").startswith(spec + ".")


def main() -> int:
    if SOURCE not in _VALID_SOURCES:
        die(f"MISE_LOCK_CHECK_FROM must be one of {_VALID_SOURCES}, got '{SOURCE}'")

    os.chdir(os.environ.get("MISE_LOCK_CHECK_ROOT", "."))

    if SOURCE == "worktree":
        for f in ("mise.toml", "mise.lock"):
            if not Path(f).is_file():
                die(f"{f} not found in {Path.cwd()}")

    toml_bytes = read_source("mise.toml")
    lock_bytes = read_source("mise.lock")

    base = subprocess.run(
        ["git", "merge-base", "HEAD", BASE_REF],
        capture_output=True, text=True, check=False,
    )
    if base.returncode == 0:
        base_sha = base.stdout.strip()
        if (git_show(f"{base_sha}:mise.toml") == toml_bytes
                and git_show(f"{base_sha}:mise.lock") == lock_bytes):
            print(f"✓ mise.lock check skipped: no changes vs {BASE_REF}")
            return 0

    toml_data = parse_toml(toml_bytes, "mise.toml")
    lock_data = parse_toml(lock_bytes, "mise.lock")

    declared = toml_data.get("tools", {})
    if not declared:
        die("mise.toml has no [tools] section")
    locked = lock_data.get("tools", {})

    configured = toml_data.get("settings", {}).get("lockfile_platforms")
    if configured is not None:
        expected = set(configured)
    else:
        expected = set()
        for entries in locked.values():
            for entry in entries:
                expected |= platforms_in(entry)

    errors: list[str] = []
    for tool, spec in declared.items():
        version = spec.get("version", "") if isinstance(spec, dict) else spec
        entries = locked.get(tool, [])
        if str(version) == "latest":
            # rustup: version floats but is still locked concretely; match by presence.
            match = entries[0] if entries else None
        else:
            match = next(
                (e for e in entries
                 if version_matches(str(version), str(e.get("version", "")))),
                None,
            )
        if match is None:
            found = ", ".join(e.get("version", "?") for e in entries) or "none"
            errors.append(f"{tool}@{version}: no matching entry in mise.lock (found: {found})")
            continue
        backend = match.get("backend", "")
        if backend.startswith(_NO_PLATFORM_BACKENDS):
            continue
        missing = expected - platforms_in(match) - _PLATFORM_EXEMPTIONS.get(tool, set())
        if missing:
            errors.append(
                f"{tool}@{match.get('version')}: missing platforms: {','.join(sorted(missing))}"
            )

    if errors:
        print("mise.lock is out of date:", file=sys.stderr)
        for e in errors:
            print(f"  - {e}", file=sys.stderr)
        print("\nRun: mise lock", file=sys.stderr)
        return 1

    print(f"✓ mise.lock OK: {len(declared)} tool(s), {len(expected)} platform(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
