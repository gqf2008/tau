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
   以摘要条目替换更早历史，原件留在树里。run 中途死亡的已产出内容也
   不丢：流式帧持久化在 sidecar（见 §3 帧级崩溃恢复）。
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
│ tau-cli   tau 二进制：print / REPL / --acp          │
│           三种模式，子命令（sign/trust/             │
│           consent/gc/push…），授权解析与合并        │
│           （flags + remembered）                    │
├─────────────────────────────────────────────────┤
│ tau-openai / tau-anthropic   内置 provider       │
│ （三个 API：chat completions / Responses /        │
│  Messages），实现 tau-core 的 Model trait        │
├─────────────────────────────────────────────────┤
│ tau-ext   wasmtime 组件宿主：三个世界的加载器、    │
│ 实例生命周期（trap→revive）、签名/信任/同意存储、 │
│ scoped 能力（process/http）的宿主实现、OCI 分发   │
├─────────────────────────────────────────────────┤
│ tau-tools 内置工具：read/write/edit/ls/grep/find/ │
│ bash/powershell。宿主代码（非 wasm），默认注册，   │
│ 组件工具同名可覆盖                                │
├─────────────────────────────────────────────────┤
│ tau-core  域模型：session 树、agent 循环、Model/  │
│ Tool/Probe 注册表、事件总线、控制通道、blob 存储、 │
│ faux 模型（离线演示与测试）                       │
├─────────────────────────────────────────────────┤
│ wit/tau.wit   扩展契约（版本化 tau:extension@x）   │
└─────────────────────────────────────────────────┘
```

依赖方向严格向下：`tau-cli → tau-ext → tau-core`，
`tau-cli → tau-tools → tau-core`，`tau-openai/anthropic → tau-core`。
tau-core 不知道 wasm 的存在；tau-ext 不知道 CLI 的存在；
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
- **帧级崩溃恢复**（`frames.rs`）：turn 原本是原子的——run 完成才
  append 条目，进程中途死亡丢失整轮已产出内容。现在 agent 循环把
  每个流式增量与每个已落地工具结果写成一帧（可选 sink，REPL 与
  print 模式开启），帧落在 sidecar `<session>.frames.jsonl`——
  **不进会话文件**：冻结期新增行型会被旧二进制读成 corrupt（与
  `/name` 裁定同源）。帧是进度不是历史，永不过进上下文。干净 run
  条目落盘即 retire sidecar；sidecar 幸存意味着 run 死亡，下次打开
  （或 REPL 当场）由 `salvage` 重建已提交前缀为真实条目：部分
  assistant 消息附 pi 原文的 interrupted 通告，未落地结果的调用
  合成 "external outcome is unknown" 的诚实结果；通告即幂等标记。
- **入口不新增核心构件**：print / REPL / ACP 三种模式共用同一份
  `Agent` + session 树 + 工具与探针注册表；ACP 只是这些构件的
  JSON-RPC 序列化外壳（`docs/acp.md`），tau-core 不知道协议存在。
- **媒体与 blob**（`types.rs`/`blobs.rs`）：内存里是诚实字节，base64
  只存在于 JSON 边缘；>256KB 的媒体在会话写入时外置到内容寻址 blob
  存储，请求边缘物化回来；`tau gc` 全树标记清扫。

## 4. 可插拔模块设计

### 4.1 契约：两个 world，四种能力，资源与流

契约是 `wit/tau.wit`（版本化——那里的 `package` 行即权威，当前
`tau:extension@0.8.0`）。它按「组件扮演什么角色」切成两个 world，
而不是一个大接口：

| world | export | import | 角色 |
|-------|--------|--------|------|
| `extension` | `tools`（definitions/execute）、`probes`（points/probe） | `host`（notify/emit/steer/follow-up/subscribe） | 通用扩展：给 agent 加工具、在生命周期点上观察与影响、经宿主通道回传 |
| `bridge` | `tools`、`probes`、`bridge-io`、`ingress-handler` | `process`、`http`、`ws`、`host`、`ingress` | 桥：把外部协议（MCP、IM 长连/webhook）翻译成 tau 工具与入站消息 |

0.8.0 删掉了 `world provider`、`world realtime`、`interface models`
与 `interface session`：模型不是扩展点，模型集合由宿主内置并随发布
节奏演进（迁移说明见 `docs/extensions.md` §5）。

四个 **capability interface**（`process`、`http`、`ws`、`ingress`）
只表达宿主能力，不表达协议知识（无 wasi:http、无 MCP/IM 形状）。
0.7.0 起句柄全部资源化（`child` / `response` / `connection` /
`registration`，drop 即释放），等待全部换成 stream/future——「还没
好」由 guest 自己的 await 表达，`timeout-ms` 参数随之全部下线，
预算收进宿主旋钮（`TAU_*_TIMEOUT_MS`，每条拒绝自报预算名与时长）。
0.8.0 之后 stream/future 只活在 `bridge` world：没有模型侧组件，
async ABI 从「写扩展」的属性变成「跟外部世界说话」的属性。
（`host.subscribe` 与 `ws.poll` 的拉形态本身留到 0.9.0 的调用约定
统一，见 `docs/extensions.md` §4/§6 的「0.9.0 待改」。）

设计要点：**普通扩展链接不到 `process`/`http`**。world 的划分就是
能力的第一道边界——一个只做工具的组件在链接层面就拿不到 spawn。
0.8.0 把这句话变成了全部：门撤了，world 本身就是声明（§4.5）。

### 4.2 加载管线

两个 world 共用同一条管线（`tau-ext/src/lib.rs`、`bridge.rs`）：

```
path/oci:// → read_verified（签名+信任策略，先于编译）
           → Component::from_binary（wasmtime，带编译缓存）
           → Linker：wasi p2（ambient 全开；0.8.0 起无策略开关）+ tau 接口
           → instantiate
           → 加载期契约调用（见下）
           → 注册进 tau-core 注册表
```

- **信任先于编译**：`TrustPolicy::RequireTrusted`（CLI 默认）下，未
  签名/未信任的字节在编译前就被拒绝；`--allow-unsigned` 只豁免
  *缺失*的签名——签名节存在但验不过，任何策略都拒（缺失是开发者
  选择，腐坏是篡改证据，逃生舱不得洗白它）。
- **加载期契约调用**：`definitions()`、`points()` 在加载时各调一次。
  这有两个作用：一是注册表需要它们；二是**契约在加载点被强制**——
  `definitions()` 里解析不了的 `parameters-json` 让整个加载失败并点名
  工具，绝不静默放宽成开放 schema（wit-review F5）。
- **注册进核心**：`LoadedExtension` 拆出 `Vec<Box<dyn Tool>>` 与
  `Vec<Box<dyn ProbeHandler>>`，wasm 组件从此在 core 眼里与任何
  原生工具/探针无异。core 不知道 wasm——适配层在 tau-ext。

### 4.3 实例生命周期与故障语义

每个已加载组件是一个 `SharedInstance`（store + 工厂），包在
`Arc<Mutex<…>>` 里跨调用复用；每次调用经 `spawn_blocking` 进阻塞
线程，事件经 channel 流回异步侧。三条规则：

1. **trap 只杀死当前调用**：工具 trap → 这次调用变成 `is_error` 的
   工具结果；探针 trap → 降级为 `continue`（0.8.0 删掉 world
   provider 后，组件侧不再有第三种调用形态）。run 永不被楔死。
2. **trap 后实例重建**（`revive()`）：trap 会污染 store，用工厂重新
   实例化，后续调用落到全新实例上——一次崩溃不影响会话的其余部分，
   也不静默死掉。
3. **poisoned lock 恢复**：持锁 panic 后锁被取回而不是让工具永久
   失效。

编译缓存（`~/.tau/cache/wasmtime`）让组件加载从冷 ~200ms 降到热
~10ms；缓存初始化失败只是变慢，不会失败。

### 4.4 信任：签名只回答「这是哪个组件」

签名回答的是「这是谁的、被改过没有」：

- ed25519 签名以 `tau-signature` 自定义节内嵌在 .wasm 里（签的是
  剥掉签名节后的 SHA-256，重签名良定义）。指纹（公钥 SHA-256 前
  16 hex）就是作者 id。`tau trust --from-component` 从验证过的字节里
  onboarding 公钥——但指纹必须带外核实。
- **它不回答「它能做什么」**：0.8.0 起组件恒得 ambient WASI（§4.5），
  以 tau 进程的权限运行；唯一与组件一一对应的授权动作是**安装/信任
  签名**那一次，安装界面展示的声明读自组件类型（imports/exports），
  不是手写清单。

0.8.0 删掉了整套授权记忆：`~/.tau/consent/*`、`--remember`、
`tau consent --list/--revoke` 都不在了。指纹本身留下——它是作者
id，也是「这是哪个组件」的答案，只是不再承载任何 capability
grant。

### 4.5 能力模型：ambient 是唯一的 posture

| 层 | 内容 | 默认 | 收紧方式 |
|----|------|------|----------|
| **ambient WASI** | fs/env/stdio/args/network | 恒开（继承宿主环境） | 无——0.8.0 删了 `--deny-wasi` 与 `WasiPolicy`；要边界就在 OS 层围住 tau 进程 |
| **world 划分** | `process`/`http`/`ws`/`ingress` 只出现在 `bridge` world | 由组件类型决定 | 不是门：链接得到即用得到，调用时不再检查 |

这张表说的是机制，不是边界：ambient 层的实际可达面是整个宿主进程级的
东西（全宿主 FS 读写 + 网络 + env/argv），且可被 guest 直接 import
`wasi:sockets` / `wasi:filesystem` 绕开。诚实表述见
`docs/extensions.md` §7（wit-review F1）。

0.8.0 之后剩下的一组语义，是整套设计的骨架：

- **world 即声明**：一个只做工具的组件拿不到 spawn——不是因为在调用
  时被拒，而是因为 `extension` world 根本不 import `process`。
- **安装即授权**：唯一一次授权动作是安装/信任签名。`--mcp-command`
  的 argv、`--mcp-url`、`--ingress` 的监听地址都是宿主配置，不是
  运行时许可；桥只经 `TAU_MCP_COMMAND`/`TAU_MCP_URL` 两个环境变量
  得知自己被配置成了什么。
- **重定向永不跟随**：`http` 不设 origin 白名单（0.8.0 删了），但
  **重定向永不跟随**——跟随就等于把请求搬到组件没点名的 endpoint。
- **不做秘密保管**：宿主从不持久化任何秘密；0.8.0 随 world provider
  一起删掉了凭证投递（`--provider-auth`/`TAU_PROVIDER_AUTH`），内置
  provider 的凭据直接读环境变量。

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

### 4.7 Provider 组件（0.8.0 删除）

模型不是扩展点：`world provider` / `world realtime` /
`interface models` / `interface session`，连同 `--provider-wasm` /
`--provider-origin` / `--provider-auth`，一起删除。模型集合封闭、
由本仓库内置并随发布节奏演进，逃生舱是 OpenAI 兼容的 base URL；
媒体面全程宿主内部（§4.5、`docs/realtime-av.md`）。0.7.0 的本节
内容见 `CHANGELOG.md` 历史与仓库历史。

### 4.8 Bridge 组件：核心不知道 MCP 存在

核心**故意没有 MCP**——MCP 只是众多工具协议之一，进核心会让每次
发布耦合它的演进。桥组件把外部协议翻译成 tau 工具（wassette 思路
的倒置：它把组件暴露成 MCP 工具，tau 的桥消费 MCP server 并把它的
工具暴露给 agent）。参考实现 `examples/mcp-bridge`：stdio + streamable
HTTP 双传输、协议版本协商（不会说的版本大声拒绝，不糊弄分歧语义）、
单条消息 16 MiB 上限（防内存淹没）、服务器中途死亡后 respawn +
重握手。宿主对协议零感知——它只提供 world 声明的那几个能力
（spawn/http/ws/ingress）。

### 4.9 分发：OCI 与「一个文件」哲学

扩展就是单个 `.wasm`，所以任何 OCI registry 都是分发渠道
（`docs/oci.md`）：`tau push` 走标准 registry v2 流程（bearer-token
dance、wasm layer mediaType 约定），`oci://` 引用可用于一切接受组件
路径的 CLI 参数。拉取侧：manifest 每次新取（可变 tag 才能看到新
digest）、blob 先验 sha256 再落盘、内容寻址缓存命中校验、腐坏缓存
重拉而不是喂坏字节。**拉回来的字节走与本地文件完全相同的加载路径**
——签名与信任原样生效。OCI 解决「从哪来」，
签名解决「是谁的」，两者正交。

## 5. 横切设计不变量

这些是从逐轮证伪巡检沉淀下来的、写进测试与 validate.sh 的不变量，
新代码必须维持：

1. **fail-closed 方向**：安全检查的两个失败方向里，「误判拒绝」是
   安全的（误伤），「误判放行」是漏洞。0.8.0 撤掉调用时门之后，
   每一次剩下的判断（签名节腐坏、存储键形状）仍然一律向拒绝侧倒。
2. **检查器与执行路径同语义**：安全检查的解析器必须和它守护的执行
   者解析同一份输入的方式一致（origin 检查 vs reqwest 是 0.7.0 最
   痛的一课，那道门本身已于 0.8.0 删除）；这条不变量在客户端语义上
   的现代表达是 `http` 永不跟随重定向——判定「请求去哪」和执行
   「请求去哪」必须是同一个答案。
3. **宽松旗标只豁免它点名的状态**：`--allow-unsigned` 豁免缺失，
   永不豁免腐坏。每个 escape hatch 都要回答「它洗白什么、不洗白
   什么」。
4. **契约必须在选择点强制**：能宣传什么就必须能兑现什么——桥的
   `definitions()` 里解析不了的 `parameters-json` 让加载当场失败
   （F5），因为静默放宽就是一张假账单。
5. **存储层自己校验键形状**：键拼文件名的存储（trust/keys），
   形状校验落在副作用发生的那一层，不信生产者的不变量。
6. **降级必须带恢复力**：单点降级（trap→continue/error）不等于
   容错，每个降级声称都要钉「之后还能正常干活」。

## 6. 明确不做什么

- **核心无 MCP**（§4.8）、核心无 wasm（tau-ext 才碰 wasmtime）、
  核心无网络。
- **不做 secret 保管**：宿主从不持久化任何秘密；内置 provider 的
  凭据直接读环境变量，不落 tau 的盘。
- **不发明沙箱**：能力边界只有 wasm 运行时 + world 划分——0.8.0
  撤掉 scoped 门之后没有第二层，被 spawn 的 MCP server 是用户自己
  选的风险，与原生 MCP 客户端相同。
- **内置工具在沙箱之外**：`read/write/edit/ls/grep/find/bash/
  powershell` 是宿主代码，wasm 侧从来管不到它们（0.8.0 起连
  `--deny-wasi` 这个开关也不存在了）；
  唯一的关断是启动时的 `--tools` / `--no-builtin-tools`
  （`docs/builtin-tools.md`）。**唯一例外是 ACP 模式**：编辑器在连接
  的另一端，四个 mutating 内置工具经 `session/request_permission` 先
  问宿主（`docs/acp.md`）——这是「另一端有人可问」才有的门，不是沙箱；
  模式外一切照旧、无门。
- **wasip3 流式 ABI**：当前边界是「字符串整体拷入 guest」，大 ABI
  改造是保留给未来的显式决策，不被动滑入。工具链现状已 spike
  钉死：guest 侧 stable 阻塞至 Rust 1.100（预计 2026-11），host
  侧就绪——解锁条件与迁移草案见 `docs/wasip3-streams.md`。

## 7. 文档地图

| 文档 | 内容 |
|------|------|
| `wit/tau.wit` | 扩展契约（先读这个） |
| `docs/extensions.md` | 扩展作者指南：scaffold → 两个 world → 签名 → OCI |
| `docs/builtin-tools.md` | 内置工具：八个原生工具、两个旗标、沙箱诚实性 |
| `docs/acp.md` | ACP 模式：编辑器接入、会话映射、事件表、权限门 |
| `docs/probes.md` | 九个探针点的载荷与 verdict 语义 |
| `docs/events.md` | 事件总线 / 探针 / 控制通道的三通道模型 |
| `docs/bridges.md` | 桥的能力模型与 MCP 参考实现 |
| `docs/signing.md` | 签名格式与信任存储（授权记忆 0.8.0 已删） |
| `docs/oci.md` | OCI 分发链路 |
| `docs/media.md` | 多模态与 blob 存储 |
| `docs/realtime-av.md` | 实时音视频：RealtimeSession（媒体面全在宿主内）；world realtime 与设备门类 0.8.0 已删，文内标注 |
| `docs/repl.md` | REPL 与 pi 的命令面对齐：对照表、口径、分层挂账 |
| `docs/wasip3-streams.md` | wasip3 stream 迁移的工具链现状与解锁条件 |
| `docs/release.md` / `docs/perf.md` | 发布流程 / 性能基线 |
