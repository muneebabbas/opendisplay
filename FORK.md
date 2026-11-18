# Fork notes (muneebabbas/opendisplay)

This fork publishes its own Linux releases. Upstream is `peetzweg/opendisplay`.

## Tags

`v<upstream X.Y.Z>-<label>.<N>`, for example `v1.30.3-muneeb.1`, then
`v1.30.3-muneeb.2`. After syncing upstream 1.31.0 the next is
`v1.31.0-muneeb.1`. The X.Y.Z part must equal the `linux/Cargo.toml` workspace
version; do not bump Cargo versions (release-please owns them). The binary
reports the full tag: `od-sender --version` prints `od-sender 1.30.3-muneeb.1`.

## Sync upstream

```sh
git remote add upstream https://github.com/peetzweg/opendisplay.git  # once
git fetch upstream --tags
git checkout main && git merge upstream/main   # or: git rebase upstream/main
```

## Cut a release

Push upstream's tags to the fork once, so the release notes can list the fork
commits since the upstream tag: `git push origin --tags`.

```sh
git tag v1.30.3-muneeb.1 && git push origin v1.30.3-muneeb.1
```

`.github/workflows/fork-release.yml` builds and tests only the Linux sender
(`od-sender` tarballs for x86_64 and aarch64, same names and layout as
upstream's). Once both builds pass it publishes a normal (non-prerelease,
non-draft, latest) GitHub release with them, so a failed build never leaves an
empty release. No receiver Flatpak, Mac or Android builds. It can also be
started with `workflow_dispatch` and an existing tag. It never runs in the
upstream repository, and no upstream file is modified.

Its `sender` job copies the `build` job of `linux-release.yml`. After syncing
upstream, check that job for changes (FFmpeg build, symbol floors, packaging)
and carry them over.

## Disable upstream's release workflow

```sh
gh workflow disable release.yml -R muneebabbas/opendisplay
```

Otherwise release-please opens release PRs and the Mac builds run without
secrets on every push to the fork's main.

## Omarchy plugin

Point the plugin at
`https://api.github.com/repos/muneebabbas/opendisplay/releases/latest`. That
endpoint returns the newest non-prerelease, non-draft release, so fork releases
must be published as normal releases.
