#!/bin/sh
set -eu

if [ "$#" -ne 5 ]; then
    echo "usage: $0 TARGET ARCH VERSION APPIMAGETOOL OUTPUT_DIR" >&2
    exit 2
fi

target=$1
arch=$2
version=$3
appimagetool=$4
output_dir=$5
repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
mkdir -p "$output_dir"
output_dir=$(CDPATH= cd -- "$output_dir" && pwd)
appdir=$output_dir/Qtools.AppDir
output=$output_dir/Qtools-$arch.AppImage

rm -rf "$appdir"
mkdir -p "$appdir/usr/bin" "$appdir/usr/share/qtools" "$appdir/usr/share/applications" \
    "$appdir/usr/share/metainfo" "$appdir/usr/share/icons/hicolor/scalable/apps"

install -m 0755 "$repo_root/target/$target/release/dq" "$appdir/usr/bin/dq"
install -m 0755 "$repo_root/target/$target/release/pq" "$appdir/usr/bin/pq"
install -m 0755 "$repo_root/packaging/appimage/AppRun" "$appdir/AppRun"
install -m 0644 "$repo_root/packaging/appimage/sh.qtools.Qtools.desktop" "$appdir/sh.qtools.Qtools.desktop"
install -m 0644 "$repo_root/packaging/appimage/sh.qtools.Qtools.desktop" "$appdir/usr/share/applications/sh.qtools.Qtools.desktop"
install -m 0644 "$repo_root/packaging/appimage/sh.qtools.Qtools.metainfo.xml" "$appdir/usr/share/metainfo/sh.qtools.Qtools.appdata.xml"
install -m 0644 "$repo_root/packaging/appimage/sh.qtools.Qtools.svg" "$appdir/sh.qtools.Qtools.svg"
install -m 0644 "$repo_root/packaging/appimage/sh.qtools.Qtools.svg" "$appdir/usr/share/icons/hicolor/scalable/apps/sh.qtools.Qtools.svg"
printf '%s\n' "$version" > "$appdir/usr/share/qtools/VERSION"

update_url="zsync|https://repo.qtools.sh/Qtools-$arch.AppImage.zsync"
(cd "$output_dir" && ARCH=$arch "$appimagetool" --no-appstream -u "$update_url" --sign "$appdir" "$output")
echo "Built $output"
