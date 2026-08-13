# Contributing to reactor-webrtc

Thanks for taking the time to contribute. This document covers everything
you need to get a change from your machine into `main`.

## Table of contents

- [Getting set up](#getting-set-up)
- [Building and testing](#building-and-testing)
- [Code style](#code-style)
- [Commit messages](#commit-messages)
- [Developer Certificate of Origin (DCO)](#developer-certificate-of-origin-dco)
- [Opening a pull request](#opening-a-pull-request)
- [Versioning and releases](#versioning-and-releases)
- [Reporting a bug or requesting a feature](#reporting-a-bug-or-requesting-a-feature)

## Getting set up

The whole toolchain — Rust, `uv`, `ruff`, `maturin`, `cargo-nextest`,
`shellcheck` — is pinned by [mise](https://mise.jdx.dev) and locked in
`mise.lock`. Nothing else needs to be installed globally.

```bash
mise install        # installs the pinned toolchain
mise run install-hooks   # wires up pre-commit / pre-push hooks (via hk)
```

A thin `make` shim forwards to the same tasks (`make ci`, `make test`,
`make help`) at the repo root and inside each module
(`crates/reactor-webrtc-py/`, `webrtc-build/`). Run `mise tasks` (or
`mise tasks ls`) any time to see the full list.

## Building and testing

`cargo check` and `cargo build` work out of the box — `build.rs`
auto-downloads the matching prebuilt `libwebrtc` on first run. Linking a
binary, running tests, or building the Python wheel needs that prebuilt
staged locally:

```bash
export REACTOR_WEBRTC_LIB_DIR=/path/to/libwebrtc   # a packaged or extracted prebuilt

mise run ci                                  # fmt-check + clippy + repo lints
mise run test                                # cargo nextest + doctests
mise run //crates/reactor-webrtc-py:build    # build the Python wheel
mise run //crates/reactor-webrtc-py:test     # pytest against the built wheel
```

These are the exact tasks CI runs — if `mise run ci && mise run test` pass
locally, CI should too.

## Code style

- Rust: `rustfmt` (`mise run fmt`) and `clippy` with warnings denied
  (`mise run clippy`). Both run in `mise run ci` and again in CI.
- Shell: `shellcheck` over every tracked script and bash `mise-tasks`
  (`mise run lint:shell`).
- Git hooks (installed via `mise run install-hooks`, powered by
  [hk](https://github.com/jdx/hk)) run the fast subset of these on
  `pre-commit` and the heavier, compiling checks on `pre-push` — so most
  issues surface before you even open a PR.

## Commit messages

Commits follow `type(scope): summary`, e.g.:

```
feat(webrtc): let callers choose their own ICE credentials
fix(py): update .pyi stub to match frame-metadata API
chore(release): 0.6.0
```

Common types: `feat`, `fix`, `chore`, `ci`, `docs`. The scope is usually the
crate or area touched (`webrtc`, `sys`, `py`, `release`). Keep the summary
imperative and under ~72 characters; use the body to explain the *why* when
it isn't obvious from the diff.

## Developer Certificate of Origin (DCO)

Every commit must carry a `Signed-off-by` trailer certifying you wrote it
(or otherwise have the right to submit it under this project's license).
CI enforces this on every pull request.

```bash
git commit -s -m "fix(py): ..."
```

If you forgot on a commit that's already pushed:

```bash
git rebase --signoff main
git push --force-with-lease
```

## Opening a pull request

1. Branch off `main`.
2. Make your change, with tests (see [Building and testing](#building-and-testing)).
   If it touches anything documented — the root README, a crate's own
   README, or a guide under `docs/` — update that documentation and its
   code examples in the same PR, not as a follow-up. CI's `Docs freshness`
   job posts a `::warning::` annotation if a PR touches `config.rs` or the
   Python `.pyi` stub without touching any docs/README file — advisory
   only, it never blocks merging, since a path-based check can't know
   whether an update was actually needed.
3. Push and open a PR. `main` requires, before merging:
   - At least one approving review, including a review from a code owner.
   - The `CI Complete` status check passing (the aggregate of lint, tests,
     the Python wheel build/test, and the prebuilt-check jobs).
   - No unresolved review threads.
4. PRs merge via squash — one commit per PR on `main`.

## Versioning and releases

`reactor-webrtc-sys`, `reactor-webrtc`, and `reactor-webrtc-py` are always
released **in lockstep, at the same version** — there is no such thing as,
say, `reactor-webrtc-sys` at `0.8.0` while `reactor-webrtc` is still at
`0.7.2`. The version lives in exactly one place: `[workspace.package].version`
in the root `Cargo.toml`. Every crate's own `Cargo.toml` inherits it via
`version.workspace = true` — none of them declare their own `version` field.
To cut a release, bump that one field in your PR; nothing else needs a
version edit.

The internal dependency between the three crates
(`reactor-webrtc` → `reactor-webrtc-sys`, `reactor-webrtc-py` →
`reactor-webrtc`) is centralized the same way, via `[workspace.dependencies]`
in the root `Cargo.toml` — each crate depends on the sibling with
`{ workspace = true }` rather than its own `path` + `version`. This exists so
a bump can't accidentally publish (say) `reactor-webrtc@0.8.0` while its
published metadata still requires `reactor-webrtc-sys@^0.7.0` — Cargo has no
built-in way to inherit a *dependency's* version requirement from
`[workspace.package].version`, so this second table needs its version
strings bumped by hand alongside the first, but both live in the same file.

Merging that bump to `main` publishes automatically: `publish.yml` diffs the
workspace version against the previous commit, and if it changed, publishes
`reactor-webrtc-sys` + `reactor-webrtc` to crates.io and builds + publishes
the `reactor-webrtc-py` wheel to PyPI. Once all three have actually shipped,
it tags the release `vX.Y.Z` — a `v*` tag existing always means the full
release went out, never a partial one. If a merge doesn't bump the version,
nothing publishes.

No manual tagging step is needed or expected. `workflow_dispatch` on
`publish.yml` exists only for a dry run (build everything, publish nothing)
or a manual re-publish if a previous run failed partway through — crates.io
and PyPI publishing aren't atomic across packages, so a prior run can fail
after crates.io succeeded but before PyPI did; every publish step checks "is
this version already out?" first, so re-running is always safe.

## Reporting a bug or requesting a feature

Open a [GitHub issue](https://github.com/reactor-team/reactor-webrtc/issues)
with as much detail as you can: platform, Rust/Python version, and — for a
bug — a minimal repro.
