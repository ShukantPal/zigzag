#!/bin/sh
# Install the repository's local CI hooks for this worktree.

set -eu

repo_root=$(git rev-parse --show-toplevel)
for hook in pre-commit pre-push; do
    hook_path=$(git rev-parse --git-path "hooks/$hook")
    mkdir -p "$(dirname "$hook_path")"
    cp "$repo_root/scripts/githooks/$hook" "$hook_path"
    chmod +x "$hook_path"
    echo "Installed $hook hook at $hook_path"
done
