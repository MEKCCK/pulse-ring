// pulse-ring 网页壁纸渲染器（Electron 离屏）
//
// 用法：electron main.js <html路径> <宽度> <高度>
// 通过 capturePage 定时抓帧，把页面帧按 stdout 输出：
//   [4 字节 LE 宽][4 字节 LE 高][宽*高*4 字节 RGBA]
// 从 stdin 读取带类型的消息：
//   0x00 + 128 x f32 + energy f32
//   0x01 + u32 JSON 长度 + JSON

const { app, BrowserWindow } = require('electron');
const path = require('path');

const htmlPath = process.argv[2];
const width = parseInt(process.argv[3] || '1920', 10);
const height = parseInt(process.argv[4] || '1080', 10);

let win = null;
let latestConfig = null;

let queue = [];
let writing = false;
let paused = false;
let outputClosed = false;

// The Rust side may stop/restart a wallpaper while Electron is between frames.
// A closed stdout pipe is expected in that case; do not turn it into an
// uncaught EPIPE dialog from Electron's main process.
process.stdout.on('error', () => {
  outputClosed = true;
  queue = [];
  writing = false;
  paused = true;
  process.exit(0);
});

function writeFrame(buf) {
  // 管道忙（stdout 未排空）时直接丢弃本帧：防止队列无界增长导致内存爆炸。
  if (paused || outputClosed) return;
  const header = Buffer.alloc(8);
  header.writeUInt32LE(width, 0);
  header.writeUInt32LE(height, 4);
  queue.push(header);
  queue.push(buf);
  pump();
}

function pump() {
  if (writing || queue.length === 0 || outputClosed) return;
  writing = true;
  const chunk = queue.shift();
  let ok;
  try {
    ok = process.stdout.write(chunk, (err) => {
      if (err) outputClosed = true;
    });
  } catch (_) {
    outputClosed = true;
    writing = false;
    return;
  }
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

// ---- 从 stdin 读取 pulse-ring 推送的音频和配置 ----
const input = { buf: Buffer.alloc(0) };
const AUDIO_BYTES = 1 + (128 + 1) * 4;
process.stdin.on('data', (chunk) => {
  input.buf = Buffer.concat([input.buf, chunk]);
  while (input.buf.length > 0) {
    const tag = input.buf[0];
    if (tag === 0) {
      if (input.buf.length < AUDIO_BYTES) break;
      const bands = new Array(128);
      for (let i = 0; i < 128; i++) bands[i] = input.buf.readFloatLE(1 + i * 4);
      const energy = input.buf.readFloatLE(1 + 128 * 4);
      const peak = (from, to) => {
        let value = 0;
        for (let i = from; i < to; i++) value = Math.max(value, bands[i]);
        return value;
      };
      if (win && !win.isDestroyed()) {
        win.webContents.send('pulse-bands', {
          bands,
          energy,
          bass: peak(0, 32),
          mid: peak(32, 96),
          treble: peak(96, 128),
          timestamp: Date.now(),
        });
      }
      input.buf = input.buf.slice(AUDIO_BYTES);
      continue;
    }

    if (tag === 1) {
      if (input.buf.length < 5) break;
      const len = input.buf.readUInt32LE(1);
      if (len === 0 || len > 1024 * 1024) {
        input.buf = input.buf.slice(1);
        continue;
      }
      if (input.buf.length < 5 + len) break;
      try {
        const cfg = JSON.parse(input.buf.slice(5, 5 + len).toString('utf8'));
        latestConfig = cfg;
        if (win && !win.isDestroyed()) win.webContents.send('pulse-config', cfg);
      } catch (_) {}
      input.buf = input.buf.slice(5 + len);
      continue;
    }

    // Unknown byte: discard only that byte so a malformed packet cannot make
    // subsequent valid audio/config packets disappear.
    input.buf = input.buf.slice(1);
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
  win.webContents.on('did-finish-load', () => {
    if (latestConfig) win.webContents.send('pulse-config', latestConfig);
  });

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
