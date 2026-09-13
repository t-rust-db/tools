# ADR-0001: Bump MSRV to 1.88 for ratatui 0.30

## Status

Accepted

## Context

Issue #12 (TUI mode) adds `ratatui` as a dependency. The latest stable
release, `ratatui` v0.30.2, requires rustc >= 1.88. loglume's `Cargo.toml`
pinned `rust-version = "1.75"`, which caused `cargo add ratatui` to back off
and resolve the older `ratatui` v0.29.0 (paired with `crossterm` v0.29.0)
instead, with a warning that 0.30.2 was ignored to preserve the 1.75 floor.

## Decision

Bump `rust-version` from `"1.75"` to `"1.88"` in `Cargo.toml`, and take
`ratatui` v0.30.2 (with `crossterm` v0.29.0, its paired terminal backend)
rather than staying on the older `ratatui` v0.29.0 to preserve the lower
MSRV.

## Consequences

- Building loglume from source now requires Rust 1.88 or newer.
- CI (`.github/workflows/ci.yml`) already installs `stable` via
  `dtolnay/rust-toolchain`, so this doesn't affect CI; it only affects
  contributors/users building with an older pinned toolchain.
- No other dependency in the tree required the bump; this is scoped
  entirely to taking the current `ratatui` release.
