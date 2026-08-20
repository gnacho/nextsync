# Maintainer: gnacho <https://github.com/gnacho>
pkgname=nextsync
pkgver=0.102.0
pkgrel=1
pkgdesc='Nextcloud desktop synchronization client for GNOME (Rust rewrite)'
arch=('x86_64' 'aarch64')
url='https://github.com/gnacho/nextsync'
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
    # Build portable binaries regardless of the build host (issue #1):
    # - CachyOS's /etc/makepkg.conf.d/rust.conf exports
    #   RUSTFLAGS="-C target-cpu=native", which compiles ALL Rust code
    #   with build-host instructions; on an AVX-512 host, zmm ops land
    #   in generic functions with no runtime dispatch and the binary
    #   dies with SIGILL on older CPUs.
    # - The distro CFLAGS/LDFLAGS (-march=native, hardening, LTO) both
    #   leak -march into the bundled aws-lc C library and break its
    #   link; aws-lc keeps its own runtime CPU dispatch for the
    #   *_avx512 paths, so the toolchain default (x86-64 baseline) is
    #   what we want everywhere.
    unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS
    unset CFLAGS CXXFLAGS LDFLAGS
    cargo build --frozen --release

    # Smoke gate (issue #1): 512-bit zmm instructions may only appear in
    # aws-lc's runtime-dispatched *_avx512 symbols (plus its local asm
    # label skip_iv_len_12_init_IV and the *_gtable data tables, which
    # objdump disassembles as code). Anything else — any _R/_ZN Rust
    # symbol or generic C symbol — means host-specific codegen leaked in
    # and the artifact would SIGILL on non-AVX-512 machines. Revisit the
    # allowlist when aws-lc-sys is bumped.
    local unsafe
    unsafe=$(objdump -d target/release/nextsync \
        | awk '/^[0-9a-f]+ </ { sym = $2 }
               /zmm/ && sym !~ /avx512|gtable|skip_iv_len_12_init_IV/ { print sym }' \
        | sort -u)
    if [[ -n "$unsafe" ]]; then
        error 'zmm instructions outside runtime-dispatched symbols (issue #1):'
        printf '  %s\n' $unsafe
        return 1
    fi
}

package() {
    cd "$pkgname-$pkgver"
    install -Dm755 target/release/nextsync "$pkgdir/usr/bin/nextsync"

    # Full-color SVGs live in scalable/apps.
    local colored=(
        io.github.gnacho.nextsync
        io.github.gnacho.nextsync-folder
        nextsync-menu-log
        nextsync-menu-open
        nextsync-menu-quit
        nextsync-row-battery
        nextsync-row-error
        nextsync-row-not-configured
        nextsync-row-offline
        nextsync-row-ok
        nextsync-row-paused
        nextsync-row-syncing
        nextsync-state-globe
        nextsync-state-globe-off
        nextsync-tray-cloud
        nextsync-tray-cloud-alert
        nextsync-tray-cloud-check
        nextsync-tray-cloud-off
        nextsync-tray-cloud-sync
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
        nextsync-info-symbolic
        nextsync-list-checks-symbolic
        nextsync-settings-2-symbolic
        nextsync-status-battery-symbolic
        nextsync-status-error-symbolic
        nextsync-status-offline-symbolic
        nextsync-status-ok-symbolic
        nextsync-status-paused-symbolic
        nextsync-status-syncing-symbolic
        nextsync-theme-auto-symbolic
    )
    for icon in "${symbolic[@]}"; do
        install -Dm644 "data/icons/$icon.svg" \
            "$pkgdir/usr/share/icons/hicolor/symbolic/apps/$icon.svg"
    done

    install -Dm644 data/io.github.gnacho.nextsync.desktop \
        "$pkgdir/usr/share/applications/io.github.gnacho.nextsync.desktop"
}
