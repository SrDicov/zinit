# Maintainer: SrDicov
# zinit itself. Co-installable with dinit (see z-dinit-generator's
# PKGBUILD note): no provides/conflicts — the migration runs both.
pkgname=zinit
pkgver=0.1.0
pkgrel=1
pkgdesc="One-concept init: desired state + reconciler (PID 1 and supervisor)"
arch=('x86_64' 'aarch64')
url="https://github.com/SrDicov/zinit"
license=('GPL-3.0-or-later')
depends=('glibc')
makedepends=('cargo')
source=("$pkgname-$pkgver.tar.gz::https://github.com/SrDicov/zinit/archive/v$pkgver.tar.gz")
sha256sums=('SKIP')

build() {
  cd "$pkgname-$pkgver"
  cargo build --release --locked -p zinit -p zctl
}

package() {
  cd "$pkgname-$pkgver"
  install -Dm755 "target/release/zinit" "$pkgdir/usr/bin/zinit"
  install -Dm755 "target/release/zctl" "$pkgdir/usr/bin/zctl"
}
