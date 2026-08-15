// pulse-ring 网页壁纸预加载脚本：把音频/控制 API 暴露给页面
// 页面里可这样使用：
//   window.pulseRing.onBands(({ bands, energy }) => { ... })  // 每帧 128 频段 + 整体能量
//   window.pulseRing.onConfig((cfg) => { ... })               // 壁纸清单参数（可选）
const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('pulseRing', {
  onBands: (cb) => {
    ipcRenderer.on('pulse-bands', (_event, data) => {
      cb({
        bands: new Float32Array(data.bands),
        energy: data.energy,
        bass: data.bass,
        mid: data.mid,
        treble: data.treble,
      });
    });
  },
  onConfig: (cb) => {
    ipcRenderer.on('pulse-config', (_event, cfg) => cb(cfg));
  },
});
