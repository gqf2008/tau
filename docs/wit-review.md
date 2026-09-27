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

### F1 [高·安全模型] Ambient WASI 默认放开网络+全文件系统，consent 门有侧门

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

### F2 [高·双向机制] extension world 只有半条双向通道

用户裁定：MCP 可以经 mcp-bridge 包装，但**内部**必须有完善的双向
调用机制。评审结论：「完善」要三条腿，目前只有一条半——

| 腿 | 状态 | 证据 |
|---|---|---|
| 注入（guest→host 发消息进会话） | ✅ 已设计未落地 | `docs/host-channel.md`（steer/follow-up） |
| 反馈（host→guest 告知调用结果） | ❌ 缺 | `events.emit` 无返回（`lib.rs:573`）；malformed frame 静默 skip（`lib.rs:806` `Err(_) => continue`），guest 的事件 schema 错了只会流进虚空，无任何信号 |
| 观测（guest 订阅宿主事件） | ❌ 缺 | extension world 无事件 import；observe-only probe 点未实现（F3） |

处置：host-channel.md 修订补两条腿——① `host` 接口与 `events.emit`
统一返回 `result<_, string>`（0.2.0 一并改，0.x 允许 breaking）；
② 观测腿设计二选一：observe-only probe 点（低频生命周期事件，复用
probe 语义）+ host import 拉取订阅（高频 delta，events.md 规则 3
禁止 probe 上高频路径，所以 probe 形态覆盖不了 text/audio delta，
**两条腿都要**）。

### F3 [高·文档漂移] probes.md 承诺的 observe-only 点位未实现

证据：`docs/probes.md` 列了 `session_start/session_end/branch/
text_delta/tool_progress`「observe-only in v0」，但
`crates/tau-core/src/probe.rs` 的 `ProbePoint` 只有 9 个已接线点，
tau-ext/tau-cli 对这些名字**零引用**；`docs/events.md` 的「wasm
extensions that observe (via tau-ext; observe-only)」同样无实现支撑。

处置（docs-first 规矩）：本次评审已把两份文档的相关段落标注
「未实现」；实现并入 F2 观测腿。

### F4 [中·全模态] 工具结果只能是文本

证据链：`wit tool-result{content: string}` ↔
`tau-core ToolOutput{content: String}`（tool.rs:21）↔
`Content::ToolResult{content: String}`（types.rs）。消息模型支持
image/audio/video/file 块，但**工具无法返回媒体**——截图工具、
文件生成工具做不了，与全模态故事（CHANGELOG）断一环。

处置：0.3.0 评估项。改动面到 session 线格式（与 pi 的兼容性），
动手前先查 pi 的 ToolResult 线格式是否支持多块内容。

### F5 [中·fail-loud] parameters-json 解析失败静默降级为全开放 schema

证据：`lib.rs:426`——`.unwrap_or_else(|_| json!({"type":"object"}))`。
组件给了坏 schema，宿主静默当成「任意对象」，模型自由发挥参数。
与信任体系全线的 fail-closed 原则不一致（gc 坏路径、坏签名、坏
consent 文件全部 fail-closed，唯独这里 fail-open）。

处置：load 期警告（渲染层可见）或直接拒载，0.2.x 即可改，不等契约。

### F6 [低·一致性] `events.emit` 无返回 vs 0.2.0 设计全 result

0.2.0 把 `events.emit` 与 `host.*` 统一为 `result<_, string>`
（provider world breaking，0.x 语义允许）。并入 F2 反馈腿。

### F7 [低·命名] `hooks` 接口 vs probes 术语漂移

WIT 接口叫 `hooks`，代码/文档/CLI（`tau probes`）全叫 probes。
0.2.0 是正名窗口（`hooks` → `probes`），改名成本随采用度增长。

### F8 [低] `process.kill` 无返回；u64 句柄无代数

kill 失败静默；句柄 close 后复用理论上有 ABA 风险。0.2.0 一并补。

### F9 [中] ws 能力确认必要；http 能力缺超时/取消控制

评审确认 im-channels.md 的 `ws` 能力设计必要（飞书/钉钉 stream
模式是 WebSocket 帧协议，现有 `http.read-body` 的增量读只覆盖
SSE/长轮询）。补充要求进设计：`ws` 与 `http` 都应明确 keepalive /
idle 超时的处置语义（bridge 长连是断线敏感场景，参照飞书长连接
断线窗口 lesson）。

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

## 0.2.0 契约动作清单（从 findings 汇总）

1. F2/F6：`host` 接口 + `events.emit` 全部返回 `result`；host-channel.md
   补观测腿设计（observe-probe 点 + 高频拉取订阅）。
2. F3：probes.md/events.md 的未实现段落已标注；实现并入观测腿。
3. F5：parameters-json 坏 schema fail-loud（可先行，不等契约）。
4. F7/F8：hooks→probes 正名、kill 返回 result、句柄代数，随 0.2.0。
5. F1：ambient 侧门处置（A/B/C）留用户拍板，0.3.0 决策项。
6. F4：工具媒体结果，0.3.0 评估项（先查 pi 线格式）。

## 修订记录

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
