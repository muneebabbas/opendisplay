#!/usr/bin/env bash
# Bring upstream into the fork. Run from the code checkout
# (~/projects/opendisplay), clean tree.
#
#   ~/projects/opendisplay-fork/tools/sync-upstream.sh [--push]
#
# Fork model (2026-11-10): `main` is the fork's own branch, the one builds come
# from: upstream merged in, plus our topic branches merged through fork PRs
# (merge commits, no squash). Upstream is followed through `upstream/main`.
#
# 1. Merges upstream/main into main (a merge commit when they diverged).
# 2. Rebases each *open* topic stack onto upstream/main, so the branches stay
#    clean starting points for upstream PRs. STACKS lists the top branch of
#    each stack; `--update-refs` moves the branches underneath along with it.
#    Remove a stack from STACKS once it is merged into main: rebasing branches
#    that main already contains would duplicate their commits on the next
#    merge.
#
# Upstream PRs: `git switch -c up/<name> upstream/main`, cherry-pick the piece,
# open the PR from that branch. Never from main.
set -euo pipefail

STACKS=(
  linux/dmabuf-capture
)

push=0
[ "${1:-}" = "--push" ] && push=1

if [ -n "$(git status --porcelain)" ]; then
  echo "working tree not clean; commit or stash first" >&2
  exit 1
fi
start="$(git branch --show-current)"

git fetch upstream
git fetch origin
git checkout -q main
git merge --ff-only origin/main
git merge --no-edit upstream/main
echo "main -> $(git log --oneline -1)"

for top in "${STACKS[@]}"; do
  echo "rebasing ${top} (and branches below it) onto upstream/main"
  git rebase --update-refs upstream/main "${top}"
done

git checkout -q "${start}"

if [ "${push}" = 1 ]; then
  git push origin main
  for top in "${STACKS[@]}"; do
    # Push every branch of the stack that the rebase may have moved.
    for b in $(git for-each-ref --format='%(refname:short)' --contains upstream/main refs/heads/linux/); do
      if git merge-base --is-ancestor "${b}" "${top}"; then
        git push --force-with-lease origin "${b}"
      fi
    done
  done
else
  echo "done locally; re-run with --push to update the fork on GitHub"
fi
