#!/bin/sh
# Install the repository's convenience hooks for this worktree.

set -eu

repo_root=$(git rev-parse --show-toplevel)
hook_path=$(git rev-parse --git-path hooks/pre-commit)
mkdir -p "$(dirname "$hook_path")"
cp "$repo_root/scripts/githooks/pre-commit" "$hook_path"
chmod +x "$hook_path"
echo "Installed pre-commit hook at $hook_path"
