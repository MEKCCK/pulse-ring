// pulse-ring 网页壁纸预加载脚本：把音频/控制 API 暴露给页面
// 页面里可这样使用：
//   window.pulseRing.onBands(({ bands, energy }) => { ... })  // 每帧 128 频段 + 整体能量
//   window.pulseRing.onConfig((cfg) => { ... })               // 壁纸清单参数（可选）
const { contextBridge, ipcRenderer } = require('electron');

const subscribe = (channel, map, cb) => {
  if (typeof cb !== 'function') throw new TypeError('pulseRing callback must be a function');
  const listener = (_event, value) => cb(map(value));
  ipcRenderer.on(channel, listener);
  return () => ipcRenderer.removeListener(channel, listener);
};

let latestAudio = Object.freeze({
  bands: new Float32Array(128), energy: 0, bass: 0, mid: 0, treble: 0, timestamp: 0,
});

ipcRenderer.on('pulse-bands', (_event, data) => {
  latestAudio = Object.freeze({
    bands: new Float32Array(data.bands),
    energy: Number(data.energy) || 0,
    bass: Number(data.bass) || 0,
    mid: Number(data.mid) || 0,
    treble: Number(data.treble) || 0,
    timestamp: Number(data.timestamp) || Date.now(),
  });
});

const onAudio = (cb) => subscribe('pulse-bands', (data) => ({
  bands: new Float32Array(data.bands),
  energy: Number(data.energy) || 0,
  bass: Number(data.bass) || 0,
  mid: Number(data.mid) || 0,
  treble: Number(data.treble) || 0,
  timestamp: Number(data.timestamp) || Date.now(),
}), cb);

contextBridge.exposeInMainWorld('pulseRing', {
  apiVersion: 1,
  onAudio,
  onBands: onAudio,
  getAudioData: () => latestAudio,
  onConfig: (cb) => subscribe('pulse-config', (cfg) => cfg, cb),
});
