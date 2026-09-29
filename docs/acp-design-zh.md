# ACP 前端：把 tau 当作宿主背后的执行层

> **状态：设计草案（2026-09-28）；实现已于 2026-09-29 落地——本文入库时是设计记录，
> 不再是开工前置。** 与 `docs/im-channels.md` 的约定「代码不得先行于本文」在本轮被反向
> 打破（owner 批准计划后实现先行），如实记在这里。
> **与实现不一致处，以 `docs/acp.md` 为准**；已知三处：① 入口是 `--acp` 旗标，不是
> `tau serve --acp` 子命令；② 分帧是换行分隔的 JSON-RPC（SDK 的 framing），不是 §3 建议的
> `Content-Length`；③ `session/new` 的 `mcpServers` 被忽略并在 stderr 说明——本文 §6 主张
> 「宿主注入即宿主已授权」，该分歧**未裁决**，实现取了保守一侧。
> 关联 issue：#1（本文载体）、#2（内建工具层）、#3（sandbox tier / 权限中介）、
> #4（skills / AGENTS 加载）。
> 前置决议（阻塞开工，见 §2）：会话模型、能力协商语义、以及 #2×#3 的
> 「闸门在哪儿」。本文正文按**方案 A（一进程 N 会话）** 书写。

## 1. 动机

tau 今天是 CLI：print 模式（`-p`，一次一提示即退）与交互 REPL。全仓
没有任何 JSON-RPC / server 面（`rg -i acp` 零命中），`docs/im-channels.md:206`
把这一点写得很直白——「**tau 是单会话 CLI**」。

一个想让 tau 当**执行层**的宿主（例：桌面端接 IM 通道、每篇对话需要一个
本地 agent 跑回合）今天只能：**每回合起一个 CLI 进程、解析 stdout**。这条
路会丢四样东西，而且丢得没法补：会话连续性、回合取消、流式输出、权限中介。
本协议就是补上这个缺口的那一层。

**命名。** 本文的 ACP 指 **Zed 的 Agent Client Protocol**（JSON-RPC over
stdio，方法名即 `initialize` / `session/new` / `session/prompt` /
`session/cancel` / `session/update` / `session/request_permission`）。
**不是** AGNTCY 的 A2A。若将来要兼容后者，另开文档，不要在本文里混。

## 2. 必须先钉死的决议

### 2.1 会话模型（二选一，别留给实现）

- **方案 A（本文默认，推荐）：一进程 N 会话。** 宿主每 bot 起一条常驻
  `tau serve --acp`，进程内按 session key 多路复用。
  - 代价：CLI 现在的前提是「一次一个 `Agent` 实例 + 一个 `--session` 文件」
    （`crates/tau-cli/src/main.rs` 主流程、`repl.rs::interactive`）。要多一个
    `session key → (Agent, JsonlStore, ControlTx, EventStream)` 的注册表。
  - 收益：连续性 / 取消 / 流式天然成立；与 #4 的「按 session 的 cwd 加载
    skills 与 AGENTS」语义一致。
- **方案 B：一会话一进程。** 文档化即可，代价是把 `-p --session <f> --continue`
  做扎实，让 supervisor 能廉价重启（热启动约 24 ms，`docs/perf.md`）。进程
  隔离天然，只是 chattier。

**必须写进文档的是选哪个、以及为什么**——因为它同时决定了 #4 的加载范围
（按 session cwd 还是按进程 cwd），以及权限状态的存放粒度（会话级 vs 进程级）。

### 2.2 能力协商必须 fail-closed

- `initialize` 的返回里「协议版本 + capability 列表」是**必答项**。
- 宿主请求、而 agent 声明不了的能力 ⇒ **拒绝开会话**，不是静默降级到全权。
- 与 #3 的红线同源：给不了的保证要**响亮拒绝**。

### 2.3 闸门在哪儿（#2 × #3 必须一起拍）

- 若工具是 **tau-core 内建**（#2 的选项一）：tier / 权限可在 `before_tool`
  （`docs/probes.md` #5）**统一 gate**，是真闸。
- 若工具是 **wasm 组件**（#2 的选项二）：组件自带 ambient WASI，直接 import
  `wasi:filesystem` / `wasi:sockets` 即可绕过门控（`docs/extensions.md` §7，
  wit-review F1 原话：门控是 *declaration of intent, not a security boundary*），
  **tier 只能是 advisory**。

**结论先行（本文采用）：** ACP 的 sandbox tier **只对 agent 原生工具与宿主
驱动的会话做保证**，**不**对 wasm guest 的 ambient WASI 做任何声称——那是 F1
（owner 2026-09-28 裁定 A）的既有权衡，要改就单独立项。想约束 guest，现有
唯一硬手段仍是 per-fingerprint 的 `--deny-wasi`（all-or-nothing）。

## 3. 传输与帧

- **transport**：stdio。JSON-RPC 2.0。
- **分帧**：建议 LSP 式 `Content-Length` 头（与 Zed ACP 一致），不依赖换行。
- **client → agent（请求）**：`initialize`、`session/new`、`session/prompt`、
  `session/cancel`。
- **agent → client（通知）**：`session/update`。
- **agent → client（请求）**：`session/request_permission`。
- **版本协商**：client 提议协议版本，agent 择一或拒绝；未知必答项即拒（§2.2）。

## 4. 方法与 tau 现有构件的映射

| ACP 方法 | 落到 tau 的什么 | 备注 |
|---|---|---|
| `initialize` | **新写**（无对应） | 返回协议版本 + capabilities（§5） |
| `session/new {cwd, mcpServers?, sandboxTier?}` | `JsonlStore::open(<路径>)`（`session.rs:107`）+ `Agent::new(model, tools)` | 默认 `<cwd>/.tau/session.jsonl`；`mcpServers` 走既有 `--mcp-bridge` / `--mcp-command` 路径（consent 语义见 §6） |
| `session/prompt {sessionId, message}` | `Agent::run(&history, Message::user(text))`（`agent.rs:277`） | history 由 `store.active_branch(head)` 给出（`session.rs:282`） |
| `session/cancel` | `ControlTx::send(Control::Abort)`（`control.rs`） | abort 在下一个 stream 事件 / 工具边界生效，`RunEnd{stop: Aborted}`；「是否有回合在飞」需一个 in-flight 标志 |
| `session/update` | ← `AgentEvent`（`agent.rs:18`） | 见下表映射 |
| `session/request_permission` | **待建**：内核里从「模型决定」到「动作执行」之间目前没有任何宿主钩子（#3 的事实陈述成立） | 建议落点：`before_tool` 之后、执行之前插中介；宿主应答可缓存进会话级 allowlist |

`AgentEvent` → `session/update` 的建议映射：

| AgentEvent | session/update |
|---|---|
| `RunStart` / `RunEnd{stop}` / `RunError{message}` | turn 开始 / 结束 / 失败 |
| `TextDelta(String)` | 助手文本增量 |
| `ToolCallStart{id,name}` / `ToolCallEnd{id,name,is_error,output}` | 工具调用开始 / 结束（`output` 只发 compact 预览，别 dump） |
| `Probe{point,action}` / `ExtensionNotice` / `ExtensionFact` | 决策轨迹 / 通知 / 事实（可选，宿主按需订阅） |
| `Steer` / `FollowUp` / `Abort` | 控制面回执（宿主自己发的，回执用于对账） |
| `AudioDelta` / `InputAudioChunk` / `SpeechStarted` / `SpeechStopped` / `Interrupted` | **不直接内联进 JSON-RPC 文本帧**，见 §8 |

## 5. 能力声明（capabilities，初版字段）

`protocolVersion`、`sessionModel`（`"multiplex"` | `"per-process"`）、`steer`、
`cancel`、`permission`、`sandboxTiers[]`、`builtinTools[]`（#2 定型后回填）、
`mcpInjection`、`realtime`、`media`。

规则：

- 未知**必答** capability ⇒ 拒绝开会话。
- 会话请求的 `sandboxTier` 不在 `sandboxTiers` 里 ⇒ **在 `session/new` 就拒绝**
  （不要拖到第一次工具调用才报工具错误——宿主会在跑了几轮后才发现保证不成立）。

## 6. consent 与授权：谁在授权

今天 consent 是**按组件签名指纹**（bridge argv / origins、provider auth、WASI
deny），由**人类在 CLI 上**用 flag 授予并可用 `--remember` 固化
（`main.rs` 的 `recall_consent` / `maybe_remember`）。

宿主经协议驱动时，是**宿主替用户**授权。协议里必须写清三件事：

1. 宿主经 `session/new` 注入的 MCP server = **宿主已授权**（等价于一次
   `--mcp-command` consent）；组件侧的身份/完整性仍由签名指纹照旧约束。
2. `session/request_permission` 是**会话级、运行时**的新授权面，与
   per-fingerprint 的 remembered consent 是**两套东西**，不要混为一谈，也不
   互相顶替。
3. 红线：agent 不得把宿主未授予的能力当已授予；缺席即拒（fail-closed）。

## 7. 与既有构件的关系（不要重造内核）

- 协议层是这些的**序列化外壳**：事件总线（`docs/events.md`）、控制通道
  （`crates/tau-core/src/control.rs`）、session 树（`session.rs`）、九个探针点
  （`docs/probes.md`）。**不新造内核**。
- print 模式与 REPL 继续存在；`tau serve --acp`（名称待定）是**第三个入口**。
- `ingress` / bridge 是「**组件被 tau 加载**」；ACP 是「**tau 被宿主加载**」——
  方向相反，别用 bridge world 硬拼。

## 8. 媒体、音频与背压

- 文本走 JSON-RPC 帧；**媒体与音频大块不要内联进 JSON**（`docs/media.md` 的
  原则：字节在模型里，base64 只出现在 JSON 边上）。建议 `session/update` 只发
  引用 / 分片元数据，字节走带外（blob store 的 digest，或分片的 binary frame）。
- 实时语音要吃 Phase 1 的播放语义（chunk 到达即播、溢出丢最旧），别在协议层
  另编一套。
- 总线有界（1024，溢出发 `lagged(n)`）；协议层要有**等价的背压 / 丢弃策略并明说**，
  不能让一条慢宿主把 agent 卡住。

## 9. 验收（离线回环）

1. 起一条 `tau serve --acp`，回环客户端建**两个并发会话**，各跑一轮；中途
   `session/cancel` 其中一个；两个会话都收到流式 `session/update`。
2. 触发一次权限动作：`session/request_permission` 到客户端，客户端的「允许」
   与「拒绝」两条腿都被采纳并生效。
3. 未识别的**必答** capability ⇒ 会话被拒，错误信息点名缺什么。
4. `scripts/validate.sh` 增一条**离线**腿驱动整条回路（不依赖网络、不依赖真 key）。
5. 跑完会话文件仍是标准 JsonlStore：`tau tree` 能看形状，分叉/compaction 语义不变。

## 10. 未决 / 交给后续

- 是否兼容 AGNTCY A2A —— 另开文档，本文不混。
- stdio 之外是否需要 socket（宿主跨机场景）—— 本文**只锁 stdio**。
- `session/update` 的逐帧 schema（字段名、可选性）—— 建议另开
  `docs/acp-frames.md`，或在本文件 §4 表格后补齐。
- `session/request_permission` 的应答缓存策略（一次性 / 会话级 / 指纹级）。

> 本文尚未挂进 `README.md` 与 `docs/architecture.md` 的文档地图；落地时一并补。
