// pulse-ring 网页壁纸渲染器（Electron 离屏）
//
// 用法：electron main.js <html路径> <宽度> <高度>
// 通过 capturePage 定时抓帧，把页面帧按 stdout 输出：
//   [4 字节 LE 宽][4 字节 LE 高][宽*高*4 字节 RGBA]
// 从 stdin 读取配置（tag 0x01 + 4 字节长度 + JSON）。
// 音频 API 已移除（避免子进程 stdin 阻塞导致主循环死锁）。

const { app, BrowserWindow } = require('electron');
const path = require('path');

const htmlPath = process.argv[2];
const width = parseInt(process.argv[3] || '1920', 10);
const height = parseInt(process.argv[4] || '1080', 10);

let win = null;

let queue = [];
let writing = false;
let paused = false;

function writeFrame(buf) {
  const header = Buffer.alloc(8);
  header.writeUInt32LE(width, 0);
  header.writeUInt32LE(height, 4);
  queue.push(header);
  queue.push(buf);
  pump();
}

function pump() {
  if (writing || queue.length === 0) return;
  writing = true;
  const chunk = queue.shift();
  const ok = process.stdout.write(chunk);
  if (!ok) {
    paused = true;
    process.stdout.once('drain', () => {
      paused = false;
      writing = false;
      pump();
    });
  } else {
    writing = false;
    pump();
  }
}

// ---- 从 stdin 读取 pulse-ring 推送的配置（tag 0x01 + 4 字节长度 + JSON）----
const pendingCfg = { buf: Buffer.alloc(0), need: 5 };
process.stdin.on('data', (chunk) => {
  pendingCfg.buf = Buffer.concat([pendingCfg.buf, chunk]);
  while (pendingCfg.buf.length >= 5) {
    const tag = pendingCfg.buf[0];
    const len = pendingCfg.buf.readUInt32LE(1);
    if (tag !== 1 || len <= 0 || len > 4096 || pendingCfg.buf.length < 5 + len) {
      pendingCfg.buf = Buffer.alloc(0);
      break;
    }
    try {
      const cfg = JSON.parse(pendingCfg.buf.slice(5, 5 + len).toString('utf8'));
      if (win && !win.isDestroyed()) win.webContents.send('pulse-config', cfg);
    } catch (_) {}
    pendingCfg.buf = pendingCfg.buf.slice(5 + len);
  }
});

app.commandLine.appendSwitch('ozone-platform', 'x11');
// 必须软件渲染：本机 MESA DRI 权限问题会让 GPU 进程段错误崩溃（exit 139）。
app.disableHardwareAcceleration();

app.whenReady().then(() => {
  win = new BrowserWindow({
    width,
    height,
    show: false,
    frame: false,
    transparent: false,
    webPreferences: {
      offscreen: true,
      backgroundThrottling: false,
      preload: path.join(__dirname, 'preload.js'),
    },
  });
  win.webContents.setFrameRate(30);
  win.loadFile(htmlPath);

  // 隐藏窗口的离屏 paint 事件只触发前 1-2 帧就停止（Electron 已知行为），
  // 改用 capturePage 定时抓帧：稳定 ~30fps。
  const captureTimer = () => {
    if (paused || !win || win.isDestroyed()) return;
    win.webContents.capturePage()
      .then((image) => {
        const size = image.getSize();
        if (size.width !== width || size.height !== height) return;
        const bgra = image.toBitmap(); // BGRA（Electron 位图）
        const rgba = Buffer.allocUnsafe(bgra.length);
        for (let i = 0; i < bgra.length; i += 4) {
          rgba[i] = bgra[i + 2];
          rgba[i + 1] = bgra[i + 1];
          rgba[i + 2] = bgra[i];
          rgba[i + 3] = bgra[i + 3];
        }
        writeFrame(rgba);
      })
      .catch(() => {})
      .finally(() => setTimeout(captureTimer, 33)); // ~30fps
  };
  setTimeout(captureTimer, 300); // 等页面加载后开始

  win.webContents.on('did-fail-load', (_e, code, desc) => {
    console.error(`web wallpaper load failed (${code}): ${desc}`);
    process.exit(1);
  });
});

process.on('SIGTERM', () => process.exit(0));
process.on('SIGINT', () => process.exit(0));
