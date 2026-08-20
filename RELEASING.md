# Releasing

Releases are cut by pushing a tag. Everything else — building the three
targets, checksums, the shell installer, the Homebrew formula, and the GitHub
Release itself — is done by `.github/workflows/release.yml`, which is
**generated** by [`dist`](https://github.com/axodotdev/cargo-dist) from
`dist-workspace.toml`.

## Installers are off, and why

`installers = []` while the repository is private. This is a constraint, not a
preference: both the shell installer and a Homebrew formula fetch
`releases/download/…` with no credentials, which a private repo refuses, and
Homebrew has no way to authenticate for one at all — [cargo-dist#2267][private]
is open and unimplemented. Turning them on would produce artifacts that build
cleanly in CI and then fail for every person who tries to use them.

A tagged release still builds all three targets and attaches them, with
checksums, to a GitHub Release that anyone with repository access can download.

[private]: https://github.com/axodotdev/cargo-dist/issues/2267

## When the repository goes public

Three steps, in this order:

1. **Create the tap.** A Homebrew tap is an ordinary GitHub repository whose
   name begins with `homebrew-`; nothing is registered with Homebrew itself.
   `brew install NicolasSchuler/tap/delgui` resolves to `delgui.rb` in
   `github.com/NicolasSchuler/homebrew-tap`, so that repository has to exist and
   be public.

   ```sh
   gh repo create NicolasSchuler/homebrew-tap --public \
     --description "Homebrew formulae for NicolasSchuler's tools"
   ```

2. **Give the release workflow write access to it.** `GITHUB_TOKEN` is scoped to
   this repository alone, so add a PAT with `contents: write` on the tap as a
   repository secret named `HOMEBREW_TAP_TOKEN`.

3. **Switch the installers back on** in `dist-workspace.toml` and regenerate:

   ```toml
   installers = ["shell", "homebrew"]
   tap = "NicolasSchuler/homebrew-tap"
   publish-jobs = ["homebrew"]

   # delta is a hard runtime dependency and git is part of the render pipeline,
   # so the formula says so. This is the best argument for shipping via brew at
   # all: `brew install delgui` then cannot leave you with an app whose first
   # act is to tell you delta is missing.
   [dist.dependencies.homebrew]
   git-delta = { stage = ["run"] }
   git = { stage = ["run"] }
   ```

   ```sh
   dist generate
   ```

   Then update the Install section of the README, which currently explains the
   private-repo situation instead.

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
  delgui-core` then `-p delgui` is all it takes when wanted. Note crates.io is
  public and irreversible — a published version cannot be unpublished, only
  yanked — so it waits on the same decision the installers do.
