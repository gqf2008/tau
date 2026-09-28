# tau 架构设计

本文描述 tau 的整体架构，重点是**可插拔模块（wasm 组件）的设计**——它是
tau 与 pi 最大的分歧点，也是其余所有设计（签名、授权、能力、分发）的
围绕中心。各子系统的操作细节另有专文（见文末「文档地图」），本文负责
把它们拼成一张图并说明**为什么这样切**。

## 1. 设计目标与源流

tau 是一个最小 agent harness，设计沿袭 pi（MIT, earendil-works/pi）的三
个核心决定：

1. **会话即树**：append-only JSONL，条目带 id + parent；根到当前条目的
   路径是 active branch，供作模型历史；从旧条目继续即分叉。compaction
   以摘要条目替换更早历史，原件留在树里。
2. **极简 agent 循环**：prompt → model stream → tool calls → results →
   重复。steering / follow-up / abort 走独立的控制通道。
3. **一切皆可扩展**：核心刻意保持最小，功能长在扩展上。

分歧点在第 3 条的实现方式：pi 的扩展是进程内 TypeScript 模块，tau 的
扩展是 **wasm 组件**。这一个选择换来了四样东西——

- **故障隔离**：扩展 trap 只杀死它自己的那一次调用，宿主重建实例后续
  跑；进程内脚本的一个 panic 就是整个 harness 的 panic。
- **语言中立**：任何能编到 wasm32-wasip2 的语言都能写扩展
  （C/C++/Python/JS/TS/Go 实测矩阵见 `docs/wasm-languages.md`）。
- **单一可分发生产物**：一个 `.wasm` 文件，可内嵌签名、可推 OCI
  registry——「扔一个 .wasm 进去就能扩展」。
- **能力边界**：组件能做什么由它 import 什么 + 宿主授予什么决定，
  沙箱是 wasm 运行时的本性，不需要宿主额外发明。**但默认策略下这句话
  要打折**：ambient WASI 默认全开（全宿主 FS 读写 + 网络 + env/argv），
  scoped 的 http/process 门可被 guest 直接用 `wasi:sockets` /
  `wasi:filesystem` 绕开——门是意图声明，不是墙。细节与收紧开关见
  `docs/extensions.md` §7（wit-review F1 裁定 A，2026-09-28）。

代价也明确：所有跨边界数据必须序列化（tau 选择了 JSON-over-string 的
朴素线形），跨边界调用是同步的，大载荷要整体拷入 guest 线性内存
（已钉：>10 MiB 的 request JSON 逐字节完整到达）。wasip3 的原生流式
ABI 是已知的未来改进项，当前边界先行钉死。

## 2. 分层总览

```
┌─────────────────────────────────────────────────┐
│ tau-cli   tau 二进制：print / REPL 两种模式，      │
│           子命令（sign/trust/consent/gc/push…）， │
│           授权解析与合并（flags + remembered）      │
├─────────────────────────────────────────────────┤
│ tau-openai / tau-anthropic   内置 provider       │
│ （三个 API：chat completions / Responses /        │
│  Messages），实现 tau-core 的 Model trait        │
├─────────────────────────────────────────────────┤
│ tau-ext   wasmtime 组件宿主：三个世界的加载器、    │
│ 实例生命周期（trap→revive）、签名/信任/同意存储、 │
│ scoped 能力（process/http）的宿主实现、OCI 分发   │
├─────────────────────────────────────────────────┤
│ tau-core  域模型：session 树、agent 循环、Model/  │
│ Tool/Probe 注册表、事件总线、控制通道、blob 存储、 │
│ faux 模型（离线演示与测试）                       │
├─────────────────────────────────────────────────┤
│ wit/tau.wit   扩展契约（版本化 tau:extension@x）   │
└─────────────────────────────────────────────────┘
```

依赖方向严格向下：`tau-cli → tau-ext → tau-core`，`tau-openai/anthropic
→ tau-core`。tau-core 不知道 wasm 的存在；tau-ext 不知道 CLI 的存在；
契约文件（WIT）不依赖任何 crate——组件作者 vendor 它即可。

## 3. 核心域模型（tau-core，简述)

- **Model trait**（`model.rs`）：流式接口，首要契约是**永不 panic**——
  错误作为终末流事件（`error` + `done{stop:"error"}`）传播。内置
  provider、wasm provider、faux 模型都在这一个 trait 后面。第二种
  交互模式是 `realtime(config) -> Option<RealtimeSession>`：全双工会话
  （上行 push_audio/push_image，下行 typed ModelEvent 流，interrupt
  打断即冻结），默认 `None`——能力发现就是调用本身
  （`docs/realtime-av.md`）。
- **Agent 循环**（`agent.rs`）：每个 turn 组装 request（system + active
  branch + 本轮产出 + tools），探针在九个点介入（见 §4.7），tool call
  执行结果回到历史，循环到 stop reason 或 max turns。
- **三个通道，职责不混**（`docs/events.md`）：
  - **事件总线**（`bus.rs`，广播，容量 1024）：事实。慢订阅者收
    `Lagged` 跳过，永不能卡住循环。
  - **探针**（`probe.rs`，同步 request→verdict）：决策。想影响执行
    必须走这里。
  - **控制通道**（`control.rs`，无界 mpsc）：命令（steer/follow-up/
    abort），循环在检查点消费，steer 永不落在 tool_use 与
    tool_result 之间。
- **媒体与 blob**（`types.rs`/`blobs.rs`）：内存里是诚实字节，base64
  只存在于 JSON 边缘；>256KB 的媒体在会话写入时外置到内容寻址 blob
  存储，请求边缘物化回来；`tau gc` 全树标记清扫。

## 4. 可插拔模块设计

### 4.1 契约：三个 world，两种能力，一条推送通道

契约是 `wit/tau.wit`（版本化——那里的 `package` 行即权威，当前
`tau:extension@0.5.0`）。它按「组件扮演什么角色」切成三个 world，
而不是一个大接口：

| world | export | import | 角色 |
|-------|--------|--------|------|
| `extension` | `tools`（definitions/execute）、`probes`（points/probe） | `host`（notify/emit/steer/follow-up；注入类过 consent） | 通用扩展：给 agent 加工具、在生命周期点上观察与影响、经宿主通道回传 |
| `provider` | `models`（list-models/run） | `events`、`http` | 模型 provider：推送式流式输出，网络走授权出口 |
| `bridge` | `tools` | `process`、`http` | 桥：把外部工具协议（MCP）翻译成 tau 工具 |

两个 **capability interface**（`process`、`http`）是刻意朴素的数据
接口（handle + list\<u8\>，无 wasi:io/wasi:http 依赖，无 MCP 形状）：
宿主能力，不是协议知识。`events` 是 provider 的推送通道，宿主永远
提供，不算能力。

设计要点：**普通扩展链接不到 `process`/`http`**。world 的划分就是
能力的第一道边界——一个只做工具的组件在链接层面就拿不到 spawn。

### 4.2 加载管线

三个 world 共用同一条管线（`tau-ext/src/lib.rs`、`bridge.rs`）：

```
path/oci:// → read_verified（签名+信任策略，先于编译）
           → Component::from_binary（wasmtime，带编译缓存）
           → Linker：wasi p2（按 WasiPolicy）+ tau 接口
           → instantiate
           → 加载期契约调用（见下）
           → 注册进 tau-core 注册表
```

- **信任先于编译**：`TrustPolicy::RequireTrusted`（CLI 默认）下，未
  签名/未信任的字节在编译前就被拒绝；`--allow-unsigned` 只豁免
  *缺失*的签名——签名节存在但验不过，任何策略都拒（缺失是开发者
  选择，腐坏是篡改证据，逃生舱不得洗白它）。
- **加载期契约调用**：`definitions()`、`points()`、`list-models()`
  在加载时各调一次。这有两个作用：一是注册表需要它们；二是**契约
  在加载点被强制**——`--model` 给了一个组件没广告的 id，加载即拒
  并列出可用 id（曾经不查，拼错的 id 静默照跑）。
- **注册进核心**：`LoadedExtension` 拆出 `Vec<Box<dyn Tool>>` 与
  `Vec<Box<dyn ProbeHandler>>`，wasm 组件从此在 core 眼里与任何
  原生工具/探针无异。core 不知道 wasm——适配层在 tau-ext。

### 4.3 实例生命周期与故障语义

每个已加载组件是一个 `SharedInstance`（store + 工厂），包在
`Arc<Mutex<…>>` 里跨调用复用；每次调用经 `spawn_blocking` 进阻塞
线程，事件经 channel 流回异步侧。三条规则：

1. **trap 只杀死当前调用**：工具 trap → 这次调用变成 `is_error` 的
   工具结果；探针 trap → 降级为 `continue`；provider trap → error
   事件 + `done{stop:"error"}`。run 永不被楔死。
2. **trap 后实例重建**（`revive()`）：trap 会污染 store，用工厂重新
   实例化，后续调用落到全新实例上——一次崩溃不影响会话的其余部分，
   也不静默死掉。
3. **poisoned lock 恢复**：持锁 panic 后锁被取回而不是让工具永久
   失效。

编译缓存（`~/.tau/cache/wasmtime`）让组件加载从冷 ~200ms 降到热
~10ms；缓存初始化失败只是变慢，不会失败。

### 4.4 信任与授权：两个正交的问题

签名与沙箱回答的是不同问题，文档与实现都保持它们正交：

- **签名回答「这是谁的、被改过没有」**：ed25519 签名以
  `tau-signature` 自定义节内嵌在 .wasm 里（签的是剥掉签名节后的
  SHA-256，重签名良定义）。指纹（公钥 SHA-256 前 16 hex）就是作者
  id。`tau trust --from-component` 从验证过的字节里 onboarding 公钥
  ——但指纹必须带外核实。
- **沙箱回答「它能做什么」**：见 §4.5。信任的组件也不会多得任何
  能力。

签名同时还**承载授权记忆**：capability grants 按指纹记在
`~/.tau/consent/<fingerprint>.json`（origin 并集、布尔粘滞、
`--remember` 只增、`--revoke` 才减）。同一 key 签名的后续版本继承
用户授权；未签名组件没有指纹，永远没有记忆。同意文件的 key 在存储
层校验形状（16 小写 hex），`../escape` 式输入在任何调用点都过不了；
腐坏文件读作缺席——门关上，不是放行。

### 4.5 能力模型：ambient 与 scoped 两层

| 层 | 内容 | 默认 | 收紧方式 |
|----|------|------|----------|
| **ambient WASI** | fs/env/stdio/args/network | AllowAll（继承宿主环境） | `--deny-wasi`（可按指纹记忆） |
| **scoped 能力** | `process`（按 argv）、`http`（按 origin）、凭证投递 | 空 | 只有显式同意才授予 |

这张表说的是机制，不是边界：scoped 的门只对愿意走门的组件成立，ambient
层的实际可达面是整个宿主进程级的东西（全宿主 FS 读写 + 网络 + env/argv），
且可被 guest 绕开。诚实表述与收紧开关见 `docs/extensions.md` §7。

scoped 能力的一组共同语义，是整套安全设计的骨架：

- **「给即同意」（giving IS the consent）**：`--mcp-command` 的 argv、
  `--provider-origin` 的 origin、`--provider-auth` 的 token——传给
  宿主这个动作本身就是授权，不存在第二份配置漂移的可能。桥只经
  `TAU_MCP_COMMAND`/`TAU_MCP_URL` 两个环境变量得知自己被允许做什么。
- **调用时失败，而非实例化时失败**：能力永远链接、默认授予为空。
  没授权的组件照样加载，到真正调用那一刻才 permission-denied——
  错误发生在离用户决定最近的地方。
- **秘密投递不落盘**：bearer token 由用户显式交给宿主，注入每个
  request 的 `auth.bearer`；宿主从不持久化秘密本身，可以记忆的只有
  *投递授权*（记忆后 `TAU_PROVIDER_AUTH` 才流得动）。
- **出口检查器与客户端同语义**：`http` 能力的 origin 检查与 reqwest
  解析 URL 的方式一致（authority 止于 `/ ? # \`，userinfo、尾点、
  端口规范化全部对齐并已做差分对账），且**重定向永不跟随**——
  跟随就等于把请求搬到用户没同意的 origin。

### 4.6 探针：影响执行的唯一通道

九个已接线点覆盖 run 的完整生命周期（`docs/probes.md` 有载荷表）：
`before_run → transform_context → before_request → after_response →
before_tool → after_tool → before_run_end`，外加 `before_compaction`
与 `before_navigation`。verdict 三态：`continue` / `replace(+payload)`
/ `block(+reason)`。

规则（与 pi 的 HookMap 同源）：

1. 注册序折叠：每个探针看到上一个的 replace 结果；第一个 block 胜。
2. block 的语义按点定义：`before_tool` 的 reason 作为工具结果回给
   模型（模型能看到否决并反应）；`before_navigation`  veto 跳转；
   其余作为 run 错误。
3. 探针同步——harness 暂停等 verdict，热路径上的探针必须快；高
   频路径（text/audio delta）因此是 observe-only 事件，不开探针。
4. trap 降级为 `continue` 并重建实例（§4.3）：坏扩展不得楔死
   harness，也不得静默变瞎——非平凡 verdict 同时发布到事件总线，
   决策轨迹不丢。

### 4.7 Provider 组件

- **推送式流式**：组件逐块调 `events.emit(json)`（text-delta /
  audio-delta / tool-call-delta / done / error），`run()` 返回即结束。
  宿主把 channel 里的事件反序列化成 `ModelEvent` 流——组件在 core
  眼里就是一个普通 `Model`。
- **模型清单强制**：`--model` 必须命中 `list-models()`（§4.2）。
- **永不 trap 契约**：请求/传输失败走 error 事件 + `done{stop:
  "error"}`；trap 是组件违约，宿主按 §4.3 兜底但不鼓励。
- **网络出口**走 §4.5 的 `http` 能力：与桥同一条 consent-gated 通道、
  同一个宿主实现（`http.rs`）、同一套 origin 语义。

### 4.8 Bridge 组件：核心不知道 MCP 存在

核心**故意没有 MCP**——MCP 只是众多工具协议之一，进核心会让每次
发布耦合它的演进。桥组件把外部协议翻译成 tau 工具（wassette 思路
的倒置：它把组件暴露成 MCP 工具，tau 的桥消费 MCP server 并把它的
工具暴露给 agent）。参考实现 `examples/mcp-bridge`：stdio + streamable
HTTP 双传输、协议版本协商（不会说的版本大声拒绝，不糊弄分歧语义）、
单条消息 16 MiB 上限（防内存淹没）、服务器中途死亡后 respawn +
重握手。宿主对协议零感知——它只授予 spawn/http 能力。

### 4.9 分发：OCI 与「一个文件」哲学

扩展就是单个 `.wasm`，所以任何 OCI registry 都是分发渠道
（`docs/oci.md`）：`tau push` 走标准 registry v2 流程（bearer-token
dance、wasm layer mediaType 约定），`oci://` 引用可用于一切接受组件
路径的 CLI 参数。拉取侧：manifest 每次新取（可变 tag 才能看到新
digest）、blob 先验 sha256 再落盘、内容寻址缓存命中校验、腐坏缓存
重拉而不是喂坏字节。**拉回来的字节走与本地文件完全相同的加载路径**
——签名、信任、按指纹的授权记忆原样生效。OCI 解决「从哪来」，
签名解决「是谁的」，两者正交。

## 5. 横切设计不变量

这些是从逐轮证伪巡检沉淀下来的、写进测试与 validate.sh 的不变量，
新代码必须维持：

1. **fail-closed 方向**：安全检查的两个失败方向里，「误判拒绝」是
   安全的（误伤），「误判放行」是漏洞。所有 gate 的规范化歧义
   （尾点、空端口、IDN、大小写）一律向拒绝侧倒。
2. **检查器与执行路径同语义**：安全检查的解析器必须和它守护的执行
   者解析同一份输入的方式一致（origin 检查 vs reqwest 是最痛的一
   课）；最强的钉是差分一致测试——gate 接受的每个 URL，客户端必须
   解析出同一个 origin。
3. **宽松旗标只豁免它点名的状态**：`--allow-unsigned` 豁免缺失，
   永不豁免腐坏。每个 escape hatch 都要回答「它洗白什么、不洗白
   什么」。
4. **契约必须在选择点强制**：广告-选择分离的接口（list-models vs
   --model），不强制等于没有契约——而强制之日先违规的往往是自己
   的测试套件。
5. **存储层自己校验键形状**：键拼文件名的存储（consent/trust/
   keys），形状校验落在副作用发生的那一层，不信生产者的
   不变量。
6. **降级必须带恢复力**：单点降级（trap→continue/error）不等于
   容错，每个降级声称都要钉「之后还能正常干活」。

## 6. 明确不做什么

- **核心无 MCP**（§4.8）、核心无 wasm（tau-ext 才碰 wasmtime）、
  核心无网络。
- **不做 secret 保管**：宿主只按用户当次的显式交付转发秘密，记忆
  的永远只是授权。
- **不发明沙箱**：能力边界就是 wasm 运行时 + world 划分 + scoped
  能力的宿主实现，没有额外的进程隔离层（被 spawn 的 MCP server 是
  用户自己选的风险，与原生 MCP 客户端相同）。
- **wasip3 流式 ABI**：当前边界是「字符串整体拷入 guest」，大 ABI
  改造是保留给未来的显式决策，不被动滑入。工具链现状已 spike
  钉死：guest 侧 stable 阻塞至 Rust 1.100（预计 2026-11），host
  侧就绪——解锁条件与迁移草案见 `docs/wasip3-streams.md`。

## 7. 文档地图

| 文档 | 内容 |
|------|------|
| `wit/tau.wit` | 扩展契约（先读这个） |
| `docs/extensions.md` | 扩展作者指南：scaffold → 三 world → 签名 → OCI |
| `docs/probes.md` | 九个探针点的载荷与 verdict 语义 |
| `docs/events.md` | 事件总线 / 探针 / 控制通道的三通道模型 |
| `docs/bridges.md` | 桥的能力模型与 MCP 参考实现 |
| `docs/signing.md` | 签名格式、信任存储、授权记忆 |
| `docs/oci.md` | OCI 分发链路 |
| `docs/media.md` | 多模态与 blob 存储 |
| `docs/realtime-av.md` | 实时音视频：RealtimeSession、WIT world realtime、设备 consent 门类 |
| `docs/wasip3-streams.md` | wasip3 stream 迁移的工具链现状与解锁条件 |
| `docs/release.md` / `docs/perf.md` | 发布流程 / 性能基线 |
