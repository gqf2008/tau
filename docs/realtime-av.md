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

### Phase 0 实施定稿（2026-09-28）

- **UX**：REPL 斜杠命令 `/mic <秒>` 录默认输入设备；`/mic <秒> sine`
  合成 440Hz 正弦（无硬件、确定可断言——门禁用这条）。录制即
  consent：宿主 CLI 本人按显式命令动作，红线 1 的 consent 门类管的是
  wasm guest，宿主自身不需能力门（与用户敲键盘输入文本同权）。
- **格式**：`Content::Audio{ media_type: "audio/wav" }`——WAV 容器
  自描述（采样率/位深在头部），下行播放与同媒体类型组装块零解析
  成本；16-bit PCM。base64 内联进会话 JSON（MediaSource::Bytes；
  blob 化是存储优化，不在 Phase 0 发明）。
- **下行**：run 结束后回放 assistant 消息里组装好的 Audio 块
  （cpal 默认输出设备；无输出设备 = 提示不是失败——无声卡环境
  不许红）。
- **测试替身**：`FauxModel::demo` 学会有音频输入时的应答——把上行
  WAV 的 PCM 采样拆成 2-3 个 `AudioDelta` 块回声下行（组装路径因此
  被真实走过）+ 文本注记。回声即闭环：录（或合成）→ 上行 →
  provider（demo 替身）→ AudioDelta → 组装 → 回放。
- **验收**（validate.sh 11b，pty）：`/mic 2 sine` → 断言会话 JSONL
  含 audio/wav 块、`[tau] ▶` 回放行出现。真实麦克风采的冒烟留
  手工路径（无硬件环境跳过不红）。
- 依赖：cpal 0.17（采集/播放）+ hound 3.5（WAV 编解码），仅
  tau-cli。

### Phase 1 — 下行实时化（纯宿主，契约零改动）

渲染层加播放 sink 订阅总线 `AudioDelta`，即收即播（环形缓冲 + 从
`media_type` 解析采样率喂输出流；打断时清缓冲）。就是把
`main.rs:803` 那行 `eprintln!` 换掉。

### Phase 1 实施定稿（2026-09-28）

- **sink 形态**：`audio::PlaybackSink` 随 `drive()` 创建，订阅渲染层
  的 `AudioDelta`（替换 main.rs/repl.rs 的 `eprintln!` 占位）；环形
  缓冲（`VecDeque<f32>`，上限 4s 音频，溢出丢最旧——实时语义下
  积压意味着已经听不到了）。输出流创建失败（无设备）→ null sink：
  照常收帧计数、不发声、不红——门禁在无音频硬件的机器上照样
  断言「流过了」。
- **media_type → 解码**：`audio/wav` 自描述——攒头 44+ 字节解析
  WAV 头后按流喂 PCM（Phase 0 的回声替身上下两块天然是同
  container 的连续字节）；`audio/pcm` 按 MIME 参数 `rate=` 解析
  （缺省 24000，OpenAI realtime 约定），16-bit 小端 mono。
- **打断清缓冲**：`Control::Abort` / Ctrl-C 路径调 `sink.clear()`；
  跨 media_type 切换（组装规则同）也清——残声不该漏进下一段。
- **总线修正（落地时发现）**：`AgentEvent::AudioDelta` 原本只带
  字节数（"bytes are not on the bus"），即收即播无米下锅——改为
  携带 `data: Vec<u8>`（tau-core Rust API 破坏性改动，计入
  0.3.0）。**契约零改动的承诺保住**：WIT 一字未动，wasm 订阅
  路径的 `audio-segment` 仍只给计数（无音频热路径进 guest），
  字节只为宿主渲染层而上车。
- **与 Phase 0 的交接**：delta 已即收即播，run 结束后**不再回放**
  组装块（听过的不重听）；Phase 0 的 run 后回放只保留给「无
  delta 的整块音频」（当前组装路径必经 delta，实为死路，但语义
  上留给未来非流式 provider）。
- **验收**（validate.sh 11b 升级）：sine 回声三 chunk 进 sink →
  断言 sink 计数行 `[tau] ▶ streamed N samples (audio/wav @
  16kHz)` 且 N == 完整 clip 采样数（逐字节对上，不是「有声音」
  级别的断言）；打断路径由单元测试覆盖（clear 后计数归零）。

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

- [x] Phase 0：宿主采集 + `Content::Audio` 上行 + 播放组装块
      （2026-09-28 落地）：`/mic <sec> [sine]` 录制/合成 → WAV 上行；
      demo 替身回声 3 个 AudioDelta；组装块 run 后回放（无输出设备
      仅提示不红）；validate.sh 11b pty 全链断言（sine 路径）
- [x] Phase 1：渲染层播放 sink（2026-09-28 落地）：
      `audio::PlaybackSink` 挂在两个渲染器（REPL + print）上，
      AudioDelta 即收即播（WAV 增量解析 / `audio/pcm;rate=` 裸流，
      环形缓冲 4s 溢出丢最旧，无输出设备降级 null sink 照常计数）；
      打断/换段清缓冲；`AgentEvent::AudioDelta` 为此携带字节
      （WIT 契约不变）；validate.sh 11b 断言逐样本账目
      （32000 == 2s @ 16kHz）
- [ ] Phase 2：`RealtimeSession` trait + 新事件 kind + WIT world
      `realtime` + consent 门类（microphone/camera）+ faux provider
      测试替身
- [ ] Phase 3：按 wasip3-streams.md 执行
