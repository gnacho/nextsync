# Maintainer: gnacho <https://github.com/gnacho>
pkgname=nextsync
pkgver=0.2.0
pkgrel=1
pkgdesc='Nextcloud desktop synchronization client for GNOME (Rust rewrite)'
arch=('x86_64' 'aarch64')
url='https://github.com/gnacho/nextsync-rs'
license=('GPL-3.0-or-later')
depends=('gtk4' 'libadwaita' 'glibc' 'gcc-libs')
makedepends=('cargo')
optdepends=('nextcloud-client: nextcloudcmd sync engine')
source=("nextsync-$pkgver.tar.gz")
sha256sums=('SKIP')

prepare() {
    cd "$pkgname-$pkgver"
    cargo fetch --locked
}

build() {
    cd "$pkgname-$pkgver"
    export CARGO_TARGET_DIR=target
    # aws-lc-sys' bundled C library does not link under the distro's
    # hardening/LTO flags; build with the toolchain defaults instead.
    unset CFLAGS CXXFLAGS LDFLAGS
    cargo build --frozen --release
}

package() {
    cd "$pkgname-$pkgver"
    install -Dm755 target/release/nextsync "$pkgdir/usr/bin/nextsync"

    # Full-color SVGs live in scalable/apps.
    local colored=(
        io.github.gnacho.nextsync
        io.github.gnacho.nextsync-folder
        nextsync-info-symbolic
        nextsync-settings-2-symbolic
        nextsync-tray-cloud
        nextsync-tray-cloud-off
        nextsync-tray-settings
    )
    for icon in "${colored[@]}"; do
        install -Dm644 "data/icons/$icon.svg" \
            "$pkgdir/usr/share/icons/hicolor/scalable/apps/$icon.svg"
    done

    # Symbolic (currentColor) SVGs: hicolor only indexes symbolic/apps, never
    # symbolic/status — installing under status makes the icons unresolvable.
    local symbolic=(
        io.github.gnacho.nextsync-symbolic
        nextsync-status-battery-symbolic
        nextsync-status-error-symbolic
        nextsync-status-offline-symbolic
        nextsync-status-ok-symbolic
        nextsync-status-paused-symbolic
        nextsync-status-syncing-symbolic
    )
    for icon in "${symbolic[@]}"; do
        install -Dm644 "data/icons/$icon.svg" \
            "$pkgdir/usr/share/icons/hicolor/symbolic/apps/$icon.svg"
    done

    install -Dm644 data/io.github.gnacho.nextsync.desktop \
        "$pkgdir/usr/share/applications/io.github.gnacho.nextsync.desktop"
}
