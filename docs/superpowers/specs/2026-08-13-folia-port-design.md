# pulse-ring 完善设计：渲染优化 + Folia 能力移植 + 多语言扩展

日期：2026-08-13
状态：已批准（决策点采用推荐默认值）

## 1. 背景与目标

pulse-ring 是 Wayland 壁纸层音乐律动可视化（wgpu/Vulkan）。当前为 Rust 单核架构：

- Rust 内核（~4900 行）：PipeWire 采集 → FFT → 128 频段、QML 配置解析、wgpu 渲染、C ABI 插件加载
- Lua（mlua 内嵌）：每帧 `onUpdate(dt)` / `transformBands(bands)`，`pulse.*` widget API
- C ABI cdylib 插件（libloading）：`PulsePluginV1` 结构体 + 函数指针回调

参考项目 folia-major（Folia，全屏沉浸式歌词播放器，TypeScript/React + Node + Electron）具有可移植资产：

- 歌词管线：`lrcParser` / `parserCore`（LRC/增强 LRC/翻译行）、网易云/酷狗 provider、`autoMatchBestLyric` 智能匹配、逐字 grapheme timing、卡拉OK着色
- 10 个视觉主题 + AI 配色（`aiThemePrompts` + Gemini/OpenAI）
- Stage API 外部控制 + `GET /v1/lyric` 本地歌词接口
- 工程实践：vitest 单元测试、playwright UI 测试、CI、文档

**目标**（按优先级）：
1. 优化渲染性能（GPU 化、批处理、自适应帧率、降 CPU 占用）
2. 移植歌词能力（解析、获取、渲染、动画）
3. 移植"能移植的都移植"：主题、外部集成、AI 配色
4. 将项目从 Rust 为主拓展为多编程语言项目（插件层支持 Python/JS 等）

## 2. 决策点（默认值，用户已确认"ok"）

| # | 决策点 | 默认值 |
|---|--------|--------|
| 1 | 歌词在线获取 | **启用**：MPRIS title/artist → 网易云/酷狗公开搜索 API（无需 key），失败自动回退本地 `.lrc` |
| 2 | AI 配色 | **可选功能，默认关闭**：配置 API key 后启用（Gemini/OpenAI 兼容） |
| 3 | 多语言优先级 | **Python 先**，随后 JS/TS；保留 Lua 内嵌 + C ABI |
| 4 | 优化目标 | **两者都要**：低端 GPU 提帧率（自适应 30→60fps）+ 降低 CPU 占用 |

## 3. 阶段规划

### Phase 0 — 基线测量

- 在 `main.rs` tick 循环加帧耗时统计（tick/audio/lua/plugins/widgets/draw 分项），可选 `PULSE_RING_PROFILE=1` 环境变量输出到日志
- 产出瓶颈报告，校准后续优化优先级

### Phase 1 — 渲染优化

1. **粒子 GPU 化**：`compute_particles`（main.rs，CPU 32 槽顶点计算）改为 GPU 侧（compute shader 更新粒子 buffer，vertex shader 直接消费）
2. **绘制批处理**：widgets/粒子合并为单大 vertex buffer + instanced draw，减少 draw call 数量
3. **自适应帧率**：音乐活动时 30/60fps，静音/暂停时 5fps 或停帧；空闲时跳过粒子/魔法阵计算（参考 Folia `frameRateLimiter.ts` 思路）
4. **共享 GPU 资源**：多屏共用 device/queue，按屏幕复制 uniform，避免每屏独立实例开销
5. **零分配帧循环**：复用 Vec/HashMap，消除每帧分配（当前 Lua `frame` 每帧建表）
6. **音频线程化确认**：确保 FFT/band 计算在音频线程，不与渲染争主线程

### Phase 2 — 歌词移植（核心）

1. **LRC 解析器**（`src/lyrics/lrc.rs`）：移植 Folia `parserCore` 语义 → Rust：
   - 普通 LRC `[mm:ss.xx]`、增强 LRC `<mm:ss.xx>` 逐字、多时间戳行、翻译行（`LyricData` 结构）
   - 单元测试覆盖（从 Folia 测试用例移植）
2. **歌词来源**：
   - 本地 `.lrc`：`~/.config/pulse-ring/lyrics/<title>.lrc`；若 MPRIS 提供本地文件路径优先读取
   - 在线：MPRIS title/artist → 网易云搜索 → 下载 LRC；失败回退酷狗；再失败回退本地
   - 歌词缓存：`~/.cache/pulse-ring/lyrics/<artist>-<title>.lrc`
3. **歌词渲染 widget**：新增 `lyric` widget 类型（QML `Widget { type: "lyric"; ... }`）：
   - 用现有 ab_glyph/rusttype 字体管线渲染文字
   - 布局：当前行高亮 + 上一/下一行（参考 Folia `monetLyricsModel` rail）；当前行放大
   - 逐字卡拉OK着色（参考 `wordColoring.ts` / grapheme timing）
   - 换行/省略/字号/颜色/对齐由 QML 配置
4. **歌词动画**：淡入淡出、当前行滚动过渡、逐字推进

### Phase 3 — 主题移植

1. **预设主题**：Folia 现有视觉主题（浮名/流光/心象/云阶/群唱/倾诉/镜台/时计等，README 展示 8 个）的配色+布局映射为 QML preset，存放 `~/.config/pulse-ring/themes/<name>.qml`，`pulse-ring --theme <name>` 切换
2. **AI 配色**：移植 `aiThemePrompts` 提示词思路 → 子进程/HTTP 调 Gemini/OpenAI 兼容接口，返回配色写入主题文件（默认关闭）

### Phase 4 — 外部集成

- Loopback HTTP 服务（127.0.0.1，端口可配置，默认 32109 与 Folia 对齐）：
  - `GET /v1/lyric`：当前歌词 JSON（对齐 Folia `lyric-api.md` 协议）
  - `POST /v1/control`：切换主题/调参/控制 widget（仿 Stage API）
  - `GET /v1/status`：当前音乐/频段能量/插件列表
- 轻量 HTTP 依赖（`tiny_http` 或手写 socket 监听，避免 tokio 全量引入）

### Phase 5 — 多语言扩展

**架构：子进程 + JSON-RPC over stdio**（类 Folia worker 模式）：

```
Rust 内核 ──spawn──> python3/node plugin.py/plugin.js
            stdin/stdout 帧协议（JSON-RPC 2.0）
```

- **协议**：`serde_json` + 手写长度前缀帧（4 字节 LE 长度 + JSON）。请求：`initialize` / `onUpdate(dt)` / `transformBands(bands)` / `onEvent(name, payload)` / `shutdown`；响应或单向通知；每帧超时保护（如 5ms 跳过该插件本帧）
- **Python 插件**：`~/.config/pulse-ring/plugins-py/*.py`，`spawn python3`；宿主提供 `host.get_band(i)` / `host.set_config(key,val)` / `host.log(msg)` / `host.get_time_hms()` 反向调用（对齐 C ABI `PluginCtx` 语义）
- **JS/TS 插件**：`~/.config/pulse-ring/plugins-js/*.js`，`spawn node`，同样协议
- **保留**：Lua 内嵌（轻量脚本路径）+ C ABI（性能关键路径，如渲染纹理）
- **隔离性**：子进程崩溃不影响主进程；stderr 进日志；禁止插件直接触碰渲染
- 新增依赖：`serde` + `serde_json`（其余手写）

### Phase 6 — 工程化

- `cargo test` 覆盖：LRC 解析、config 解析、频段变换、RPC 帧协议、歌词匹配
- CI：GitHub Actions（fmt + clippy + test + release 构建）
- 文档：README 补多语言插件开发指南、歌词配置、HTTP 接口说明
- 示例：`plugins-example/python_demo.py`、`plugins-example/js_demo.js`、`themes/` 预设

## 4. 新增依赖（预估）

| 依赖 | 用途 |
|------|------|
| `serde` / `serde_json` | RPC 协议、HTTP JSON |
| `tiny_http` | loopback HTTP 服务（轻量） |
| `ureq` | 歌词在线获取（阻塞式、轻量，避免 tokio） |
| `md-5` / `base64` | 网易云/酷狗搜索签名（参考 Folia provider） |

## 5. 风险与注意

- **歌词 API 可用性**：网易云/酷狗公开接口可能变动；全部走缓存 + 优雅降级（在线 → 本地 → 无歌词显示占位）
- **子进程开销**：每帧 RPC 有进程间通信开销；帧率敏感路径（transformBands）建议批处理（如 4 帧合并一次）或留 C ABI 做高性能路径
- **字体渲染**：中文歌词需要 CJK 字体（现有 JetBrains Maple Mono / 任意 CJK 字体依赖已满足）
- **多屏 + 歌词**：歌词按屏渲染，`renderScreen` 语义保持不变

## 6. 成功标准

- 渲染：相同配置下 CPU 占用明显下降；活动时可达 60fps，空闲自动降帧
- 歌词：MPRIS 播放任意歌曲能显示歌词（在线优先，离线回退），逐字卡拉OK可配置开关
- 多语言：Python 和 JS 示例插件可运行、可控制 widget、崩溃隔离
- 主题：`--theme` 可切换 ≥5 个移植主题；AI 配色在配置 key 后可用
- HTTP：`/v1/lyric` 与 Folia 协议对齐，外部程序可读取当前歌词
