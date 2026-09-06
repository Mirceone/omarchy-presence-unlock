pkgname=omarchy-presence-unlock
pkgver=0.1.0
pkgrel=1
pkgdesc='BLE device presence unlock for Omarchy'
arch=('x86_64')
url='https://github.com/mirceone/omarchy-presence-unlock'
license=('MIT')
install=packaging/omarchy-presence-unlock.install
backup=('etc/pam.d/omarchy-lock-presence')
depends=('bluez' 'pam' 'omarchy' 'hyprland' 'systemd')
makedepends=('cargo')
source=("$pkgname-$pkgver.tar.gz::$url/archive/refs/tags/v$pkgver.tar.gz")
# Replace with the release digest after the v0.1.0 tag is pushed.
sha256sums=('SKIP')
prepare() {
  cd "$srcdir/$pkgname-$pkgver"
  export RUSTUP_TOOLCHAIN=stable
  cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
  cd "$srcdir/$pkgname-$pkgver"
  export RUSTUP_TOOLCHAIN=stable
  export CARGO_TARGET_DIR=target
  cargo build --frozen --release --workspace
}

package() {
  cd "$srcdir/$pkgname-$pkgver"
  install -Dm755 target/release/omarchy-presence-unlock "$pkgdir/usr/bin/omarchy-presence-unlock"
  install -Dm755 target/release/presenced "$pkgdir/usr/bin/presenced"
  install -Dm755 target/release/libpam_omarchy_presence_unlock.so "$pkgdir/usr/lib/security/pam_omarchy_presence_unlock.so"
  install -Dm644 packaging/presenced.service "$pkgdir/usr/lib/systemd/user/presenced.service"
  install -Dm644 packaging/presenced.path "$pkgdir/usr/lib/systemd/user/presenced.path"
  install -Dm644 packaging/omarchy-lock-presence.pam "$pkgdir/etc/pam.d/omarchy-lock-presence"
  install -Dm644 packaging/plugin/manifest.json "$pkgdir/usr/share/omarchy-presence-unlock/plugin/manifest.json"
  install -Dm644 packaging/plugin/Service.qml "$pkgdir/usr/share/omarchy-presence-unlock/plugin/Service.qml"
  install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
