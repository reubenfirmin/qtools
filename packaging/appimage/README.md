# Qtools AppImage

The release workflow builds one multi-command AppImage for each supported architecture.
`AppRun` dispatches `dq` and `pq` from symlink invocation through the AppImage runtime's `ARGV0`.

The AppImage can install itself for the current user:

```sh
chmod +x Qtools-x86_64.AppImage
./Qtools-x86_64.AppImage --install
```

This moves the image to `~/.local/bin/qtools.AppImage` and creates relative `qtools`, `dq`, and `pq`
symlinks. It refuses to replace unrelated files or symlinks. `qtools update` downloads the full
architecture-specific AppImage and checksum from `repo.qtools.sh`, verifies the digest and GitHub
build-provenance attestation, and only then atomically replaces the installed image.

GitHub Actions publishes a keyless build-provenance attestation for each AppImage. Verification uses
`gh attestation verify` and the repository identity, so there is no persistent signing key.
