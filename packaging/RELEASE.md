# Releasing qtools

Qtools publishes updateable AppImages through GitHub Releases and the stable repository at
`https://repo.qtools.sh/`. The website is maintained independently in the sibling `qtools-site`
repository and deployed to `https://qtools.sh/`.

## One-time infrastructure

1. Create a Cloudflare Pages project named `qtools-repo` and attach `repo.qtools.sh`.
2. Create a GitHub environment named `release` and require approval if desired.
3. Add narrowly scoped `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` environment secrets.

The Cloudflare token needs permission only to deploy the `qtools-repo` Pages project. No token or
account credential belongs in Git. GitHub creates keyless build-provenance attestations through OIDC;
there is no persistent release-signing key to store or rotate.

## Release

Commit the changes to release on `main`, then run:

```sh
./release.py
```

Requires Python 3.9+, Cargo, Git, and an authenticated GitHub CLI (`gh`) with push access.
The script increments the patch version in `Cargo.toml` and `Cargo.lock`, runs the release-script
tests, Rust tests, Clippy, and a release build, then commits and tags the version. It pushes `main`
and the tag atomically over HTTPS using `gh` credentials, waits for CI and the release workflow,
and verifies both published download channels, all four checksums, and both build attestations.
It refuses a dirty checkout, a branch other than `main`, or a history that omits remote `main`.

To choose a version or supply release notes:

```sh
./release.py 0.5.0 --notes-file /path/to/notes.txt
```

Without a notes file, GitHub generates the release notes. If interrupted after pushing, continue
without creating another release:

```sh
./release.py --resume v0.4.4
```

Failures leave local edits, commits, and tags available for inspection. If a workflow fails,
fix or rerun it before resuming verification.

The tag workflow validates the version, tests, builds x86-64 and ARM64 AppImages, embeds the stable
zsync update URLs, creates keyless GitHub build-provenance attestations, creates the GitHub Release,
and atomically deploys a new Pages snapshot. GitHub Releases retain versioned history;
`repo.qtools.sh` is the stable update channel.

Users can verify a downloaded binary against the repository and release workflow:

```sh
gh attestation verify Qtools-x86_64.AppImage --repo reubenfirmin/qtools
```

## Local packaging check

`packaging/appimage/build.sh` expects already-built target binaries and an `appimagetool` executable:

```sh
packaging/appimage/build.sh x86_64-unknown-linux-gnu x86_64 0.2.0 /path/to/appimagetool dist
```
