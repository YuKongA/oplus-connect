# Maintainer: OPLUS Connect contributors
pkgname=oplus-connect-git
pkgver=0.1.0.r0.g0000000
pkgrel=1
pkgdesc='Linux client for OPPO and OnePlus device connectivity'
arch=('x86_64')
url='https://github.com/YuKongA/oplus-connect'
license=('LicenseRef-unknown')
# GCC LTO objects from C dependencies are incompatible with Rust's linker.
options=('!lto' '!debug')
depends=(
  'glibc' 'gcc-libs' 'glib2' 'fontconfig' 'freetype2' 'libxcb' 'libxkbcommon' 'libxkbcommon-x11'
  'vulkan-icd-loader'
  'gstreamer' 'gst-plugins-base' 'gst-plugins-good' 'gst-plugins-bad'
  'gst-plugin-va' 'gst-plugin-pipewire' 'libnice' 'xdg-desktop-portal' 'iproute2'
)
makedepends=('git' 'cargo' 'pkgconf')
optdepends=(
  'gst-plugins-ugly: software H.264 encoder for tablet screen sharing'
  'libva-driver: GPU-specific VA-API driver'
  'networkmanager: determine the current Wi-Fi network'
)
provides=('oplus-connect')
conflicts=('oplus-connect')
_source_name=oplus-connect
if [[ -n ${OPLUS_CONNECT_SOURCE_URL:-} ]]; then
  _source_name=oplus-connect-local
fi
source=("${_source_name}::git+${OPLUS_CONNECT_SOURCE_URL:-${url}.git}" 'oplus-connect.desktop')
sha256sums=('SKIP' 'd710c773ba7476e0df6a726d3e7b30447e81afb9eaba8b518f15ba662bf3b021')

pkgver() {
  cd "$srcdir/$_source_name"
  printf '0.1.0.r%s.g%s' "$(git rev-list --count HEAD)" "$(git rev-parse --short=7 HEAD)"
}

prepare() {
  cd "$srcdir/$_source_name"
  cargo fetch --locked --target x86_64-unknown-linux-gnu --manifest-path oplus-connect-gui/Cargo.toml
}

_cargo_build_env() {
  local -a flags=()
  if [[ -n ${CARGO_ENCODED_RUSTFLAGS:-} ]]; then
    IFS=$'\x1f' read -r -a flags <<< "$CARGO_ENCODED_RUSTFLAGS"
  elif [[ -n ${RUSTFLAGS:-} ]]; then
    read -r -a flags <<< "$RUSTFLAGS"
  fi
  flags+=("--remap-path-prefix=$srcdir=/usr/src/oplus-connect")
  flags+=("--remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/usr/src/cargo")
  local encoded
  printf -v encoded '%s\x1f' "${flags[@]}"
  export CARGO_ENCODED_RUSTFLAGS="${encoded%$'\x1f'}"
  export CARGO_TARGET_DIR="$srcdir/target"
}

build() (
  cd "$srcdir/$_source_name"
  _cargo_build_env
  cargo build --release --locked --manifest-path oplus-connect-gui/Cargo.toml
)

check() (
  cd "$srcdir/$_source_name"
  _cargo_build_env
  cargo test --release --locked --manifest-path oplus-connect-gui/Cargo.toml
)

package() {
  cd "$srcdir/$_source_name"
  install -Dm755 "$srcdir/target/release/oplus-connect-gui" "$pkgdir/usr/bin/oplus-connect"
  install -Dm644 "$srcdir/oplus-connect.desktop" "$pkgdir/usr/share/applications/oplus-connect.desktop"
  install -Dm644 oplus-connect-gui/assets/app-icon.png "$pkgdir/usr/share/icons/hicolor/512x512/apps/oplus-connect.png"
  install -d "$pkgdir/usr/share/oplus-connect/assets"
  install -m644 oplus-connect-gui/assets/*.png "$pkgdir/usr/share/oplus-connect/assets/"
  install -Dm644 README.md "$pkgdir/usr/share/doc/oplus-connect/README.md"
}
