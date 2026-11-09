# `fork` branch: notes, plan and tools for this fork

This is an **orphan branch**: it shares no history with upstream
(peetzweg/opendisplay) and is never merged into anything. It holds what only
this fork needs: the plan, handoff notes, research and measurements, and
helper scripts. **Nothing here goes into an upstream PR.**

**Start here: [HANDOFF.md](HANDOFF.md).** The plan is in [PLAN.md](PLAN.md).

## Layout

| Path | What |
|---|---|
| `HANDOFF.md` | Where things stand and what to do next. Update it at the end of every session. |
| `PLAN.md` | Goal, targets, milestones, parked ideas, workflow rules |
| `research/` | Design notes and findings (`vaapi-encode.md`: stage A design, measurements, GPU conversion and colour findings) |
| `results/` | Raw measurements, one directory per run (date + what) |
| `tools/ipad_bench.py` | A/B od-sender encoders against the real iPad over USB |
| `tools/sync-upstream.sh` | Fast-forward `main` to upstream and rebase the topic branches |

## Working with two checkouts

Code and notes live in two worktrees of the same repository:

```bash
git clone git@github.com:muneebabbas/opendisplay.git ~/projects/opendisplay
cd ~/projects/opendisplay
git remote add upstream git@github.com:peetzweg/opendisplay.git
git remote set-url --push upstream no_push        # never push upstream by accident
git worktree add ../opendisplay-fork fork          # this branch, next to the code
```

* `~/projects/opendisplay`: code. `main` and the `linux/*` topic branches.
* `~/projects/opendisplay-fork`: this branch.

## Branch rules

* **`main`** mirrors `upstream/main` exactly. Never commit to it; only
  fast-forward it (`tools/sync-upstream.sh`).
* **Topic branches** (`linux/<topic>`) start from `main` (or from the branch
  they build on, stacked) and contain only code, tests and user docs that
  could one day go upstream. Rebased onto `main`, never merged with it.
  Conventional Commit messages, upstream's style.
* **`fork`** (this branch): everything else. Never merged anywhere.
* Each topic branch gets a draft PR **in this fork** (base `main`, or the
  branch below it) for review and a record of results.
