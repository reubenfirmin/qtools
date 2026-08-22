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

1. Update the version in `Cargo.toml` and update `Cargo.lock` with `cargo check`.
2. Update release notes and commit the release.
3. Run `cargo test --locked --all-targets` and `./build.sh` locally.
4. Tag exactly the Cargo version and push:

```sh
version=0.2.0
git tag -a "v$version" -m "qtools $version"
git push origin main
git push origin "v$version"
```

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
