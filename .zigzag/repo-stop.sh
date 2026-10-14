#!/bin/sh
# Default worktree deletion hook. Runs before relay deletions and, when Git
# still retains a prunable entry, after manual `git worktree remove` calls.
set -eu

repo=$(CDPATH= cd "$(dirname "$0")/.." && pwd -P)
target=${1:-${ZIGZAG_WORKTREE_PATH:-}}
case "$target" in
  /*) ;;
  *) echo "refused: target is not an absolute worktree path: $target" >&2; exit 2 ;;
esac
case "/$target/" in
  */../*) echo "refused: target contains parent traversal: $target" >&2; exit 2 ;;
esac

# Prove the exact path is registered under this repository. A removed
# worktree is accepted only when the relay supplies its prior observation or
# Git still marks its retained admin entry prunable.
if [ -d "$target" ]; then
  canonical=$(CDPATH= cd "$target" 2>/dev/null && pwd -P) || {
    echo "refused: target cannot be canonicalized: $target" >&2; exit 2;
  }
  [ "$canonical" = "$target" ] || {
    echo "refused: target is not its canonical path: $target" >&2; exit 2;
  }
  top=$(git -C "$target" rev-parse --show-toplevel 2>/dev/null || true)
  [ "$top" = "$target" ] || {
    echo "refused: target is not a Git worktree root: $target" >&2; exit 2;
  }
  registered=$(git -C "$target" rev-parse --git-common-dir 2>/dev/null || true)
  case "$registered" in
    /*) common=$registered ;;
    *) common=$target/$registered ;;
  esac
  common=$(CDPATH= cd "$common" 2>/dev/null && pwd -P) || {
    echo "refused: cannot prove owning repository for $target" >&2; exit 2;
  }
  repo_common=$(git -C "$repo" rev-parse --git-common-dir 2>/dev/null || true)
  case "$repo_common" in
    /*) ;;
    *) repo_common=$repo/$repo_common ;;
  esac
  repo_common=$(CDPATH= cd "$repo_common" 2>/dev/null && pwd -P) || {
    echo "refused: cannot identify this hook repository" >&2; exit 2;
  }
  [ "$common" = "$repo_common" ] || {
    echo "refused: target belongs to another repository: $target" >&2; exit 2;
  }
else
  repo_common=$(git -C "$repo" rev-parse --git-common-dir 2>/dev/null || true)
  case "$repo_common" in
    /*) ;;
    *) repo_common=$repo/$repo_common ;;
  esac
  repo_common=$(CDPATH= cd "$repo_common" 2>/dev/null && pwd -P) || {
    echo "refused: cannot identify this hook repository" >&2; exit 2;
  }
  observed_repo=${ZIGZAG_WORKTREE_REPO:-}
  if [ "${ZIGZAG_WORKTREE_OBSERVED:-0}" = 1 ] && [ "$observed_repo" = "$repo" ]; then
    echo "verified by relay worktree observation path=$target repo=$repo"
  else
    prunable=$(git -C "$repo" worktree list --porcelain 2>/dev/null | awk -v target="$target" '
      /^worktree / { path = substr($0, 10); stale = 0 }
      /^prunable / { stale = 1 }
      /^$/ { if (path == target && stale) found = 1; path = ""; stale = 0 }
      END { if (path == target && stale) found = 1; if (found) print "yes" }
    ')
    [ "$prunable" = yes ] || {
      echo "refused: Git or relay cannot prove removed target was this repository's worktree: $target" >&2
      exit 2
    }
  fi
fi

echo "worktree-delete hook path=$target"

# These are common per-checkout caches. Every removal is a direct child of the
# proven target, so symlinks are unlinked without traversing their destinations.
if [ -d "$target" ]; then
  for name in .cache node_modules target build dist bazel-out bazel-bin bazel-testlogs; do
    item=$target/$name
    if [ -e "$item" ] || [ -L "$item" ]; then
      rm -rf -- "$item"
      echo "removed worktree cache=$item"
    fi
  done
fi

# Bazel's default output base is output_user_root/_bazel_$USER/<MD5(workspace)>.
# Use only known platform default roots. A custom output_user_root/output_base
# is intentionally left alone because this hook cannot prove its ownership.
user=${USER:-$(id -un)}
if command -v md5 >/dev/null 2>&1; then
  hash=$(printf %s "$target" | md5 -q 2>/dev/null || true)
elif command -v md5sum >/dev/null 2>&1; then
  hash=$(printf %s "$target" | md5sum | awk '{print $1}')
else
  hash=
fi
case "$hash" in
  ''|*[!0-9a-fA-F]*) echo "skipped Bazel output base: no supported MD5 utility" >&2; exit 0 ;;
esac

removed_base=0
remove_default_base() {
  output_user_root=$1
  output_base=$output_user_root/$hash
  [ "$(dirname "$output_base")" = "$output_user_root" ] || return 0
  if [ -L "$output_base" ]; then
    echo "refused Bazel output base: target is a symlink: $output_base" >&2
    return 0
  fi
  if [ -d "$output_base" ]; then
    owns_workspace=0
    for workspace_link in "$output_base"/execroot/*; do
      [ -L "$workspace_link" ] || continue
      link_target=$(readlink "$workspace_link" 2>/dev/null || true)
      if [ "$link_target" = "$target" ]; then
        owns_workspace=1
        break
      fi
      # While the worktree exists, also accept a symlink that resolves to the
      # same canonical directory. After manual removal, an unresolved link is
      # accepted only when its recorded absolute destination is an exact match.
      if [ -d "$target" ] && [ -d "$workspace_link" ]; then
        resolved_link=$(CDPATH= cd "$workspace_link" 2>/dev/null && pwd -P || true)
        [ "$resolved_link" = "$target" ] && owns_workspace=1 && break
      fi
    done
    if [ "$owns_workspace" -ne 1 ]; then
      echo "refused Bazel output base: no execroot link proves worktree ownership: $output_base" >&2
      return 0
    fi
    rm -rf -- "$output_base"
    echo "removed Bazel output base=$output_base"
    removed_base=1
  fi
}
remove_default_base "${XDG_CACHE_HOME:-$HOME/.cache}/bazel/_bazel_$user"
remove_default_base "$HOME/Library/Caches/bazel/_bazel_$user"
remove_default_base "/private/var/tmp/_bazel_$user"
[ "$removed_base" -eq 1 ] || echo "no default Bazel output base found for worktree=$target"
