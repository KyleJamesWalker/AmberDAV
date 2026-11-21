#!/usr/bin/env bash
# Points every worktree under .claude/worktrees at the main checkout's target/, so the dependencies compile once
# for all of them. Run once from the main checkout; the setting lives in .claude/worktrees/.cargo/config.toml.
set -euo pipefail
cd "$(dirname "$0")/.."
main=$(cd "$(git rev-parse --git-common-dir)/.." && pwd)
mkdir -p "$main/.claude/worktrees/.cargo"
printf '[build]\ntarget-dir = "%s/target"\n' "$main" >"$main/.claude/worktrees/.cargo/config.toml"
echo "worktrees under $main/.claude/worktrees now build into $main/target"
