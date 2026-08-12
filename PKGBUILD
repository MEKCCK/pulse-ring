# Maintainer: MEKCCK <MEKCCK@users.noreply.github.com>
pkgname=pulse-ring
pkgver=0.1.0
pkgrel=1
pkgdesc="Wayland 壁纸层上的音乐律动可视化（GPU 渲染 + QML 样式 + Lua 行为）"
arch=('x86_64')
url="https://github.com/MEKCCK/pulse-ring"
license=('AGPL-3.0-only')
depends=('libpipewire' 'libxkbcommon' 'wayland' 'libglvnd' 'fontconfig')
makedepends=('rust' 'cargo' 'git')
source=("$pkgname::git+https://github.com/MEKCCK/pulse-ring.git")
sha256sums=('SKIP')

pkgver() {
  cd "$pkgname"
  git describe --tags --always 2>/dev/null | sed 's/^v//'
}

build() {
  cd "$pkgname"
  cargo build --release
}

package() {
  cd "$pkgname"
  install -Dm755 target/release/pulse-ring "$pkgdir/usr/bin/pulse-ring"
  # 默认配置（首次运行时自动复制到 ~/.config）
  install -Dm644 config/pulse-ring.qml "$pkgdir/usr/share/pulse-ring/pulse-ring.qml"
  install -Dm644 config/pulse-ring.lua "$pkgdir/usr/share/pulse-ring/pulse-ring.lua"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
