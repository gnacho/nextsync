# Maintainer: gnacho <https://github.com/gnacho>
pkgname=nextsync
pkgver=0.44.4
pkgrel=1
pkgdesc='Nextcloud desktop synchronization client for GNOME (Rust rewrite)'
arch=('x86_64' 'aarch64')
url='https://github.com/gnacho/nextsync'
license=('GPL-3.0-or-later')
depends=('gtk4' 'libadwaita' 'glibc' 'gcc-libs')
makedepends=('cargo')
# The sync engine ships inside the binary. nextcloud-client and
# opencloud-desktop are only needed for the per-folder legacy mode.
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

        # Icons: full-color SVGs go to scalable/apps, symbolic (currentColor)
    # ones to symbolic/apps (hicolor only indexes symbolic/apps, never
    # symbolic/status). Installed by glob from the source tree so icon
    # renames (Flathub naming) can never desync this list.
    local svg base
    for svg in data/icons/*.svg; do
        base=$(basename "$svg" .svg)
        if [[ "$base" == *-symbolic ]]; then
            install -Dm644 "$svg" "$pkgdir/usr/share/icons/hicolor/symbolic/apps/$base.svg"
        else
            install -Dm644 "$svg" "$pkgdir/usr/share/icons/hicolor/scalable/apps/$base.svg"
        fi
    done

    install -Dm644 data/io.github.gnacho.nextsync.desktop \
        "$pkgdir/usr/share/applications/io.github.gnacho.nextsync.desktop"
    install -Dm644 data/io.github.gnacho.nextsync.metainfo.xml \
        "$pkgdir/usr/share/metainfo/io.github.gnacho.nextsync.metainfo.xml"
}
