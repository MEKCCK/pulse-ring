# Maintainer: MEKCCK <MEKCCK@users.noreply.github.com>
pkgname=pulse-ring
pkgver=4601668
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
  install -Dm644 pulse-ring-panel.desktop "$pkgdir/usr/share/applications/pulse-ring-panel.desktop"
  # 运行时资源（打包安装后二进制按 /usr/share/pulse-ring/ 查找）
  # 1) 网页壁纸 Electron helper（main.js/preload.js）
  install -Dm644 electron-wallpaper/main.js "$pkgdir/usr/share/pulse-ring/electron-wallpaper/main.js"
  install -Dm644 electron-wallpaper/preload.js "$pkgdir/usr/share/pulse-ring/electron-wallpaper/preload.js"
  install -Dm644 electron-wallpaper/package.json "$pkgdir/usr/share/pulse-ring/electron-wallpaper/package.json"
  # 2) 50+ 种 GLSL 过渡着色器
  mkdir -p "$pkgdir/usr/share/pulse-ring/assets/shaders/transitions"
  install -m644 assets/shaders/transitions/*.glsl "$pkgdir/usr/share/pulse-ring/assets/shaders/transitions/"
  # 3) 内置壁纸预设（首次运行自动部署到 ~/.config/pulse-ring/wallpapers/）
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
