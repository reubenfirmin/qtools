# Qtools AppImage

The release workflow builds one signed, multi-command AppImage for each supported architecture.
`AppRun` dispatches `dq` and `pq` from symlink invocation through the AppImage runtime's `ARGV0`.

The AppImage can install itself for the current user:

```sh
chmod +x Qtools-x86_64.AppImage
./Qtools-x86_64.AppImage --install
```

This copies the image to `~/.local/bin/qtools.AppImage` and creates relative `qtools`, `dq`, and `pq`
symlinks. It refuses to replace unrelated files or symlinks. `qtools update` delegates to the
standard `appimageupdatetool` using the zsync URL embedded by `build.sh`.

The GPG private key is supplied only through the `APPIMAGE_GPG_PRIVATE_KEY` GitHub Actions secret.
No private key belongs in this directory or repository.
