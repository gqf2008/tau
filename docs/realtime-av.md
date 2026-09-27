# 音视频双向实时交互：设计草案

> **状态：设计草案，未落地。** 代码不得先行于本文。
> Phase 0/1 无契约依赖可先行；Phase 2 是契约 0.3.0 主菜之一，
> 与 `docs/host-channel.md` 同批设计；Phase 3 排期见
> `docs/wasip3-streams.md`（Rust 1.100 解锁）。

## 现状家底（代码实证，2026-09-27）

| 方向 | 状态 | 证据 |
|---|---|---|
| 下行音频流 | ✅ 骨架已通 | `ModelEvent::AudioDelta{bytes, media_type}` → loop 广播 `AgentEvent::AudioDelta` 并把同 media_type 连续块组装成 `Content::Audio`（`agent.rs:598`）；wasm provider 有 `events.emit({"kind":"audio-delta",...})`；faux 测试覆盖组装与 wire |
| 下行播放 | ❌ 只打印 | CLI 对 AudioDelta 只 `eprintln!("[tau] audio Δ N bytes")`（`main.rs:803`、`repl.rs:152`），无播放 sink |
| 上行（用户→模型） | ❌ 零 | 控制通道可送完整 `Message`（可含 Audio 块），但无采集、无流式上行、无打断语义 |
| 视频 | 数据模型有块 | `Content::Video/Image` 在；无 delta 事件、无采集 |
| 流 ABI | 冻结 | wasip3 streams 冻结到 Rust 1.100（`docs/wasip3-streams.md`）；现 base64-JSON 通道已验证够用 |

## 架构红线（从脊柱推导，不得破）

1. **采集/播放是宿主侧 I/O，永远不进 wasm guest**——WASI 无音频设备；
   mic/camera 走 consent 门类（per-fingerprint，UX 显示设备名，与
   process/http 同姿势），沙箱故事不被稀释。
2. **音视频块全是事件**（事实；高吞吐路径绝不放 probe，
   `docs/events.md` 规则 3）；**打断/会话控制走控制通道**（决定）。
3. 新通道不发明新时序：上行块与打断在现有 checkpoint 规则落地。

## 核心缺口：持久会话抽象

`Model` 今天是一回合一次「请求→流式响应」；OpenAI Realtime /
Gemini Live 是持久双向 socket（开会话 → 推入音频块 → 收事件，含
server VAD 与 barge-in → 关会话）。Phase 2 的本质就是补这个抽象。

## 分阶段路径

### Phase 0 — 按键对讲（零契约改动）

宿主侧录一段（cpal）→ 包成 `Content::Audio` 的 user `Message` 经控制
通道送入 → 现有多模态 provider 直接吃；下行在 run 结束后播放组装好
的 Audio 块。**双向成立但不实时**，用作端到端冒烟。

### Phase 1 — 下行实时化（纯宿主，契约零改动）

渲染层加播放 sink 订阅总线 `AudioDelta`，即收即播（环形缓冲 + 从
`media_type` 解析采样率喂输出流；打断时清缓冲）。就是把
`main.rs:803` 那行 `eprintln!` 换掉。

### Phase 2 — 全双工（契约 0.3.0 主菜）

- `tau-core` 加 `RealtimeSession`（`Model` 的可选能力）：
  `open(config) / push_audio(bytes) / push_image(jpeg) / interrupt() /
  events() / close()`。
- 新事件 kind：`InputAudioChunk`（上行事实）、`SpeechStarted/Stopped`
  （server VAD）、`Interrupted`（barge-in：用户开口 → provider 发
  interrupted → loop 截断当前 assistant 音频组装 + 播放 sink 清缓冲）。
- WIT 加 world `realtime`（wasm provider 版）：export 会话函数组，
  推送复用现有 `events.emit`。
- 视频上行 = 定时（~1fps）或场景触发 JPEG 帧走 `push_image`；下行
  视频 delta 等供应商真有了再加 kind，**不提前设计**。

### Phase 3 — wasip3 换 ABI

base64-JSON 块调用换 `stream<u8>`，按 `docs/wasip3-streams.md` 的
解锁条件执行。

## 带宽核算（为什么 Phase 2 不用等 wasip3）

24kHz × 16bit PCM = 48KB/s → base64 ≈ 64KB/s；按 50ms 一块 =
20 次 emit/s × ~3.2KB JSON——canonical ABI 每秒 20 次调用无压力
（参照：validate.sh 4b 步 3 MiB 单帧跨界逐字节核验通过）。
**瓶颈在抽象不在 ABI**，即冻结文档所说「realtime 形态的提供者今天
就能写」。

## 落地清单

- [ ] Phase 0：宿主采集 + `Content::Audio` 上行 + 播放组装块（冒烟）
- [ ] Phase 1：渲染层播放 sink（即收即播 + 打断清缓冲）
- [ ] Phase 2：`RealtimeSession` trait + 新事件 kind + WIT world
      `realtime` + consent 门类（microphone/camera）+ faux provider
      测试替身
- [ ] Phase 3：按 wasip3-streams.md 执行
