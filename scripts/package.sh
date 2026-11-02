#!/bin/sh
# Builds release tarballs into dist/.
#   scripts/package.sh            macOS universal (on a Mac) and Linux (natively on Linux, for
#                                 its own architecture, or x86_64 in Docker on a Mac when it's running)
#   scripts/package.sh macos      just macOS
#   scripts/package.sh linux      just Linux
# Layout: glass/bin/glass, glass/share/glass/web/*, and on macOS
# glass/share/glass/GlassMicrophone.driver (native/glass-mic; `glass mic install`) and
# glass/share/glass/libglassnowplaying.dylib (native/now-playing: what the Mac is playing)
set -eu
cd "$(dirname "$0")/.."

only=${1:-all}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
sha256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }

# tarball <platform> <binary> [extra things for share/glass…]
tarball() {
  platform=$1 binary=$2
  shift 2
  stage=$(mktemp -d)
  mkdir -p "$stage/glass/bin" "$stage/glass/share/glass"
  cp "$binary" "$stage/glass/bin/glass"
  chmod 755 "$stage/glass/bin/glass"
  cp -R web "$stage/glass/share/glass/web"
  for extra in "$@"; do cp -R "$extra" "$stage/glass/share/glass/"; done
  mkdir -p dist
  name="glass-$version-$platform.tar.gz"
  tar -C "$stage" -czf "dist/$name" glass
  rm -rf "$stage"
  (cd dist && sha256 "$name" > "$name.sha256" && cp "$name" "glass-latest-$platform.tar.gz")
  echo "dist/$name ($(du -h "dist/$name" | cut -f1))"
}

package_macos() {
  # CI builds the two architectures on separate runners and sets GLASS_PREBUILT.
  if [ -z "${GLASS_PREBUILT:-}" ]; then
    for target in aarch64-apple-darwin x86_64-apple-darwin; do
      cargo build --release -p glass-host --target "$target"
    done
  fi
  universal=$(mktemp)
  lipo -create -output "$universal" \
    target/aarch64-apple-darwin/release/glass-host \
    target/x86_64-apple-darwin/release/glass-host

  # Glass Microphone, the audio driver a viewer's mic plays through (universal already).
  cmake -S native/glass-mic -B target/glass-mic -DCMAKE_BUILD_TYPE=Release >/dev/null
  cmake --build target/glass-mic >/dev/null
  driver=target/glass-mic/GlassMicrophone.driver
  # What the Mac is playing, for viewers' media controls (loaded by /usr/bin/perl).
  mkdir -p target/now-playing
  nowplaying=target/now-playing/libglassnowplaying.dylib
  clang -dynamiclib -fobjc-arc -arch arm64 -arch x86_64 -mmacosx-version-min=13.0 -framework Foundation \
    native/now-playing/now_playing.m -o "$nowplaying"

  # A stable signing identity keeps macOS Screen Recording / Accessibility grants across
  # updates (ad-hoc signatures change with every build). The key stays out of the repo.
  key=${GLASS_SIGNING_KEY:-$HOME/.config/glass-release/signing.key}
  cert=${GLASS_SIGNING_CERT:-$HOME/.config/glass-release/signing.crt}
  if [ -f "$key" ] && [ -f "$cert" ] && command -v rcodesign >/dev/null 2>&1; then
    rcodesign sign --pem-file "$key" --pem-file "$cert" --binary-identifier dev.glassdesk.glass "$universal" >/dev/null
    codesign -dr - "$universal" 2>&1 | grep -q 'identifier "dev.glassdesk.glass"' || { echo "signing check failed" >&2; exit 1; }
    rcodesign sign --pem-file "$key" --pem-file "$cert" "$driver" >/dev/null
    rcodesign sign --pem-file "$key" --pem-file "$cert" --binary-identifier dev.glassdesk.nowplaying "$nowplaying" >/dev/null
    codesign -dr - "$driver" 2>&1 | grep -q 'identifier "dev.glassdesk.microphone"' || { echo "driver signing check failed" >&2; exit 1; }
    echo "signed macOS binary (dev.glassdesk.glass) and Glass Microphone"
  elif [ -n "${CI:-}" ]; then
    echo "error: release builds must be signed (set GLASS_SIGNING_KEY / GLASS_SIGNING_CERT, install rcodesign)" >&2
    exit 1
  else
    echo "warning: unsigned macOS build (permissions will reset on update)" >&2
  fi
  tarball darwin-universal "$universal" "$driver" "$nowplaying"
  rm -f "$universal"
}

package_linux() {
  if [ "$(uname -s)" = Linux ]; then
    cargo build --release -p glass-host
    tarball "linux-$(uname -m)" target/release/glass-host
  elif docker info >/dev/null 2>&1; then
    scripts/linux/build.sh
    tarball linux-x86_64 target/linux-x86_64/release/glass-host
  else
    echo "skipping Linux build (Docker isn't running)"
  fi
}

case "$only" in
  macos) package_macos ;;
  linux) package_linux ;;
  all)
    [ "$(uname -s)" = Darwin ] && package_macos
    package_linux
    ;;
  *) echo "usage: $0 [all|macos|linux]" >&2; exit 2 ;;
esac
