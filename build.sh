#!/bin/sh
# Build release assets for Linux-based Stations. Desktop installers build natively.
set -eu
cd "$(dirname "$0")"
: "${VERSION:=dev}" "${REPO:=newfpv/YaJarvis}" "${ZIG:=zig}"
VERSION=${VERSION#v}
export JARVIS_VERSION="$VERSION" JARVIS_REPO="$REPO"
if [ -z "${STATION_PLUGIN_IDS:-}" ] && [ -z "${JARVIS_UI:-}" ]; then
    npm run build:panel
fi
out="$PWD/build/release"
tools="$PWD/build/station-build"
mkdir -p "$out" "$tools"
ZIG=$(command -v "$ZIG")
export ZIG
cat > "$tools/cc" <<'EOF'
#!/bin/sh
original=$#
while [ "$original" -gt 0 ]; do
    case "$1" in --target=*) ;; *) set -- "$@" "$1" ;; esac
    shift
    original=$((original - 1))
done
exec "$ZIG" cc -target "$JARVIS_ZIG_TARGET" ${JARVIS_ZIG_CPU:+-mcpu="$JARVIS_ZIG_CPU"} "$@"
EOF
cat > "$tools/ar" <<'EOF'
#!/bin/sh
exec "$ZIG" ar "$@"
EOF
chmod +x "$tools/cc" "$tools/ar"

"$ZIG" cc -target arm-linux-gnueabihf.2.30 -mcpu=cortex_a7 -shared -fPIC -O2 -s -o "$tools/vqe_tap-armhf.so" station/vqe_tap.c -ldl
"$ZIG" cc -target aarch64-linux-gnu.2.30 -shared -fPIC -O2 -s -o "$tools/vqe_tap-arm64.so" station/vqe_tap.c -ldl
"$ZIG" cc -target arm-linux-gnueabihf.2.30 -mcpu=cortex_a7 -O2 -s -o "$tools/spotcmd" station/spotcmd.c -ldl
"$ZIG" cc -target arm-linux-gnueabihf.2.17 -Os -s -o "$tools/mini1-pcm" station/mini1-pcm.c -ldl

manifest="$out/rust-builds.json"
printf '{"version":"%s","builds":[' "$VERSION" > "$manifest"
comma=
core_assets=
for arch in ${STATION_ARCHES:-arm64 armv7}; do
    case "$arch" in
        arm64)
            target=aarch64-unknown-linux-musl
            export JARVIS_ZIG_TARGET=aarch64-linux-musl JARVIS_ZIG_CPU= JARVIS_CMAKE_CPU=aarch64
            ;;
        armv7)
            target=armv7-unknown-linux-musleabihf
            export JARVIS_ZIG_TARGET=arm-linux-musleabihf JARVIS_ZIG_CPU=cortex_a7 JARVIS_CMAKE_CPU=armv7
            includes=$("$tools/cc" -E -v -x c /dev/null 2>&1 | sed -n '/^#include <\.\.\.>/,/^End of search list\./s/^ \(.*\)$/-isystem "\1"/p' | tr '\n' ' ')
            test -n "$includes"
            export BINDGEN_EXTRA_CLANG_ARGS_armv7_unknown_linux_musleabihf="-nostdinc $includes"
            ;;
        *) echo "Unknown Station architecture: $arch" >&2; exit 1 ;;
    esac
    env_target=$(printf '%s' "$target" | tr - _)
    export "CC_$env_target=$tools/cc" "AR_$env_target=$tools/ar"
    export "CMAKE_TOOLCHAIN_FILE_$env_target=$PWD/scripts/aarch64-musl.cmake" "CMAKE_GENERATOR_$env_target=Ninja"
    export JARVIS_CMAKE_CC="$tools/cc" JARVIS_CMAKE_AR="$tools/ar"
    release="${CARGO_TARGET_DIR:-$PWD/jarvis/target}/$target/release"
    if [ -n "${STATION_PLUGIN_IDS:-}" ]; then
        sh scripts/pack-official.sh "$out" "$arch" "$target" "$release" $STATION_PLUGIN_IDS
        continue
    fi
    (cd jarvis && cargo build --locked --release --target "$target" -p jarvis &&
        cargo build --locked --release --target "$target" -p jarvis-station)
    stage="$tools/$arch"
    mkdir -p "$stage"
    cp "$release/jarvis" "$stage/jarvisd"
    cp "$release/jarvis-supervisor" "$stage/"
    cp "$tools"/vqe_tap-*.so "$tools/spotcmd" "$tools/mini1-pcm" models/wake.bin models/oww.bin models/wake_oww.bin "$stage/"
    for file in jarvisd.sh S98jarvisd; do tr -d '\r' < "station/$file" > "$stage/$file"; done
    rm -f "$out/station-$arch.zip"
    (cd "$stage" && zip -q "$out/station-$arch.zip" jarvisd jarvis-supervisor vqe_tap-*.so spotcmd mini1-pcm wake.bin oww.bin wake_oww.bin jarvisd.sh S98jarvisd)
    cp "$stage/jarvisd" "$out/jarvisd-$arch"
    core_assets="$core_assets jarvisd-$arch"
    bytes=$(wc -c < "$stage/jarvisd" | tr -d '[:space:]')
    printf '%s{"arch":"%s","asset":"jarvisd-%s","modules":[],"bytes":%s}' "$comma" "$arch" "$arch" "$bytes" >> "$manifest"
    if [ "${STATION_CORE_ONLY:-0}" != 1 ]; then
        sh scripts/pack-official.sh "$out" "$arch" "$target" "$release"
    fi
    comma=,
done
printf ']}\n' >> "$manifest"
if [ -n "${STATION_PLUGIN_IDS:-}" ]; then
    (cd "$out" && sha256sum plugin-*.zip > SHA256SUMS)
    exit
fi
for file in install.sh install.ps1; do
    sed "s#__REPO__#$REPO#g" "installer/$file" | tr -d '\r' > "$out/$file"
done
tar -czf "$out/THIRD_PARTY_NOTICES.tar.gz" LICENSE LICENSES THIRD_PARTY.md CREDITS.md
sh scripts/pack-plugins.sh "$out"
# core_assets contains only names formed from the two known architectures.
(cd "$out" && sha256sum station-*.zip $core_assets install.* plugin-*.zip rust-builds.json THIRD_PARTY_NOTICES.tar.gz > SHA256SUMS)
