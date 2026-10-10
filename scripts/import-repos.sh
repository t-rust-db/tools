#!/usr/bin/env bash
# Import client repos into this workspace with full history, one subdirectory
# each: `git log -- column-rs/src/query.rs` keeps the old history.
#
# Clones fresh from GitHub (never from local working copies), rewrites each
# clone's paths under <name>/ with git-filter-repo, then merges it here with
# --allow-unrelated-histories. Run from the repo root, on a branch.
#
# Usage: scripts/import-repos.sh [--no-commit] <repo>...
#   --no-commit  import one repo and leave the merge staged, so the workspace
#                integration lands in the same commit as the import.
set -euo pipefail

ORG=https://github.com/t-rust-db
NO_COMMIT=()
if [ "${1:-}" = --no-commit ]; then NO_COMMIT=(--no-commit); shift; fi
[ $# -gt 0 ] || { echo "usage: $0 [--no-commit] <repo>..." >&2; exit 2; }
[ ${#NO_COMMIT[@]} -eq 0 ] || [ $# -eq 1 ] || { echo "--no-commit takes one repo" >&2; exit 2; }
[ -z "$(git status --porcelain)" ] || { echo "working tree not clean" >&2; exit 1; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

for name in "$@"; do
    [ ! -e "$name" ] || { echo "$name/ already exists, skipping" >&2; continue; }
    git clone --quiet --no-local "$ORG/$name.git" "$WORK/$name"
    (cd "$WORK/$name" && uvx git-filter-repo --quiet --force --to-subdirectory-filter "$name")
    git remote add "import-$name" "$WORK/$name"
    git fetch --quiet --no-tags "import-$name"
    git merge --quiet --allow-unrelated-histories --no-edit "${NO_COMMIT[@]}" \
        -m "chore: import $name with history from $ORG/$name" "import-$name/main"
    git remote remove "import-$name"
done
