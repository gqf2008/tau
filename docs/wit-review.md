# WIT 契约评审：`tau:extension@0.1.0` 全部对外扩展面

> 评审日期 2026-09-27，评审人 Claude，范围：`wit/tau.wit`（canonical）
> + tau-ext vendored 副本 + 宿主实现证据 + 0.2.0 设计稿
> （host-channel/im-channels/realtime-av）与现行契约的一致性。
> 每条 finding 附证据与处置。用户裁定已并入：「mcp-bridge 包装是包装，
> 内部必须有完善的双向调用机制保障」（→ F2）。

## 总评

契约的基本盘是好的：string 信封约定统一、能力接口（process/http）
形状一致且 consent 门 fail-closed、vendored 副本与 canonical 同步
（diff 为空）、`StopReason` 五态与 WIT `done` 注释精确一致、
`tau probes` 发现机制真实可用。但评审发现**一个结构性安全张力、
一个双向机制缺口、一处文档-代码漂移**，外加若干一致性问题。

## Findings（按严重度）

### F1 [高·安全模型] Ambient WASI 默认放开网络+全文件系统，consent 门有侧门 —— 已裁定（A，2026-09-28）

证据：`crates/tau-ext/src/lib.rs:207-232`——`WasiPolicy::AllowAll`
（**默认**）对每个组件 `inherit_network().allow_ip_name_lookup(true)`
+ 全宿主文件系统 preopen + 全 env 继承。

后果：bridge/provider 组件的 `http` origin consent、`process` argv
consent 可以被 **wasi:sockets / wasi:fs 直接绕过**——被拒绝了 origin
的组件改用 ambient socket 照样出网。「能力边界：组件能做什么由它
import 什么 + 宿主授予什么决定」（architecture.md）在默认策略下不成立；
签名→信任→consent 链条对网络和文件系统是建议性的，只有用户跑
`--deny-wasi` 或逐指纹记 `wasi_deny` 时才闭合。这是文档明示的取舍
（CHANGELOG「Ambient WASI by default」），不是 bug，但它是契约评审
必须摆上台面的第一问题。

处置选项（0.3.0 决策项，需用户拍板）：
- A. 维持默认开（可用性优先），把「consent 可被 ambient 绕过」在
  `docs/extensions.md` 写明；
- B. **对 import 了 consent 能力的 world（bridge/provider）默认 deny
  ambient 网络+fs**，普通 extension world 保持默认开——能力门对其
  针对的组件闭合，usability 损失最小（推荐）；
- C. 全局翻转默认为 deny（最严，破坏性最大）。

**裁定（owner，2026-09-28）：A —— 维持默认放开。** 绕过路径与两个真正的
收紧开关写进了作者指南 `docs/extensions.md` §7「WASI: ambient by default
— and why the gates are not a wall」，那一节是此事的**唯一权威表述**（本
finding 不复述细节，避免两处漂移）；`docs/architecture.md` 的「能力边界」
一条与 §4.5 表下各补了一句诚实注解并指回该节。证据在代码层复核过：
`wasmtime_wasi::p2::add_to_linker_sync` 对四个组件种类一律挂全量 p2
（含 `sockets::tcp/udp/ip-name-lookup` 与 `filesystem`），配合默认
`inherit_network()` + 全盘 preopen，绕行是结构性的而非推测。B/C 保留为
后续可选项：若将来要把能力门从「意图声明」升级成「墙」，B（按 world
精准闭合）是首选路径，代价是 bridge/provider 既有的 ambient 用法要重新
征得同意。

### F2 [高·双向机制] extension world 只有半条双向通道 —— 已全部落地（0.2.0 + 0.3.0）

用户裁定：MCP 可以经 mcp-bridge 包装，但**内部**必须有完善的双向
调用机制。评审结论：「完善」要三条腿，目前只有一条半——

| 腿 | 状态 | 证据 |
|---|---|---|
| 注入（guest→host 发消息进会话） | ✅ 已落地（0.2.0） | `host.steer/follow-up`，consent 门（`docs/host-channel.md`） |
| 反馈（host→guest 告知调用结果） | ✅ 已落地（0.2.0） | `host.*` 与 `events.emit` 全部返回 `result<_, string>` |
| 观测·低频（生命周期点） | ✅ 已落地（0.2.0） | observe-only probes：session_start/branch/session_end |
| 观测·高频（流 delta） | ✅ 已落地（0.3.0） | `host.subscribe/poll/unsubscribe` 拉取订阅（`docs/stream-subscribe.md`，jev 裁决 pull_buffer） |

处置（均已落地）：① `host` 接口与 `events.emit` 统一返回
`result<_, string>`（0.2.0）；② 观测腿两条腿——observe-only probe
点（低频生命周期，0.2.0）+ `host` 拉取订阅（高频 delta，0.3.0：
`subscribe/poll/unsubscribe`，有界环 + lagged 标记，句柄随实例作用域，
events.md 规则 3 不变——高频路径永远不放 probe）。

### F3 [高·文档漂移] probes.md 承诺的 observe-only 点位未实现 —— 低频三点已落地

证据：`docs/probes.md` 列了 `session_start/session_end/branch/
text_delta/tool_progress`「observe-only in v0」，但
`crates/tau-core/src/probe.rs` 的 `ProbePoint` 只有 9 个已接线点，
tau-ext/tau-cli 对这些名字**零引用**；`docs/events.md` 的「wasm
extensions that observe (via tau-ext; observe-only)」同样无实现支撑。

处置（2026-09-27 落地）：session_start/session_end/branch 三点 wired
（observe-only：verdict 上报为 ignored 永不生效，CLI 发射，
`Agent::observe`）；text_delta/tool_progress 保持 reserved（高频拉取
订阅设计落地前不接线），`tau probes` 目录列出 wired/reserved 状态。

### F4 [中·全模态] 工具结果只能是文本 —— 已落地（tau:extension@0.3.0，docs/tool-media.md）

证据链：`wit tool-result{content: string}` ↔
`tau-core ToolOutput{content: String}`（tool.rs:21）↔
`Content::ToolResult{content: String}`（types.rs）。消息模型支持
image/audio/video/file 块，但**工具无法返回媒体**——截图工具、
文件生成工具做不了，与全模态故事（CHANGELOG）断一环。

评估结论（2026-09-27，查过 pi 线格式）：
- pi `ToolResultMessage.content = (TextContent|ImageContent)[]`——
  **多块，但仅 text+image**；image 在线上是内联 base64
  （`ImageContent{data: base64, mimeType}`），read 工具即如此返回。
- tau session 的媒体本来就走 blob 引用（`sha256:`），请求边物化——
  与 pi 的字节级兼容本就不存在，兼容面在格式族（JSONL 树），
  F4 不引入新的不兼容。
- 厂商边：Anthropic/OpenAI 的 tool_result 内容块只收 text/image，
  非 image 媒体须在 provider 边降级为文本占位符。

落地改动面（2026-09-27）：WIT `tool-result.content: string →
list<result-block>`（契约 breaking 至 0.3.0；**非递归 result-block
变体**替代原草样的 `list<content>`——wasmtime 宿主侧 bindgen 拒编
任何递归 WIT 类型，且工具结果本不该嵌套调用/结果，与 pi 的
text|image 块对齐）；`ToolOutput.content: String → Vec<Content>`；
convert.rs 映射 + 尺寸上限沿用；provider 边非 image 媒体降级；
旧 session 文件读兼容（string → [Text]）。设计与验收清单见
`docs/tool-media.md`（2026-09-27 定稿并落地，jev 裁决先行文档）。

### F5 [中·fail-loud] parameters-json 解析失败静默降级为全开放 schema —— 已落地

证据（修复前）：`.unwrap_or_else(|_| json!({"type":"object"}))`。
组件给了坏 schema，宿主静默当成「任意对象」，模型自由发挥参数。
与信任体系全线的 fail-closed 原则不一致（gc 坏路径、坏签名、坏
consent 文件全部 fail-closed，唯独这里 fail-open）。

处置（2026-09-27 落地，随 0.2.0 批次）：**拒载并点名工具**（jev
裁决：strict→lenient 是单向门，且与 provider --model 拒载先例一致）。
`tau-ext::tool_def_strict` 统一 extension 与 bridge 两个加载点；
负面夹具 `examples/bad-schema`（永不发布）+ 单元测试 +
validate.sh 断言三重验收。

### F6 [低·一致性] `events.emit` 无返回 vs 0.2.0 设计全 result —— 已落地（0.2.0）

0.2.0 把 `events.emit` 与 `host.*` 统一为 `result<_, string>`
（provider world breaking，0.x 语义允许）。并入 F2 反馈腿。

### F7 [低·命名] `hooks` 接口 vs probes 术语漂移 —— 已落地（0.2.0，接口名即 `probes`）

WIT 接口叫 `hooks`，代码/文档/CLI（`tau probes`）全叫 probes。
0.2.0 是正名窗口（`hooks` → `probes`），改名成本随采用度增长。

### F8 [低] `process.kill` 无返回；u64 句柄无代数 —— 已落地（0.2.0）

kill 失败静默；句柄 close 后复用理论上有 ABA 风险。0.2.0 一并补。

### F9 [中] ws 能力确认必要；http 能力缺超时/取消控制 —— 已落地（ws 0.3.0，http 0.4.0）

评审确认 im-channels.md 的 `ws` 能力设计必要（飞书/钉钉 stream
模式是 WebSocket 帧协议，现有 `http.read-body` 的增量读只覆盖
SSE/长轮询）。补充要求已进设计（2026-09-27，im-channels.md `ws`
节）：`ws` 宿主 ping 保活 + pong 超时判死 + `recv` idle 超时显式
error（永不永阻）；`http` 增量读带 idle 超时，连接级 keepalive 归
宿主 HTTP 栈。**实现已落地**：`ws` 随 0.3.0；`http.read-body` 的
idle 超时随 `tau:extension@0.4.0`（2026-09-28，owner 已裁定 A 的同一
轮）——签名加 `timeout-ms`（0 拒绝、超时显式 error，与 `ws.recv`
同形），宿主两条单测 + validate.sh 静默对端腿，见 docs/bridges.md。

### F10 [信息] 评审通过项（无需动作）

- vendored `crates/tau-ext/wit/tau.wit` 与 canonical 逐字节同步，
  `wit_vendored` 测试盯漂移；
- `StopReason` 五态与 WIT `done` 注释精确一致；`ModelEvent` 无
  usage 事件，WIT 侧不缺；
- consent 五门类（command/mcp_url/origins/auth_delivery/wasi_deny）
  覆盖现有能力面；未签名组件无指纹永不可 remembered（签名→consent
  链条闭环）；
- probe 点名 `from_name` 双向映射完整，目录测试强制每变体一条目；
- `http` redirect never followed 写进契约注释，validate.sh 有
  consent-escaping 302 实测；
- 签名/信任全链 fail-closed（51 条断言覆盖）。

### F11 [低·一致性] `process.read-stdout` 无 idle 超时 —— 已落地（0.5.0，同批补齐等响应头/等握手）

`http.read-body` 与 `ws.recv` 现在都有界，`process.read-stdout` 仍是
「Blocks until at least one byte is available or the stream closes」——
子进程挂死且不写 stdout 时，宿主线程同样会永久阻塞。F9 的设计文字只
点名了 http，本轮按范围纪律不顺手改它。若要把「永不永阻」做成全能力
不变量，下一批按同一形状补 `timeout-ms`（契约 0.5.0）。

**已落地（2026-09-28，`tau:extension@0.5.0`）**：owner 开 0.5.0 轮并
指定先做 F11，实施时按「等待对端的每一处都设界」把同类面一次收口，
共三处签名加 `timeout-ms`——`process.read-stdout(handle, max,
timeout-ms)`（等字节）、`http.request(…, timeout-ms)`（等响应头）、
`ws.connect(url, timeout-ms)`（等握手）。三处都是 0 拒绝（理由：
「block forever is not a contract」）+ 超时显式 error，句柄/子进程
在超时后仍可用。**宿主实现各有各的形状**：reqwest 的阻塞 `timeout`
是「直到响应体读完」的总超时，拿来界响应头会连带把长 SSE 截断，所以
改成辅助线程 + `mpsc::recv_timeout` 只界头部（体仍归 `read-body` 的
预算）；`tungstenite::connect` 把 TCP+TLS+upgrade 合成一次无界阻塞调用
且读滴答要握手后才设得上，同样只能线程 + 通道界住。验收：宿主七条
单测（三处 0 拒绝 + 静默对端等头 / 永不 upgrade 的监听 / 静默子进程，
外加一条「超时后迟到的字节仍读得到」）+ validate.sh 两条腿（`/mute`
路由等头超时、只 accept 不 upgrade 的监听握手超时）。

### F12 [低·一致性] `process.write-stdin` 无界，但它不能照抄 F11 的形状 —— 已登记（下一批）

F9/F11 之后，`write-stdin` 是唯一仍可能永久阻塞的宿主调用：子进程不读
自己的 stdin 时，管道缓冲区写满即阻塞。**但它与那一类是不同型的问题**：
`read-*`/`connect` 超时可以「放弃等待 + 显式 error」，客座重试仍然安全
（没读到就没有副作用；`connect` 只是重开一条连接）；而一次 write 超时后
可能**已经送进去一部分**，
此时返回 error 会让客座重试整段数据，等于静默重放半条 JSON-RPC 消息
——比挂起更糟。要正解就得先给契约加「部分写入」语义（`write-stdin`
返回已写字节数，客座自持偏移续写），那是接口形状变更，不是加一个
参数。故本轮如实登记为下一批候选，不在 0.5.0 顺手改。

（`http.request` 不属于这一型：超时后「发出去了但不知道结果」是任何 HTTP
客户端的固有性质，标准解法是幂等键或调用方判断，界本身仍是净收益——它不像
write 那样把「重试安全」直接破坏掉，只是把「无限等」换成「确定的未知」。）

## 0.2.0 契约动作清单（从 findings 汇总）

1. F2/F6：`host` 接口 + `events.emit` 全部返回 `result`（0.2.0）；
   观测腿两腿全落地——observe-probe 点（0.2.0）+ 高频拉取订阅
   `host.subscribe/poll/unsubscribe`（0.3.0，docs/stream-subscribe.md）。
2. F3：probes.md/events.md 的未实现段落已标注；实现并入观测腿。
3. ~~F5~~：parameters-json 坏 schema 拒载并点名工具（已落地）。
4. ~~F7/F8~~：hooks→probes 正名、kill 返回 result、句柄代数——已落地
   （0.2.0；句柄高位代数在 bridge.rs 的进程/HTTP 句柄上统一校验）。
5. ~~F1~~：ambient 侧门处置——owner 2026-09-28 裁定 A（见上文裁定段）。
6. ~~F4~~：工具媒体结果——已落地（0.3.0：`tool-result.content` 收
   `list<result-block>`，非递归变体绕开 wasmtime bindgen 的递归类型
   拒编；provider 边非 image 媒体降级为占位符；旧 session 读兼容）。
7. F9：ws 随 0.3.0、http idle 超时随 0.4.0 落地。F11 随 0.5.0 落地
   （read-stdout + request 等头 + connect 等握手三处一次收口）；
   新登记的 F12（write-stdin 无界）留待下一批——它需要部分写入语义，
   不是加参数。

## 修订记录

- 2026-09-28（0.5.0 批次**已落地**，本仓 main）：F11 收口为「等待对端的
  每一处都设界」——`process.read-stdout` / `http.request` / `ws.connect`
  三处签名加 `timeout-ms`（0 拒绝、超时显式 error、句柄与子进程在超时后
  仍可用）；实现细节见 F11 段（等头与等握手只能靠辅助线程 + 通道，因为
  reqwest 的阻塞 `timeout` 覆盖到响应体、tungstenite 的 connect 把
  TCP+TLS+upgrade 合成一次调用）。同批把契约版本收成单一来源：
  `tau-ext` 的 `CONTRACT_VERSION` 供 load 错误提示使用，并有测试断言它
  等于 `wit/tau.wit` 的 `package` 行。新登记 F12（`write-stdin` 无界，
  需部分写入语义，不是加参数）留待下一批。

- 2026-09-27（评审后讨论）：**信封约定被推翻**。原约定「消息载荷一律
  JSON 信封」经质询后确认论证有误——pi 兼容约束的是 session 文件与
  provider HTTP 两个 JSON 边，不约束组件 ABI；`tau_core::types` 本来
  就要求二进制边界不见 base64，信封+base64 恰恰违反它。新约定（已入
  `docs/extensions.md` 约定节）：**tau 拥有 schema 的载荷用 WIT 类型，
  JSON 只留在外生/任意 schema 的叶**（arguments-json、parameters-json、
  probe payload-json）。简约校准参照 pi：简约在机制（hook/tool 皆函数、
  载荷皆plain data），不在数据结构——类型集保持最小，content 四态
  （text/media/tool-call/tool-result），image/audio/video 塌缩为
  media（MIME 主类型即语义）。F2 的 host-channel 设计已改类型化 v2；
  `events.emit` 的类型化（消灭 audio-delta base64 热路径）与 F6 的
  result 化同属 0.2.0 breaking 批次。

- 2026-09-27（0.2.0 批次**已落地**，本仓 main）：F2 反馈腿（`events.emit`
  与 `host.*` 全部 `result<_, string>`，malformed-frame 静默 skip 路径
  随信封一并删除）、F6、F7（`hooks`→`probes` 正名）、F8（kill 返回
  result + bridge 句柄代数：重建后旧句柄报错而非别名到新子进程）。
  host 通道（F2 注入腿）落地：notify/emit 进事件总线
  （`AgentEvent::ExtensionNotice`/`ExtensionFact`），steer/follow-up
  过 consent 新门类 `inject`（--allow-inject / --remember），
  enqueue-only 走控制通道既有 checkpoint。load 错误点名契约版本错配。
  F5 同批落地：坏 parameters-json 拒载并点名工具。
  F3/F2 观测腿（低频）同批落地：session_start/session_end/branch
  observe-only probe 点（CLI 发射，`Agent::observe`，误用 verdict 上报
  ignored）；text_delta/tool_progress 留 reserved，等高频拉取订阅设计。
  F2 观测腿的高频拉取订阅仍是设计项。models.run 的 request-json
  **保持 JSON**（评审确认）：provider 的职责是翻译到厂商 JSON 线格式，
  本路径无热路径，类型化收益为零；若未来反序列化成本显现再评估。
