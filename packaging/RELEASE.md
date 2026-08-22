# Releasing qtools

Qtools publishes signed, updateable AppImages through GitHub Releases and the stable repository at
`https://repo.qtools.sh/`. The website is maintained independently in the sibling `qtools-site`
repository and deployed to `https://qtools.sh/`.

## One-time infrastructure

1. Create a Cloudflare Pages project named `qtools-repo` and attach `repo.qtools.sh`.
2. Create a GitHub environment named `release` and require approval if desired.
3. Generate a qtools-specific GPG signing key offline. Keep its primary/private material offline.
4. Add an exportable CI signing subkey as the `APPIMAGE_GPG_PRIVATE_KEY` environment secret.
5. Add narrowly scoped `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` environment secrets.
6. Export and commit only the public key as `packaging/appimage/qtools-release-key.asc`.

The release workflow deliberately refuses to run without that public-key file. It publishes the
file beside the AppImages and checksums so users can verify releases without trusting the website.

The Cloudflare token needs permission only to deploy the `qtools-repo` Pages project. No signing key,
passphrase, Cloudflare token, or account credential belongs in Git.

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
zsync update URLs, signs the images, creates the GitHub Release, and atomically deploys a new Pages
snapshot. GitHub Releases retain versioned history; `repo.qtools.sh` is the stable update channel.

## Local packaging check

`packaging/appimage/build.sh` expects already-built target binaries and an `appimagetool` executable:

```sh
packaging/appimage/build.sh x86_64-unknown-linux-gnu x86_64 0.2.0 /path/to/appimagetool dist
```
