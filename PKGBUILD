# Maintainer: MEKCCK <MEKCCK@users.noreply.github.com>
pkgname=pulse-ring
pkgver=0.1.0
pkgrel=1
pkgdesc="Wayland 壁纸层上的音乐律动可视化（GPU 渲染 + QML 样式 + Lua 行为）"
arch=('x86_64')
url="https://github.com/MEKCCK/pulse-ring"
license=('AGPL-3.0-only')
depends=('libpipewire' 'libxkbcommon' 'wayland' 'libglvnd' 'fontconfig')
makedepends=('rust' 'cargo' 'git' 'cmake' 'qt6-base' 'qt6-declarative')
optdepends=('qt6-base: Qt Quick 壁纸包切换面板 pulse-ring-panel')
source=("$pkgname::git+https://github.com/MEKCCK/pulse-ring.git")
sha256sums=('SKIP')

pkgver() {
  cd "$pkgname"
  git describe --tags --always 2>/dev/null | sed 's/^v//'
}

build() {
  cd "$pkgname"
  cargo build --release
  # Qt Quick 壁纸包切换面板（需要 qt6-base / qt6-declarative）
  if command -v cmake >/dev/null 2>&1; then
    cmake -S tools/qt-panel -B tools/qt-panel/build -DCMAKE_BUILD_TYPE=Release >/dev/null
    cmake --build tools/qt-panel/build >/dev/null
  fi
}

package() {
  cd "$pkgname"
  install -Dm755 target/release/pulse-ring "$pkgdir/usr/bin/pulse-ring"
  # 默认配置（首次运行时自动复制到 ~/.config）
  install -Dm644 config/pulse-ring.qml "$pkgdir/usr/share/pulse-ring/pulse-ring.qml"
  install -Dm644 config/pulse-ring.lua "$pkgdir/usr/share/pulse-ring/pulse-ring.lua"
  # 图标与桌面项
  install -Dm644 icon.svg "$pkgdir/usr/share/icons/hicolor/scalable/apps/pulse-ring.svg"
  install -Dm644 icon.svg "$pkgdir/usr/share/pixmaps/pulse-ring.svg"
  # 内置壁纸预设（网页/场景壁纸从编译期目录读取，需打包完整资源）
  install -Dm644 assets/wallpapers/presets.json "$pkgdir/usr/share/pulse-ring/assets/wallpapers/presets.json"
  for d in audio-scene aurora-scene demo-clock.html; do
    if [ -e "assets/wallpapers/$d" ]; then
      cp -r "assets/wallpapers/$d" "$pkgdir/usr/share/pulse-ring/assets/wallpapers/"
    fi
  done
  # Qt 面板
  if [ -x tools/qt-panel/build/pulse-ring-panel ]; then
    install -Dm755 tools/qt-panel/build/pulse-ring-panel "$pkgdir/usr/bin/pulse-ring-panel"
  fi
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
