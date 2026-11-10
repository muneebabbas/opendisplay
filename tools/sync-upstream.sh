#!/usr/bin/env bash
# Fast-forward the fork's `main` to upstream and rebase the topic branches
# onto it. Run from the code checkout (~/projects/opendisplay), clean tree.
#
#   ~/projects/opendisplay-fork/tools/sync-upstream.sh [--push]
#
# STACKS lists the *top* branch of each stack; `--update-refs` moves the
# branches underneath along with it (e.g. listing linux/dmabuf-capture also
# rebases linux/vaapi-gpu-convert and linux/vaapi-encoder below it).
# Add new topic branches here as they are created.
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
git merge --ff-only upstream/main
echo "main -> $(git log --oneline -1)"

for top in "${STACKS[@]}"; do
  echo "rebasing ${top} (and branches below it) onto main"
  git rebase --update-refs main "${top}"
done

git checkout -q "${start}"

if [ "${push}" = 1 ]; then
  git push origin main
  for top in "${STACKS[@]}"; do
    # Push every branch of the stack that the rebase may have moved.
    for b in $(git for-each-ref --format='%(refname:short)' --contains main refs/heads/linux/); do
      if git merge-base --is-ancestor "${b}" "${top}"; then
        git push --force-with-lease origin "${b}"
      fi
    done
  done
else
  echo "done locally; re-run with --push to update the fork on GitHub"
fi
