# 音视频双向实时交互：设计与实施记录

> **状态：Phase 0/1/2a/2b 已落地**（2026-09-28；Phase 2b 随契约
> `tau:extension@0.3.0` 封印，`world realtime` + `examples/realtime-echo`
> + validate.sh 11b/11c/11d 三腿；**这两样已于 0.8.0 删除**，见下条）。
> **仅 Phase 3（wasip3 流 ABI）未落地**，
> 排期见 `docs/wasip3-streams.md`（Rust 1.100 解锁）。
> **代码不得先行于本文**：已落地部分同样以本文为准，改行为先改本文。
>
> **2026-09-29 更新**：上下行的 stream 形态**已随契约 0.7.0 提前落地**
> （component-model 异步 ABI，stable `wasm32-wasip2` 即可，无需等
> wasip3 std）——`session` 资源化，`uplink-audio(stream<u8>)` /
> `downlink() -> stream<event>` 取代了 Phase 2b 的
> `open/push-audio/push-image/close` 逐块调用形（那正是「ABI 载不动
> 连续流」时代的形状），`interrupt` 保留为调用。仍冻结到 Rust 1.100
> 的只剩 **guest 迁 `wasm32-wasip3` target** 一项（wasip3-streams.md
> 的解锁条件），接口形态不再等它。下文 Phase 2b 的记录保留原样
> （那是落地时的真实形状）。
>
> **2026-10-01 更新（契约 0.8.0 减法）**：**Phase 2b 的 WIT 封印已随
> 裁定 2/3 删除**——`world realtime`、`interface session` 与
> `microphone`/`camera` 两个 consent 门类都不在契约里了，
> `examples/realtime-echo` 目录已删。今天的形态是红线 1 的彻底化：
> **音视频全程宿主内部，媒体路径上不再有 guest**——订阅通道的
> `audio-delta` 臂只剩计数，observer 看得见助手在说话、永远听不到
> 声音。下文凡与本节冲突处，以本节为准；Phase 2b 的记录作为
> 「已删除」的历史保留。

## 现状家底（代码实证，2026-09-27）

| 方向 | 状态 | 证据 |
|---|---|---|
| 下行音频流 | ✅ 骨架已通 | `ModelEvent::AudioDelta{data, media_type}` → loop 广播 `AgentEvent::AudioDelta`（**字节随车**，Phase 1 起）并把同 media_type 连续块组装成 `Content::Audio`；wasm provider 有 `events.emit`；faux 测试覆盖组装与 wire |
| 下行播放 | ✅ Phase 1 | `audio::PlaybackSink` 挂两个渲染器，AudioDelta 即收即播（WAV 增量解析 / pcm rate= 参数，4s 环形缓冲，打断清缓冲，无设备降级 null sink 照常计数） |
| 上行（用户→模型） | ✅ Phase 0/2a | Phase 0：`/mic` 整段 `Content::Audio` 上行；Phase 2a：`/live` 全双工——`RealtimeSession.push_audio` 流式上行 + VAD/打断事件 + faux 替身 |
| 视频 | 数据模型有块 | `Content::Video/Image` 在；无 delta 事件、无采集 |
| 流 ABI | 冻结 | wasip3 streams 冻结到 Rust 1.100（`docs/wasip3-streams.md`）；现 base64-JSON 通道已验证够用 |

## 架构红线（从脊柱推导，不得破）

1. **采集/播放是宿主侧 I/O，永远不进 wasm guest**——WASI 无音频设备。
   0.8.0 起这条是结构性的：realtime world 删除后媒体路径上根本没有
   guest，设备门类（microphone/camera）也随之删除；宿主 CLI 按用户的
   显式命令动作（录制即授权），不为 wasm 侧保留任何设备门。
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

### Phase 2a 实施定稿（2026-09-28，宿主核心 + faux 替身 + CLI 全双工环）

Phase 2 按 Phase 0/1 的既定姿势再拆两刀：**2a 宿主先行**（trait +
事件 kind + faux 替身 + CLI 闭环，WIT 一字不动），**2b 契约殿后**
（WIT world `realtime` + consent 门类 microphone/camera + wasm 示例；
0.8.0 已全部删除——本节保留 2a/2b 的切分账目作为历史）。
理由与 Phase 0/1 相同：先把语义在宿主侧跑真，契约只封印已验证的
形状。

- **`RealtimeSession`（`Model` 的可选能力，tau-core）**：
  `push_audio(Vec<u8>) / push_image(Vec<u8>) / interrupt() /
  close()` 四个 `&mut self` 异步方法（`Result<_, String>`——关闭
  中的会话要能当场拒收）+ `events() -> BoxStream<ModelEvent>`
  （内部 broadcast，打开即取一次）。`Model::realtime(config)`
  默认 `None`——能力发现即此：request/response provider 零负担。
  `RealtimeConfig { input_media_type, output_media_type: Option,
  instructions: Option }`——上行裸流 `audio/pcm;rate=16000`
  （realtime 无容器，容器是持久会话的反面）。
- **新事件 kind（ModelEvent + AgentEvent 镜像）**：
  `InputAudioChunk`（上行事实，wire 上 base64 与 AudioDelta 同姿势；
  AgentEvent 侧只带计数——上行字节本来就在宿主本地，没有消费者
  需要它们二次上车，这是 Phase 1 教训的正面应用而不是重蹈）、
  `SpeechStarted/Stopped`（server VAD）、`Interrupted`（barge-in：
  loop 截断当前 assistant 音频组装——已组装的留下（用户就听到
  那儿），后续块开新段；播放 sink 清缓冲，走渲染器既有的
  clear 臂）。
- **时序零发明（红线 3）**：live 会话在总线上以合成
  `RunStart`/`RunEnd` 包裹——sink 计数、渲染、总结全部复用
  Phase 1 既有臂，不为 live 发明第二套渲染路径。
- **faux 替身（仅 `demo` 变体实现，`echo` 返回 None——能力发现
  有负例可断）**：确定性剧本——首块 `push_audio` 发
  `SpeechStarted`；每块回 `InputAudioChunk` 事实 + 同字节
  `AudioDelta` 回声（media_type 与上行相同——顺带把 Phase 1
  sink 的 pcm 裸流路径打进 e2e）；`interrupt()` → `Interrupted`
  + 截断回声段；`close()` → 若语音未止补 `SpeechStopped`，终
  `Done(Stop)`。
- **CLI `/live <秒> [sine]`**：开 live 会话（demo），采集任务按
  真实节奏推 50ms 块（sine 用 tokio interval 定速——"实时"不许
  是一股脑灌）；Ctrl-C = `interrupt()`（barge-in，REPL 不死，
  语义与既有 Ctrl-C=中断当前响应一致）；时长到或二次 Ctrl-C →
  close。落账：close 后把累计上行写成一条 user 消息
  （`Content::Audio`），assistant 组装（音频段 + 文本）写一条
  assistant 消息——与 run 后写账同形状。
- **consent**：2a 不需要新门类——宿主 CLI 按显式命令动作，与
  /mic 同权；microphone/camera 门类管的是 wasm provider 驱动宿主
  采集的场景，随 2b 落地。（0.8.0：门类随 world realtime 一并删除，
  这段是落地时的记录。）
- **验收**（validate.sh 11c，pty，两腿）：①`/live 2 sine` →
  断言 speech-started 行、sink announce 行（`audio/pcm;rate=
  16000 @ 16kHz`——pcm 路径）、close 后逐样本账目
  （32000 == 2s @ 16kHz 全回声）、会话 JSONL 双块在树；②
  `/live 30 sine` + 中途 Ctrl-C → 断言 interrupted 行出现且
  REPL 存活（再发 /quit 正常退出）。截断语义由单元测试精确
  覆盖（interrupt 后旧段冻结、新块开新段）。

### Phase 2b 实施定稿（2026-09-28，契约封印 + consent 门类 + wasm 示例）

2a 已在宿主侧把语义跑真；2b 把**已验证的形状**封印进契约，一字不多。

- **WIT（package 已是 0.3.0，本批并入）**：
  - `model-event` variant 增四案：`input-audio-chunk(audio-delta)`
    （复用 data+media-type record）、`speech-started`、`speech-stopped`、
    `interrupted`——与 tau-core 2a 的 kind 一一对应。
  - 新 interface `realtime`：`open(config-json) / push-audio(list<u8>)
    / push-image(list<u8>) / interrupt() / close()`，全
    `result<_, string>`（关门的会话当场拒收）。
  - 新 world `realtime { import events; import http; export models;
    export realtime; }`——发现（models）与流式请求（models.run）
    不丢：realtime 组件同时是普通 provider；下行推送复用
    `events.emit`，不发明第二条回传通道。**一个实例一条会话**
    （session-per-instance，open 即 instantiate）。
- **tau-ext**：`realtime_bindings` 模块；`emit` 增四臂（语义校验同
  既有姿势：空 media-type 拒收）；`ExtensionHost::load_realtime()`
  返回 `WasmRealtimeModel`（`stream()` 走 `models.run`，与
  WasmModel 同 idiom；`realtime()` 每调 instantiate 一条新实例、
  调 `open`、把事件通道接上 → `WasmRealtimeSession`）。guest
  trap = 该会话当场 `Err`（门拒收姿势），不毒化后续会话
  （session-per-instance 天然隔离）。
- **consent 门类（0.8.0 已删除，以下为落地时的记录）**：`RememberedConsent` 增 `microphone`/`camera`
  两个 sticky bool（与 wasi_deny/inject 同姿势）。**门类管的是
  设备，不是会话**：wasm realtime provider 驱动宿主采集真实
  麦克风须持 microphone 授予（`--microphone`，`--remember` 可记）；
  sine 合成路径不碰设备、不需要授予（门禁因此可无硬件跑全链）。
  camera 门类先行入册但**无采集路径即恒拒**——不为不存在的路径
  发明 UX。原生/demo provider 走宿主显式命令 doctrine（录制即
  consent），不需门类授予。
- **示例 `examples/realtime-echo`（0.8.0 已删除）**（world realtime）：与
  FauxRealtime 同剧本（首块 VAD-started + 文本注记；每块回
  input-audio-chunk 事实 + audio-delta 回声；interrupt →
  interrupted；close → speech-stopped + done）——**同一剧本两种
  载体**（Rust 原生替身 + wasm 组件），门禁两路互证。
- **验收**（validate.sh 11d，pty，两腿）：①无 `--microphone`：
  `/live 2`（真实麦路径）被拒且点名 `--microphone`，`/live 2 sine`
  照样通（设备语义而非会话语义）；②持授予 + sine：VAD 行、sink
  announce（pcm 裸流）、逐样本账目 32000、会话树双块——全链
  隔着 wasm 边界断言。

> 0.8.0 注：Phase 2b 的 WIT（`world realtime` / `interface session`）、
> 两个设备门类（`microphone`/`camera`）、`examples/realtime-echo`
> 与 validate.sh 11d 腿均已删除；上面是落地时的验收记录，保留作历史。

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
- [x] Phase 2：**2a + 2b 均已落地**（2026-09-28）：`RealtimeSession` trait
      （`Model::realtime` 能力发现，默认 None）+ 新事件 kind
      （`InputAudioChunk`/`SpeechStarted`/`SpeechStopped`/`Interrupted`，
      ModelEvent + AgentEvent 镜像，Interrupted 截断组装段并清 sink）
      + faux demo 替身（确定性 VAD + 全回声剧本）+ CLI `/live <sec>
      [sine]` 全双工环（合成 RunStart/RunEnd 复用 Phase 1 全部
      sink 臂，Ctrl-C = barge-in，close 后双块落账）；validate.sh
      11c 两腿 pty 门禁（账目 32000 逐样本精确 + barge-in 存活）。
      **2b**（同日落地；**0.8.0 已整体删除**——见文首 2026-10-01 更新）：
      WIT world `realtime`（interface `session`，
      `model-event` 增四案 realtime kind）+ consent 门类
      `microphone`/`camera` 入册（门类管设备不管会话——sine 路径无需
      授予；camera 无采集路径恒拒）+ `examples/realtime-echo`（与
      FauxRealtime 同剧本双载体互证）+ `is_realtime_component` 导出
      探针（读组件类型，不用错误驱动控制流）；validate.sh 11d 两腿
      + tau-ext realtime_echo 集成测试（跨界逐事件序列断言 + 关门
      拒收）
- [ ] Phase 3：按 wasip3-streams.md 执行
