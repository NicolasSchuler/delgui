# Releasing

Releases are cut by pushing a tag. Everything else — building the three
targets, checksums, the shell installer, the Homebrew formula, and the GitHub
Release itself — is done by `.github/workflows/release.yml`, which is
**generated** by [`dist`](https://github.com/axodotdev/cargo-dist) from
`dist-workspace.toml`.

## Before the first release

The Homebrew publish job pushes to a tap repository that has to exist already:

1. Create `NicolasSchuler/homebrew-tap` on GitHub, public, with a README.
2. Give the release workflow write access to it. A `GITHUB_TOKEN` is scoped to
   this repository only, so add a PAT with `contents: write` on the tap as a
   repository secret named `HOMEBREW_TAP_TOKEN`.

Until that exists, drop `publish-jobs = ["homebrew"]` from
`dist-workspace.toml` and regenerate, or the release will fail at the last step
with the artifacts already built.

## Cutting a release

```sh
# 1. Bump the version. It is inherited, so the workspace manifest is the only
#    place it is written.
$EDITOR Cargo.toml                    # [workspace.package] version = "0.2.0"
cargo check                           # refresh Cargo.lock

# 2. Move the Unreleased entries into a dated section, and add the two link
#    definitions at the bottom.
$EDITOR CHANGELOG.md

# 3. Check what will be built, without building it.
dist plan

# 4. Commit, tag, push.
git commit -am "release 0.2.0"
git tag v0.2.0
git push && git push --tags
```

The tag is what triggers the workflow; pushing the commit alone does nothing.
The Release is created as a draft and published when every artifact has
uploaded, so a failed target does not leave a half-populated release.

## Changing what is released

Edit `dist-workspace.toml`, then regenerate the workflow — never edit
`.github/workflows/release.yml` by hand:

```sh
dist init --yes      # after a dist version bump
dist generate        # after any config change
```

Both are also checked in CI by `dist`'s own generated job, which fails if the
committed workflow does not match the config.

## What is deliberately not released

- **Windows.** Panel buffers reach delta as `/dev/fd/N` pipes and subprocess
  teardown kills a process group; both are `#[cfg(unix)]`.
- **A macOS `.app` bundle.** A Finder-launched GUI inherits launchd's `PATH`
  (`/usr/bin:/bin:/usr/sbin:/sbin`), which excludes `/opt/homebrew/bin`, so
  `Delta::discover`'s plain `PATH` lookup would report delta missing for most
  Homebrew users. Teaching `discover` to probe the usual prefixes comes first.
- **crates.io.** Both crates carry the metadata for it, so `cargo publish -p
  delgui-core` then `-p delgui` is all it takes when wanted.
