// pulse-ring 网页壁纸渲染器（Electron 离屏）
//
// 用法：electron main.js <html路径> <宽度> <高度>
// 将离屏渲染的 HTML 页面按帧通过 stdout 输出：
//   [4 字节 LE 宽][4 字节 LE 高][宽*高*4 字节 RGBA]
// 由 pulse-ring 读取并作为壁纸纹理上传（与视频壁纸同路径）。

const { app, BrowserWindow } = require('electron');
const path = require('path');
const fs = require('fs');

const htmlPath = process.argv[2];
const width = parseInt(process.argv[3] || '1920', 10);
const height = parseInt(process.argv[4] || '1080', 10);

let queue = [];
let writing = false;
let paused = false;

// 向 stdout 写入一帧（RGBA），带背压：管道满时暂停渲染。
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

app.commandLine.appendSwitch('ozone-platform', 'x11');
app.disableHardwareAcceleration(); // 壁纸场景 CPU 渲染足够，避免 GPU 冲突
// ---- 从 stdin 读取 pulse-ring 推送的数据（帧协议）----
//   tag 0x00：音频帧，516 字节（128 f32 频段 + 1 f32 能量）
//   tag 0x01：配置帧，4 字节长度 + JSON
const pending = { tag: null, buf: Buffer.alloc(0), need: 1 };
let win = null;

function handleFrame(buf) {
  if (pending.tag === 0) {
    if (buf.length < 516) return;
    const bands = new Float32Array(128);
    for (let i = 0; i < 128; i++) bands[i] = buf.readFloatLE(i * 4);
    const energy = buf.readFloatLE(512);
    let bass = 0, mid = 0, treble = 0;
    for (let i = 0; i < 32; i++) bass += bands[i];
    for (let i = 32; i < 96; i++) mid += bands[i];
    for (let i = 96; i < 128; i++) treble += bands[i];
    if (win && !win.isDestroyed()) {
      win.webContents.send('pulse-bands', {
        bands: Array.from(bands), energy,
        bass: bass / 32, mid: mid / 64, treble: treble / 32,
      });
    }
  } else if (pending.tag === 1) {
    if (buf.length < 4) return;
    const len = buf.readUInt32LE(0);
    if (buf.length < 4 + len) return;
    const cfg = JSON.parse(buf.slice(4, 4 + len).toString('utf8'));
    if (win && !win.isDestroyed()) {
      win.webContents.send('pulse-config', cfg);
    }
  }
}

process.stdin.on('data', (chunk) => {
  let i = 0;
  while (i < chunk.length) {
    if (pending.need === 1) {
      pending.tag = chunk[i++];
      pending.need = pending.tag === 0 ? 516 : 4;
      pending.buf = Buffer.alloc(0);
    } else {
      const take = Math.min(pending.need, chunk.length - i);
      pending.buf = Buffer.concat([pending.buf, chunk.slice(i, i + take)]);
      i += take;
      pending.need -= take;
      if (pending.need === 0) {
        handleFrame(pending.buf);
        pending.need = 1;
      }
    }
  }
});

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

  win.webContents.on('paint', (_event, _dirty, image) => {
    if (paused) return;
    const size = image.getSize();
    if (size.width !== width || size.height !== height) return;
    const bgra = image.toBitmap(); // BGRA（Electron 位图）
    // BGRA -> RGBA
    const rgba = Buffer.allocUnsafe(bgra.length);
    for (let i = 0; i < bgra.length; i += 4) {
      rgba[i] = bgra[i + 2];
      rgba[i + 1] = bgra[i + 1];
      rgba[i + 2] = bgra[i];
      rgba[i + 3] = bgra[i + 3];
    }
    writeFrame(rgba);
  });

  win.webContents.on('did-fail-load', (_e, code, desc) => {
    console.error(`web wallpaper load failed (${code}): ${desc}`);
    process.exit(1);
  });
});

process.on('SIGTERM', () => process.exit(0));
process.on('SIGINT', () => process.exit(0));
