# WIT 接口重设计：0.6.0 → 0.7.0

**状态：提案（owner 审阅用）。** 本文件与 `wit/next/tau.wit`（草案）先行；契约本体
`wit/tau.wit` 与宿主代码**未动**，22 个 examples 目录（`validate.sh` 构建 15 个 Rust 组件，另 6 个语言见 `docs/wasm-languages.md`）无需重建。草案已通过 `wasm-tools`
解析、访客侧（wit-bindgen 0.62 / stable `wasm32-wasip2`）与宿主侧（wasmtime 49.0.1）
的绑定编译，证据见 §5。落地分期见 §6。

参考：Component Model 异步设计
<https://component-model.bytecodealliance.org/design/async.html>、WIT 类型与标识符
<https://component-model.bytecodealliance.org/design/wit.html>，以及 WASI 0.3 自身的
用法（`wasmtime-wasi 49.0.1` 的 `src/p3/wit/`：`read-via-stream` /
`write-via-stream` / `tcp-socket.send/receive` / `udp-socket.send/receive`）。

## 1. 一句话

**形状重设计：把「同步 ABI 的歉意」从契约里删掉，并把契约变成 `tau_core` 的投影。**命名原则见 §3 的投影规则（owner 指示 2026-09-29）：同名同形，投影不了的写明理由。 功能集不变（除因形状必然消失的
东西），每个界面的语义不变；变的是怎么表达「等」、怎么表达「持有」、以及哪些载荷还
需要是 JSON。

| 0.6.0 的机制 | 0.7.0 草案 |
|---|---|
| `u64` 句柄 + 宿主代际表（stale / unknown handle 错误族） | `resource`：所有权即生命周期，丢弃即关闭 |
| 有界轮询：`read-stdout(max, timeout-ms)`、`read-body(max, timeout-ms)`、`ws.recv(timeout-ms)`、`write-stdin(..) -> taken` | `stream<T>` + `future<result<_, error>>`：背压来自流容量，取消来自丢弃流，截止时间归调用方 |
| 推送通道 `events.emit`（宿主拒绝逐事件回传） | `models.run` 返回 `stream<event>`：宿主按自己的节奏拉，`interface events` 消失 |
| `point: string` + `payload-json`、`request-json`、`config-json`、`level: string`、`topics: list<string>` | tau 拥有 schema 的载荷全部类型化（枚举/变体/记录） |
| 24 处 `result<_, string>` 等无类型错误 | `types.error`：`refused` / `failed` / `invalid` + 细节字符串 |

## 2. 为什么（0.6.0 自己写下的代价）

0.6.0 的契约文件 623 行里，有很大一部分文字不是在设计，而是在**向读者解释同步 ABI
的后果**：

- `timeout-ms` 出现 **13 次**，出现在 6 个函数的签名或文档里；
- 「0 is rejected, because "block forever" is not a contract」这类句子出现 **4 次**
  （`process.read-stdout`、`http.request`、`http.read-body`、`ws.recv`）——每一条都
  是同一个句子的重写；
- 「Handles carry a host-side generation: a stale handle after close errors instead of
  aliasing a newer child」出现 **3 次**（`process.kill`、`ws.close`、`host.unsubscribe`）；
- `write-stdin` 的「返回**取走**的字节数，而不是送达的字节数」段落（wit-review F12）
  存在，只是为了绕开「阻塞写超时后说不清送出去多少」；
- 「the wait is bounded by `timeout-ms`: … used to park the host thread forever
  (wit-review F11)」出现 3 次——**"park a host thread" 本身就是同步调用的定义**；
- 24 处 `result<_, string>`：`refused`（没同意）与 `failed`（同意了但坏了）在契约上
  是同一个东西，组件想分别处理只能匹配英文；
- 6 种 `*-json` 名字（共 12 处）里，`payload-json`、`request-json`、`config-json`
  承载的都是 **tau 自己的 schema**（工具参数 `arguments-json`、JSON Schema
  `parameters-json`、扩展自定义事实 `event-json` 才是真叶子）——0.6.0 的文件开头刚说
  完「schema 是 tau 的就用 WIT 类型」，紧接着为前三种各写了一段例外说明。

这些都不是错误，是一个同步 ABI 能给出的最好答案。Component Model 的异步 ABI 让这
些答案不再是唯一答案。

## 3. 原则（用新机器，但只在它更真的时候用）

1. **持有交给所有权。** 句柄是「借来的数字」，`resource` 是「持有的东西」：不知道
   谁在什么时候关闭、关闭两次会怎样、关闭后重建（trap）会不会撞上别人，这些问题在
   所有权下不成立。
2. **等待交给流与 future，截止时间还给调用方。** 同步调用无法说「还没有」，只能
   用「最多等 N 毫秒」近似；`stream` 说「还没有」的方式就是等。宿主线程不再被park，
   于是「不许永远阻塞」不再是契约条款，而是传输层默认。
3. **方向由谁生产决定。** 组件生产、宿主消费 ⇒ 返回 `stream`（`models.run`、
   `session.downlink`）；宿主生产、组件消费 ⇒ 参数 `stream`（`session.uplink`、
   `child.stdin`）；**宿主不能等的时候不硬上流**（总线 ≠ 可等待的上下文，见 §4
   `host.subscribe`）。
4. **tau 拥有 schema 就类型化，JSON 只留四个叶子。** 保留 JSON 的只有：工具参数
   （模型产生的任意 JSON）、JSON Schema（本身是 schema 语言）、扩展自定义事实
   （`host.emit`，schema 在外部）、压缩 reason 字符串（镜像 `tau_core` 的条目）。
   0.6.0 的例外有 6 处，草案把它们收回到 0。
5. **不用新机器顺手加能力。** 见 §7「不做的事」：流式媒体源、流式请求体、
   `process` 的 env/cwd、探针异步化，都不在本次。

### 投影规则：WIT 是 `tau_core` 的投影（owner 指示，2026-09-29）

**规则**：契约里每个类型都必须是宿主核心数据结构（`crates/tau-core`）的投影，不是另一套
词汇。机械映射：

| Rust | WIT |
|---|---|
| `struct` | `record`（字段 snake_case → kebab-case） |
| `enum`（无载荷 / 带载荷） | `enum` / `variant`（臂名 kebab-case） |
| `Option<T>` / `Vec<T>` / `Vec<u8>` | `option<T>` / `list<T>` / `list<u8>`（字节裸过 ABI，base64 只在 JSON 边缘） |
| 嵌套的具体类型 | 独立命名的 `record`/`variant`（跨界面 `use`） |

**本轮据此改掉的地方**（与 0.7.0 草案的前一版相比）：

| 位置 | 改法 |
|---|---|
| `variant content` | **4 臂 → 7 臂**，逐臂对齐 `tau_core::types::Content`：`text / image / audio / video / file / tool-call / tool-result`。「MIME 大类型推断媒介」取消——媒介由 guest 明说，`convert.rs` 里那条「给 image 命名是校验错误」的规则随之消失 |
| `record media` | 去掉 `name`（Rust 的 `Media` 只有 `media-type` + `source`）；文件名回到它在 Rust 里的位置：`record file { media, name: option<string> }` |
| `variant result-block` | 五臂 `text/image/audio/video/file`，与 `content` 的非工具臂**逐一对应**；宿主转换器穷尽匹配 ⇒ 一边加臂另一边漏掉是编译错误，不是静默漂移 |
| `record tool-result`、`tools.tool-result` | `content: list<result-block>`，对齐 `Content::ToolResult`（收窄见下）与 `ToolOutput` |
| `tools.definition` | 对齐 `ToolDef`（`parameters-json` 是 JSON Schema——schema 本身在外部） |
| `models.run` 的 `request` | = `tau_core::model::Request` 逐字段 + `model` / `auth` 两个宿主侧字段（provider 要知道选哪个模型、用哪把钥匙；这两样不在 tau 的对话模型里） |
| `stop-reason`、`model-event` | 0.6.0 起就逐臂对齐 `StopReason` / `ModelEvent`，本轮未动 |

**投影不了的两处：递归**（实测，不是取舍）。WIT 不允许类型自我依赖，`wasm-tools 1.259.0`
的两条最小复现都在 `target/wit-probe/`：

```
error: type `content` depends on itself    ← record tool-result { content: list<content>, … }
error: type `json` depends on itself       ← variant json { array(list<json>), … }
```

后果两条，写进契约注释而不是绕开：

- `Content::ToolResult { content: Vec<Content> }` 比 ABI **宽**：`result-block` 是**被迫的
  拆分**（不是因为风格）；语义上不丢——工具结果里不会再出现工具调用或嵌套结果。
- `serde_json::Value` **不可投影**：凡 schema 不在 tau 手里的载荷只能是 **JSON 文本**
  （`arguments-json`、`parameters-json`、`emit(event-json)`）。0.6.0 的理由（厂商 wire 本就是
  JSON，provider 无论如何要重新序列化）继续成立，这里多了一条工具链的硬理由。

**故意的收窄三处**（理由都在契约注释里，不是漏掉）：

1. `host.stream-event` 3 臂 vs `AgentEvent` 15 臂：订阅环只承载**高频段**
   （text-delta / audio-delta），且只见计数不见字节——`docs/stream-subscribe.md` 的形态裁决；
2. `models.info` 是 provider 自述的目录（宿主只转达），`tau_core` 不需要对应类型；
3. 事件总线上跑 `AgentEvent`（运行叙事），契约里是 `ModelEvent`（模型输出）——两者本就
   不是同一个东西，映射在 `tau-ext`。

**因此 Rust 侧长出的两类**（迁移期 1 前半，2026-09-29 已落地）：`types.error` 对应
`tau_core::error::HostError`（`Refused` / `Failed` / `Invalid`，核 3 臂替换宿主内部的
`String`）；探针的 `payload` / `verdict.replace` 对应 `tau_core::probe_payload::ProbePayload`
（每点一臂，记录逐字段对齐契约里的同名 record）——0.6.0 的 `ProbeHandler::probe(point,
payload: Json)` 与 `payload["messages"]` 式取字段随之消失。**组件侧 ABI 未动**：访客仍收发
`payload-json` / `replace-json`，两个 `to_json` / `merge_json` 就是那条边（形状与 0.6.0 逐字
节一致，由 `the_json_edge_round_trips_every_point` 钉住）。

## 4. 逐界面 before → after

`wit/next/tau.wit` 的每个界面首注释都写了该界面的 delta；这里是总览。

| 界面 | 0.6.0 | 0.7.0 草案 | 换的理由 / 代价 |
|---|---|---|---|
| `types` | `role`/`media`/`media-source`/`tool-call`/`result-block`/`tool-result`/`content`/`message` | 同前 + `stop-reason`（从 `events` 移来）+ `error {refused, failed, invalid}` | 探针载荷需要 stop-reason；错误分类是组件真正会分支的东西 |
| `tools` | `execute: func(..) -> tool-result` | `execute: async func(..) -> tool-result` | 工具要等宿主能力（http/process/ws）时原地 await，不再借宿主线程；结果仍是整块（工具进度流是另一个功能，没有夹带） |
| `probes` | `point: string` + `payload-json` + `action`/可选字段的 `verdict` | `enum point`（12 点）+ `variant payload`（每点一臂，类型化）+ `variant verdict {continue, replace(payload), block(string)}` | 点集与载荷形状都是 tau 的（docs/probes.md）；裁剪上下文的组件不必再解析消息 JSON。**仍是同步**：探针是决策点，不是 I/O 机会；`replace` 的臂必须与调用臂一致，宿主校验（否则 `invalid`） |
| `events` | `model-event` + `emit(event) -> result<_, string>` | **删除** | 推送通道整体被 `models.run` 的返回流取代 |
| `models` | `run: func(request-json)` + 组件 push | `run: async func(request: request) -> tuple<stream<event>, future<result<_, error>>>` | 背压（宿主慢 ⇒ 组件写 await ⇒ 从源头慢下来）、取消（宿主丢流 ⇒ 组件下一次写拿到未写出的余量 ⇒ 停止向厂商 API 拉取）、少一个界面。事件里的 `done`/`error` 仍是组件自己的叙事；future 报告**宿主对这条流**的裁决 |
| `host` | `notify(level: string, ..)`、`emit(event-json)`、`subscribe(topics: list<string>) -> u64` + `poll` + `unsubscribe` | `enum level`、`enum topic`、`emit(event-json)` 保留、`subscribe -> resource subscription { poll }` | 句柄族收进所有权；`lagged` **保留**（见下） |
| `host.subscribe`（单列） | 轮询句柄 | resource + `poll`，**不是** `stream<stream-event>` | 总线发布者是 agent 循环：若订阅是流，最慢的组件会把整个运行拖住——这正是「有界环 + lagged」要防的事。`lagged` 不是轮询的产物，是「组件没在读」的诚实答案；换成流也躲不开，还要多一次宿主能等的前提。这是本草案**拒绝**用流的一处，理由写在契约注释里 |
| `session`（realtime） | 界面级 `open(config-json)` / `push-audio` / `push-image` / `interrupt` / `close`，一实例一会话 | `resource session { create (static async), uplink-audio: async func(audio: stream<u8>) -> future, uplink-image: async func(jpeg: stream<list<u8>>) -> future, downlink() -> tuple<stream<event>, future>, interrupt() }` | 两个方向都成了流：上行是参数流（宿主写、组件读，丢掉写端＝上行结束），下行是返回流（组件写、宿主按播放节奏读）。「一实例一会话」原本不是关于会话的决定，是「界面没有实例状态」的结果——资源有 |
| `process` | `spawn(argv) -> u64` + `write-stdin(handle, data, timeout-ms) -> taken` + `read-stdout(handle, max, timeout-ms) -> (bytes, eof)` + `kill(handle)` | `resource child { spawn (static), stdin: async func(stream<u8>) -> future, stdout() -> stream<u8>, stderr() -> stream<u8>, wait() -> future<exit-status>, kill() }` | 短写记账、`timeout-ms`、句柄代际同时消失；EOF 与「子进程死了」不再共用一个 `eof` 布尔——退出状态问 `wait()` |
| `http` | `request(.., timeout-ms) -> u64` + `status/header/read-body(handle, max, timeout-ms)/close` | `request: async func(..) -> result<response, error>` + `resource response { status, header, body() -> stream<u8> }` | 连接与响应头是这次调用的等待；响应体是流（SSE 增量消费、提前停止＝放弃剩余并关连接，取消不需要单独的方法） |
| `ws` | `connect(url, timeout-ms) -> u64` + `send(handle, frame)` + `recv(handle, timeout-ms)` + `close(handle)` | `resource connection { connect (static async), send: async func(frame), receive() -> tuple<stream<frame>, future<result<_, error>>> }` | 「Ok 意味着已写出」（钉钉 ack 教训）由 `await` 精确表达；30s ping / 60s 空闲关闭策略不变，但以「流结束 + future 里的原因」呈现 |
| `ingress` | `listen(route) -> result` + `close(route)` | `listen(route) -> result<registration, error>`，`resource registration`（无方法） | 「关闭一个从未注册的路由会报错」这种不得不定义的错误，在所有权下不可表达 |
| `ingress-handler` | `handle-request(request) -> response`（同步、实例锁下） | `handle-request: async func` | 处理器要外呼（http/process）时原地 await，不再占着宿主线程等平台超时；**宿主仍按实例串行**（组件状态不被并发进入），这条不变 |
| worlds | `extension` / `provider{events,http}` / `realtime{events,http}` / `bridge` | `provider` / `realtime` 不再 import `events` | 少一个界面 |

## 5. 证据（哪些腿真的跑过）

| 腿 | 状态 | 命令 / 位置 |
|---|---|---|
| 契约解析（语法与名字空间） | ✅ `wasm-tools 1.259.0`，`EXIT=0` | `wasm-tools component wit wit/next/tau.wit`（931 行解析输出） |
| 投影后的 `content`（7 臂）+ `result-block`（5 臂）：访客侧 | ✅ stable `wasm32-wasip2` | `target/wit-probe/guest-bridge/src/lib.rs` 构造全部 7 臂、穷尽匹配 `result-block` 五臂；产物回读 `wasm-tools component wit probe_bridge.wasm` 与草案逐臂一致 |
| 同一形状：宿主侧 | ✅ wasmtime 49.0.1 `bindgen!`（`world bridge`） | `target/wit-probe/host-bind/src/main.rs` 同样构造 + 穷尽匹配，`EXIT=0` |
| 递归不可投影（两条最小复现） | ✅ 工具链拒绝 | `wasm-tools 1.259.0`：`error: type `content` depends on itself`（`target/wit-probe/recprobe.wit`）、`error: type `json` depends on itself`（`jsonprobe.wit`） |
| 访客侧绑定：资源导出 + `static async` 工厂 + `tuple<stream,future>` 返回 + 上行参数流 | ✅ stable `wasm32-wasip2` 编译通过 | `target/wit-probe/guest-realtime/`（`world realtime`） |
| 访客侧绑定：资源导入（child/response/connection）+ 流读取 + 类型化 probe + async ingress | ✅ stable `wasm32-wasip2` 编译通过 | `target/wit-probe/guest-bridge/`（`world bridge`） |
| 宿主侧绑定：wasmtime 49.0.1 四个 world 全部 `bindgen!`（`provider`/`realtime`/`bridge`/`extension`） | ✅ 编译并运行 | `target/wit-probe/host-bind/`；async 按界面开：`imports: { default: async }` + `exports: { default: async }`（wasmtime 的 bindgen **没有** `async: true` 这个键，接受的是 `debug/path/inline/world/ownership/trappable_error_type/interfaces/with/named_imports/additional_derives/stringify/skip_mut_forwarding_impls/require_store_data_send/wasmtime_crate/anyhow/include_generated_code_from_file/include_component_type/imports/exports`；async 是每个函数/每个界面的标记）。探针同时点名了宿主必须用到的生成类型并编译通过——`exports::…::session::Session`（访客导出的资源句柄）、`…::process::Child` / `…::http::Response` / `…::ws::Connection` / `…::ingress::Registration`（宿主实现的导入资源）、`host::Subscription` |
| 访客 async 导出 + 宿主读返回流（端到端运行） | ✅ 既有 spike（2026-09-29 复跑：修好下面那条消费者缓冲陷阱后原样通过，16384 字节模式一致） | `target/wasip3-spike/`（4×4096 字节逐字节校验） |
| 编译产物里真的是异步提升 | ✅ | 对 `probe_realtime.wasm` 反汇编：`(canon lift (core func "[async-lift]tau:extension/models@0.7.0#run") … async (callback …))`、`[static]session.create` 同；`[method]session.downlink` 是普通 lift。`wasm-tools component wit` 读回的组件类型与草案逐字一致 |
| 迁移期 1 前半：`HostError` + `ProbePayload`（0.6.0 ABI 不动） | ✅ 宿主侧全量迁移，`cargo test --workspace` 绿（`probe_payload` 6 条：每点 JSON 往返、空替换不改动、坏字段为 `invalid`、`before_tool` 整体替换、`after_tool` 的旧字符串升级；tau-ext `replace_tests` 3 条：合法替换、非 JSON/坏字段/无载荷一律降级 `continue`、未知字段忽略） | `crates/tau-core/src/{error.rs,probe_payload.rs}`、`crates/tau-ext/src/{lib.rs,bridge.rs}`、`crates/tau-cli/src/acp/permission.rs` |
| 迁移期 1 后半：tau-ext 的 async 宿主改造（0.6.0 ABI 不动） | ✅ **四个 world 全部切完**：`exports: { default: async }` 四者皆上，`imports: { default: async }` 另加在 provider / realtime / bridge 三个真正做 I/O 的 world；WASI 一律 `add_to_linker_async`；阻塞注册表挪进 `Arc<std::sync::Mutex<…>>`，整调用交 `spawn_blocking` 再 await（锁只在阻塞体内持有，绝不跨 await）；`off_runtime` 删除，同步入口统一是 `block_on_component`。门禁：`cargo test --workspace` 25 套全绿、clippy `-D warnings` 干净、`validate.sh` 86 条 ok / ALL ELEVEN STEPS PASSED | `crates/tau-ext/src/{lib.rs,realtime.rs,bridge.rs,ingress.rs}` |
| **宿主向访客流写入**（`session.uplink`、`child.stdin` 方向） | ✅ **过关**（leg 1）：宿主自产流交给访客，访客在自己的 async 导出里 `next().await` 读完 —— 16384 字节、校验和 24576 | `target/async-spike/`（访客 stable `wasm32-wasip2`，宿主 wasmtime 49.0.1）；`SPIKE_CHUNKS` / `SPIKE_SIZE` 可调。**不需要**回退到 `push-audio(data) -> result` |
| 宿主持有访客资源句柄的运行时行为 | ✅ **过关**（leg 2）：一个 `run_concurrent` 块里 `create` 出的句柄，在**另一个**块里调 async `uplink`，在任何块**外**调同步 `interrupt` / `facts`（回 `("a", 2)`）—— 实例状态与句柄身份都跨块成立 | 同上（leg 2） |
| `Store::run_concurrent` 下多会话并发的宿主模型 | ✅ **过关**（leg 3a/3b/3c）：一个 store 两个会话；B 跑到底时 A 的读被有意卡住（A 只被兜过一次数据 ⇒ 背压真的落在写端）→ 释放后 A 续上，两条流各 16384 字节逐字节一致；`a=("a",1)` / `b=("b",1)` 状态互不串 | 同上（leg 3）；0.6.0 式同步方法**不能**在块内调用（见下条硬约束） |
| 访客产流：组件写、宿主按自己的节奏读（`models.run`、`session.downlink` 方向） | ✅ **过关**（leg 3s）：单条无门下行 16384 字节逐字节一致 | 同上（leg 3s） |
| **同步导出里 `spawn` 的后台任务不会被调度** —— 草案上行签名必须改一个词 | ❌ 形状不成立 → 修法已实测 | leg 1b：`func(bytes: stream<u8>) -> future<…>`（草案里 `uplink-audio` / `uplink-image` / `process.child.stdin` 逐字如此）里 `spawn_local` 的 drain 任务**从未被 poll**（访客 `[guest-1b] spawned drain task polled` 不打印），future 永挂（25s 超时）；leg 1c：同一个实例上只要另有一个 async 调用跑到底，同一份任务就被调度、future 兑现（16384/24576）；leg 1d：把签名改成 **`async func(bytes: stream<u8>) -> future<…>`**，任务照常跑完（16384/24576）⇒ **形状不用换，`func` 改 `async func` 即可** | root cause 在 wit-bindgen 0.62 的访客执行器：`spawn_local` 只把 future 推进全局 `SPAWNED`，而 drain 它的只有异步回调里的 `Tasks::poll_next`（`src/rt/async_support/spawn.rs:15`、`:29`）；同步降低的导出没有回调/任务，所以没人 drain |

| 宿主实现方**返回**流/future（`child.stdout/stderr`、`http.response.body`、`ws.receive`、`child.wait`） | ✅ **过关**（leg 4，两种形状各一遍）：同步导入的两个函数返回宿主产出的流（`StreamReader::new(store, Paced)`）与宿主兑现的 future（`FutureReader::new(store, ready(..))`），资源方法与 freestanding 函数都成立，访客侧 `next().await` / `.await` 拿到 16384 字节 + 校验和 24576 + future 值 7 | 同上（leg 4）。**成立的前提是三条绑定配置**：① `imports: { "spike:runtime/pipes": store, "…[method]tap.stdout": store, "…[method]tap.wait": store }` —— 同步 WIT 导入默认只给 `&mut self`，而创建流/future 要 store，`store` 标志把生成物从 `Host` 换成 `HostWithStore<U>`，方法首参变 `Access<U, Self>`（WASI 0.3 的同步 `cli.stdin.read-via-stream: func() -> tuple<stream<u8>, future<…>>` 正是这么写的）；② `with: { "spike:runtime/pipes.tap": HostTap }` 指定资源存储类型——不给的话生成的是**空枚举** `pub enum Tap {}`，宿主根本存不进 `ResourceTable`；③ 宿主实现落在 `HasSelf<Ctx>` 上（`Access::get()` 因此回 `&mut Ctx`），并补 `impl HostTap/Host for Ctx {}` 两个 marker 以满足 `for<'a> D::Data<'a>: Host` |

### 本轮 spike 实测的四条硬约束（喂给迁移期 1 后半）

spike 的落点不是「能不能跑」，而是宿主改造前必须知道的事。四条都是实测，不是推断：

1. **消费者自己决定每轮能接多少字节**：`Source::read` 最多搬运 `buffer.remaining_capacity()`
   项，而 `Vec<T>` 的 `remaining_capacity()` 是 `capacity - len` —— 于是 `Vec::new()`（容量 0）
   **一项都读不到**：不报错，只是回 0。本轮两条假阴性（旧 spike 疑似失效、leg 3 疑似死锁）
   都出在这里。宿主消费者必须自备容量（`buf.reserve(64 * 1024)`），否则症状是「消费者被反复
   以空 source 轮询」或「写端永远等容量」，极易误诊成工具链缺陷。空 source 时按 trait 契约
   存 waker 再回 `Pending`。
2. **同步 WIT 函数生成的宿主 API 要独占 store**：`call_x<S: AsContextMut>(&mut store, …)`，
   走 `call_async`；它**不能**在 `run_concurrent` 闭包里调用（`acc.with(…)` 那条逃逸路借不过
   borrow check）。⇒ 0.6.0 式「资源上的同步方法」（`session.interrupt`、`probes.probe`）只能在
   块外单发——这是草案 §4 保留同步方法的代价，迁移时每个同步调用点都要落在这个形状上。
3. **`call_concurrent` 的任务无法取消**，除非丢掉整个 store（wasmtime #11833）；块内也没有可靠的
   `select`/超时（#11869/#11870，超时必须包住整个 `run_concurrent`）。⇒ 草案 §3「取消＝丢弃流」
   不只是风格选择，是这条路**唯一**可用的取消信号。
4. **访客的后台任务只在异步回调的执行器里被 drain**（见上表末行）⇒ 任何「访客起个任务、立刻
   返回句柄」的形状，宿主那一侧要么走 async 导出（leg 1d 过的），要么保证实例上还有别的 async
   活动——后者不可依赖。

**复现**（产物都在 `target/`，不入库）：

```bash
cd target/async-spike/guest && cargo build --release --target wasm32-wasip2
cd ../host && cargo build && ./target/debug/spike-async-host.exe all   # 1/1c/2/3
./target/debug/spike-async-host.exe 1d                                 # async func(...) -> future
SPIKE_CHUNKS=1 ./target/debug/spike-async-host.exe 3s                  # 单条无门下行
./target/debug/spike-async-host.exe 4                                  # 宿主返回流/future（两种形状）
# 1b 会挂满 25s 再超时退出——那正是它的结论
```

**给 tau-ext 的第五件事**（leg 4 换来）：宿主侧凡「实现了返回流/future 的导入」的界面
（`process`、`http`、`ws`）都要在 `bindgen!` 里点名 `store`，并给每个宿主-owned 资源配
`with:` 映射；漏掉 `store` 的报错是「trait 上没有这个方法」，漏掉 `with:` 的报错是资源类型
不可构造——两条都不是运行时才暴露。

### 语言矩阵（本轮实测；工具链本机全部已有）

草案最容易招致的反对是「异步＝只有 Rust 能用」。实测不是这样——**把草案的四个 world 直接喂给
仓库里已有的全部生成器**（`wit-bindgen --world <w> --out-dir … wit/next/tau.wit`、`jco types -n <w>`、
`componentize-py -d … -w <w>`）：

| 工具链 | 版本 | 结果 | 异步 / 流的痕迹 |
|---|---|---|---|
| `wit-bindgen c` | 0.62.0 | `extension`/`provider`/`realtime`/`bridge` 四个 world 全部生成成功 | `exports_tau_extension_tools_execute` + `…_execute_callback` + `…_execute_return`（异步导出 ABI）；`typedef uint32_t …_stream_u8_t` / `…_stream_event_t` / `…_future_result_void_error_t`；`own_response_t` |
| `wit-bindgen cpp` | 0.62.0 | **只有 `extension` 成功**；`provider`/`realtime`/`bridge` panic（`not yet implemented`） | 生成器源码里就是 `TypeDefKind::Future(_) => todo!()` / `Stream(_) => todo!()`（`wit-bindgen-cpp-0.62.0/src/lib.rs:1750-1751`）：**C++ 目前写不了 provider/realtime/bridge**（能写 extension——async func 与 resource 它都过）。本仓的 C++ 例子是 extension，不受影响 |
| `wit-bindgen go` | 0.62.0 | 四个 world 生成成功 | `//go:wasmexport [async-lift]tau:extension/models@0.7.0#run` + `[callback]…`；`witTypes.StreamReader/StreamWriter/FutureReader`、`StreamVtable[uint8]`、`MakeStreamU8()`；`SessionFromBorrowHandle` + `UplinkAudio(LiftStreamU8(arg1))` |
| `jco types`（JS/TS，例内 node_modules） | — | `-n extension` / `-n realtime` 生成成功 | `run(request): Promise<[AsyncIterable<Event>, PromiseLike<Result<void, Error>>]>`；`uplinkAudio(audio: AsyncIterable<number>)`；`downlink(): [AsyncIterable<Event>, …]`；`body(): AsyncIterable<number>` |
| `componentize-py` | 本机 | `-w extension` 构建成功 | Python 侧是运行期绑定，能否 await 要跑了才知道 |

**这证明了什么**：草案的形状不是 Rust 专属——C、Go、JS/TS 三个工具链都能把 `async func` / `stream` /
`future` / `resource` 表达出来，且是以各语言惯用的形状（callback ABI、StreamReader/vtable、
Promise/AsyncIterable）。**这也没证明什么**：`wit-bindgen-cpp` 缺的正是 stream/future（逐字是 `todo!()`），
所以 C++ 访客暂时只能留在 `extension` world——这是**每个生成器各自的进度**，不是语言能力问题。
**这没证明什么**：生成器通过 ≠ 产物能编译、能跑；六个语言的例子要等迁移期 2 逐个过
（`docs/wasm-languages.md` 记着每语言的断点与补丁）。

访客工具的实测细节（写进契约注释的注释）：`write_all(..).await` 返回**未写出的余量**
（空＝全部写出），`write_one(..).await` 返回 `Option<T>`（`Some` ＝没写出去）；
`StreamReader` 侧用 `.next().await`；`wit_stream::new::<T>()` / `wit_future::new(..)`
由 wit-bindgen 生成；生产流需要 `async-spawn` feature。

## 6. 迁移分期

契约版本假设为 **0.7.0**（`package tau:extension@0.7.0;`）。仓库既有纪律是「宿主只
实现一个契约版本，旧组件在加载时被 `CONTRACT_VERSION` 拒绝并提示重建」，因此**不做
双契约窗口**——这是一次性切换。

| 期 | 内容 | 门禁 |
|---|---|---|
| 0（本轮） | 本文 + `wit/next/tau.wit` 草案；`wit/tau.wit` 不动，无组件重建 | 本文的 §5 证据；`cargo test --workspace` 与 `validate.sh` 不受影响（新文件不被任何构建引用，已核：`wit_bindgen::generate!`/`bindgen!` 全部走显式文件路径） |
| 1 | **前半已落地**（2026-09-29）：tau-core 的两类——错误枚举 `HostError`（`types.error`）与探针类型 `ProbePayload`（每点一臂，含 `SessionFacts` / `Branch`），宿主侧 12 个探针调用点、`faux.rs` 测试处理器、两个 wasm 适配器（`WasmProbes` / `BridgeProbes`，失败降级为 `continue` 并在 stderr 说明）、CLI 的权限门与三处 `observe` 全部改用类型；`cargo test --workspace` 全绿，clippy `-D warnings` 干净。**后半已落地**（2026-09-29）：tau-ext 宿主改造，四个 world 全部切完。**extension world 已切**（`exports: { default: async }` + `wasmtime_wasi::p2::add_to_linker_async` + 全部 `call_*` 改 await；`spawn_blocking` + std `Mutex` 换成 `tokio::sync::Mutex` + 直接 await，一次调用不再占一个阻塞线程；`off_runtime` 那块「runtime-free 线程」随最后一个 world 一起删掉，新入口是 `block_on_component`）。**provider world 已切**（`exports` + **`imports: { default: async }`** 一并上：provider 的 `run` 一生都在 `http.*` 里，宿主导入若还是同步实现，就会占住那个正被 await 的 worker——这恰恰是要消除的东西）。`http` 的五个导入改 async，其中 `request`/`read-body` 把阻塞的 reqwest 调用交给 `spawn_blocking` 再 await（注册表挪进 `Arc<Mutex<…>>`，锁只在阻塞体内持有，绝不跨 await），`status`/`header`/`close` 是纯查表、直接答；`stream` 从 `spawn_blocking(整个 run)` 换成 `tokio::spawn` + `call_run(..).await`。**realtime world 已切**（与 provider 同一形状：`imports` 与 `exports` 都是 `{ default: async }`，会话的三个上行导出 `push-audio`/`push-image`/`interrupt` 与 `close` 从 `spawn_blocking` + std `Mutex` 换成 `tokio::sync::Mutex` + await；`realtime()` 与 `load-realtime` 这两个同步 trait 方法经 `block_on_component` 驱动内部的 async 打开与 `open`）。**bridge world 已切**（四个 world 里要 await 的最多：`http` / `ws` / `process` / `ingress` 四组导入全部照同一形状走——阻塞的注册表挪进 `Arc<std::sync::Mutex<…>>`、整调用交给 `spawn_blocking` 再 await，锁只在阻塞体内持有；`process.spawn` 与 `http.status`/`header`/`close` 是锁内快查，直接答；host 通道的七个导出只做内存操作，只改 `async fn` 不加阻塞池。`BridgeFactory` 的代际计数从 `Cell<u32>` 换成 `AtomicU32`——factory 现在要借 `&self` 跨 await，那就得 `Sync`。它的 `handle-request` 还要从 tiny_http 的同步线程跨回 async，是四个世界里唯一保留 runtime 边界的一处，由 `block_on_component` 驱动）。一处按证据推迟到期 2：`store`/`with:` 是给「宿主实现返回流/future」用的，0.6.0 里没有这种导入（async 开关本身不用配：wasmtime 49 里 `Config::async_support` 已标 `#[deprecated(note = "no longer has any effect")]`，async 恒定可用）；另 `call_concurrent`/`run_concurrent` 同理——0.6.0 的每个导出都是同步签名，`exports: {default: async}` 生成的 `call_*` 仍是 `async fn(&mut store, …)`（ASYNC 无 STORE 标志 ⇒ 没有 `Accessor` 形态），真并发要等契约里的 `async func`。资源句柄表随契约的 `resource` 一起进期 2。**§5 的腿已实测过关**（宿主向访客流写入、跨块的资源句柄、`run_concurrent` 多会话，以及宿主实现方返回流/future），另有五条照单全收——消费者自备缓冲容量、同步调用只能在块外单发、取消＝丢流、访客后台任务必须挂在 async 导出下、宿主返回流/future 的界面要在 `bindgen!` 里点 `store` 并配 `with:` | spike 端到端；`cargo test -p tau-ext` |
| 2 | 契约切换：`wit/tau.wit` → 0.7.0 + vendored 副本 + `CONTRACT_VERSION`；examples 重建（`validate.sh` 的 15 个 Rust 组件 + 6 个非 Rust 语言的例子，后者按 `docs/wasm-languages.md` 的每语言断点重验）| `validate.sh` 的 wasm 腿全绿（11b/11c/11d、5b–5f、1f 等） |
| 3 | 文档随切换更新：`docs/extensions.md`（契约章）、`docs/realtime-av.md`（两会话方向）、`docs/host-channel.md`（订阅）、`docs/probes.md`（类型化点/载荷）、`docs/builtins*` 无关 | 文档与契约一致 |
| 4 | 新增一条门禁腿：0.7.0 契约的 provider 流式（组件产流、宿主拉、`done` 后 future 为 ok）+ 一条「宿主丢流 ⇒ 组件拿到未写余量」的取消腿 | 该腿在无宿主 async 实现时会红——这正是它存在的意义 |

## 7. 代价、风险与不做的事

**投影（owner 指示）的即时收益**：`convert.rs` 里「MIME 大类型推断媒介」和它带来的校验错误
（给 image 命名即错）消失——臂就是意图；两条词汇表（契约 4 臂 vs 宿主 7 臂）合一。代价是
`content` 与 `result-block` 的臂集必须同步（宿主转换器穷尽匹配，漏一个即编译错误）且加臂要求
访客重建（已在下面的演进成本里记账）。

**代价（访客侧）**
- wit-bindgen ≥ 0.62（async / stream / future / `async-spawn`）。Rust 访客在 stable
  `wasm32-wasip2` 上可用（§5 已证），**不需要** nightly `wasm32-wasip3`（那要等
  Rust 1.100 的 std，见 `docs/wasip3-streams.md`）。
- **本草案不要求 WASI 0.3**——两者是不同的层。`async func` / `stream` / `future` 是
  Canonical ABI（component model）的能力：wasmtime 49 里已**默认编译且默认开启**
  （`component-model-async` 是 wasmtime 的默认 crate feature，`CM_ASYNC` 默认取
  `cfg!(feature = "component-model-async")`），guest 仍是 `wasm32-wasip2`——实测
  examples 产物里 import 的是 `wasi:*@0.2.9`，宿主侧 linker（spike 当时用 `wasmtime_wasi::p2::add_to_linker_sync`，
  迁移期 1 后四个 world 统一换成同义的 `add_to_linker_async`）服务的是 `wasi:*@0.2.12`。**需要 WASI 0.3 的是另一件事**：guest 想用 WASI 自己的异步接口
  （`wasi:cli` 的 `read-via-stream`、p3 sockets）或直接编到 `wasm32-wasip3`——那要等 Rust
  stable 带 p3 std（本机 stable 1.98.1 的 `rustup target list` 里还没有 `wasm32-wasip3`）。
  WASI 0.3.0 已于 2026-06-11 发布且是稳定版（wasmtime 46 起默认带 CM async），所以这件事是
  一次**独立的后续小迁移**（examples 换 target + 宿主 p2 linker 换 p3），与契约形状解耦。
- 生产流的访客需要一次 `spawn`（创建流 → 派生写任务 → 返回读端）——这正是 WASI 0.3
  自己每个 `read-via-stream` 的形状，但它是新代码模式，examples 要逐个改。**并且这个导出
  必须是 `async func`**：同步降低的导出里 `spawn_local` 的任务永远不会被调度（§5 末行，
  leg 1b/1c/1d），所以草案里 `uplink-audio` / `uplink-image` / `process.stdin` 的
  `func(...) -> future` 一律加 `async`。
- **非 Rust 工具链：三个能，一个不能（本轮实测，见 §5 语言矩阵）**。C 生成 `_callback`/`_return`
  的异步导出 ABI 与 `stream`/`future`/`own_*` 句柄类型；Go 生成 `[async-lift]` +
  `StreamReader/StreamWriter/FutureReader` + `StreamVtable`；jco 生成
  `Promise<[AsyncIterable<Event>, …]>`。**C++ 不行**：`wit-bindgen-cpp 0.62` 对
  `future`/`stream` 是 `todo!()`，C++ 访客升级后只能留在 `extension` world（本仓的 C++ 例子
  正是 extension，不受影响）。**未验证的是编译与运行**（生成器通过 ≠ 六个语言的例子跑得起来），
  归迁移期 2，并要照 `docs/wasm-languages.md` 的每语言断点过一遍。

**代价（宿主侧）**
- wasmtime ≥ 49（`component-model-async`）。tau-ext 的实例模型从「互斥锁串行」变为
  并发：多会话不再互相阻塞，代价是**组件自身的状态由组件自己保护**——这条要写进
  `docs/extensions.md`（契约层面的行为变化，比形状变化更容易被忽略）。
- `ingress-handler` 与探针的差别要说清：前者 async 但宿主仍按实例串行；后者保持同步。

**契约演进成本（诚实记账）**
- 新增探测点 / 新事件种类 = 变体加一臂 ⇒ 访客**要重建**才知道新臂（0.6.0 是字符串，
  不必重建）。换来的是编译期的完备匹配。结论：值得（点集是 tau 的，且契约版本变更本
  就要求重建），但要写进契约文档。

**不做的事（本次明确排除）**
- `media-source.stream`：大载荷已有 blob 臂（内容寻址、已在盘上），加流臂会让
  `content` 的每一个载体都变成流的所有者；
- 流式请求体（`http.request` 的 body 仍是值：MCP JSON-RPC 与 OAuth 调用都不大）；
- `process` 的 env/cwd（宿主自己的过滤与 cwd 是同意故事，argv 才是同意界面展示的东西）；
- 探针异步化；
- `host.subscribe` 换流（理由见 §4）。

## 8. 未决（需 owner 定夺）

1. **是否按本草案推进**：这是一次重构而非新功能，工作量集中在 tau-ext 宿主与
   examples，收益是契约更小、更真、少一层同步代价。
2. **上行流的形状**（原「先后」问题已被 spike 消解）：宿主向访客写流**已实测过关**
   （§5 leg 1），不必回退到逐块 `push-audio(data) -> result`；唯一要 owner 点头的是签名
   从 `func(...) -> future` 改成 **`async func(...) -> future`**（leg 1b/1d：同步降低的导出
   里访客的后台任务不会被调度）。
3. **版本号**：0.7.0（本文假设）还是 1.0.0（若把 async 契约视为稳定性宣言）。
4. **`types.error` 三分**（`refused`/`failed`/`invalid`）：宿主需要在每个调用点归类，
   成本在宿主；组件则第一次能按「没同意 / 坏了」分流。
