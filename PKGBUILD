pkgname=omarchy-presence-unlock
pkgver=0.1.0
pkgrel=3
pkgdesc='BLE device presence unlock for Omarchy'
arch=('x86_64')
url='https://github.com/mirceone/omarchy-presence-unlock'
license=('MIT')
depends=('bluez' 'pam' 'omarchy')
makedepends=('cargo' 'git')
source=("git+$url.git")
sha256sums=('SKIP')

pkgver() {
  cd "$srcdir/$pkgname"
  if description=$(git describe --long --tags 2>/dev/null); then
    printf '%s' "$description" | sed 's/^v//;s/\([^-]*-g\)/r\1/;s/-/./g'
  else
    printf 'r%s.%s' "$(git rev-list --count HEAD)" "$(git rev-parse --short HEAD)"
  fi
}

prepare() {
  cd "$srcdir/$pkgname"
  export RUSTUP_TOOLCHAIN=stable
  cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
  cd "$srcdir/$pkgname"
  export RUSTUP_TOOLCHAIN=stable
  export CARGO_TARGET_DIR=target
  cargo build --frozen --release --workspace
}

package() {
  cd "$srcdir/$pkgname"
  install -Dm755 target/release/omarchy-presence-unlock "$pkgdir/usr/bin/omarchy-presence-unlock"
  install -Dm755 target/release/presenced "$pkgdir/usr/bin/presenced"
  install -Dm755 target/release/libpam_omarchy_presence_unlock.so "$pkgdir/usr/lib/security/pam_omarchy_presence_unlock.so"
  install -Dm644 packaging/presenced.service "$pkgdir/usr/lib/systemd/user/presenced.service"
  install -Dm644 packaging/presenced.path "$pkgdir/usr/lib/systemd/user/presenced.path"
  install -Dm644 packaging/omarchy-lock-presence.pam "$pkgdir/usr/share/omarchy-presence-unlock/omarchy-lock-presence.pam"
  install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
