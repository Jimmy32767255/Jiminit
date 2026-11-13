# Arch/Artix package recipe. This builds the current checkout instead of a
# source tarball, so run `makepkg -si` from the repository root.
pkgname=jiminit
pkgver=0.1.0
pkgrel=1
pkgdesc="Minimal two-stage init for Linux (initramfs stage 1 + PID 1)"
arch=('x86_64' 'aarch64')
license=('custom')
depends=()
makedepends=('rust' 'cargo')
backup=('etc/jiminit.toml')
source=()
sha256sums=()

_pkgroot="$startdir"

build() {
    cd "$_pkgroot"
    ./scripts/build.sh
}

package() {
    cd "$_pkgroot"
    ./scripts/install.sh --destdir "$pkgdir"
    install -Dm644 contrib/mkinitcpio/install/jiminit \
        "$pkgdir/usr/lib/initcpio/install/jiminit"
    install -Dm644 contrib/mkinitcpio/hooks/jiminit \
        "$pkgdir/usr/lib/initcpio/hooks/jiminit"
}
