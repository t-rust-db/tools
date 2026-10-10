# 0045: sqlite-rs is a member of the t-rust-db/tools workspace; db-cli becomes a path dependency

Date: 2026-10-10

Amends [ADR-0040](0040-first-party-git-dependencies.md) decision 1 for
db-cli only.

## Context

t-rust-db/tools#1 moves the db-core client tools (db-cli, column-rs,
sqlite-rs, loglume, trigrep, db-studio) into one Cargo workspace,
t-rust-db/tools, with history (tools#7 for this crate). The clients had
drifted apart on db-core: each repo pinned its own tag, and changes that
span clients needed one PR and one tag bump per repo.

ADR-0040 decision 1 allows first-party crates as git dependencies "pinned
to a release tag (never a branch)". Inside the workspace, db-cli is a
sibling member.

## Decision

1. **db-cli is a path dependency** (`[workspace.dependencies] db-cli =
   { path = "db-cli" }`), still `optional` behind the `cli` feature
   (ADR-0040 decision 2, ADR-0044). The pin is the workspace commit itself:
   sqlite-rs and db-cli are built, tested and released from the same tree.
2. **db-core stays a tagged git dependency**, now pinned once for the whole
   workspace (`[workspace.dependencies]`), with this crate's own
   `default-features = false` feature list. ADR-0040 is unchanged for it.
3. **Supply-chain gates run over the workspace.** There is one `Cargo.lock`
   and one `supply-chain/` (cargo vet) at the workspace root; `make
   check-audit` reads `../Cargo.lock`. `deny.toml`, the SBOMs and every
   other gate stay in this crate. sqlite-rs keeps its own `target/`
   (`CARGO_TARGET_DIR` in the Makefile) because its gates and `tools/*.py`
   read from it.
4. **CI lives at `../.github/workflows/sqlite-rs.yml`** and runs the same
   jobs as before, scoped to `sqlite-rs/**` and the workspace files it
   builds against.

## Alternatives rejected

- **Stay a separate repository.** sqlite-rs is the only consumer of db-cli
  outside the other tools; keeping it out would leave db-cli changes
  needing a cross-repo release, and the tools repo public just for that.
- **Keep db-cli as a git dependency on the tools repo.** Same tree, two
  versions of db-cli in one lockfile; no benefit over a path dependency.

## Consequences

- `make update` updates the workspace lockfile, so it moves the other
  tools' dependencies too; review the diff accordingly.
- Some transitive versions moved to what the other tools had locked
  (patch releases of `bitflags`, `cfg-if`, `libc`, `libredox`, `rustix`,
  `unicode-ident`, `zerocopy`); the SBOMs and vet exemptions are
  regenerated against the workspace lockfile.
- `t-rust-db/sqlite-rs` is archived with a pointer to the tools repo.
