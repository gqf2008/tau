# wasip3 stream 迁移：工具链现状与解锁条件

> Spike 结论（2026-09-27 实机验证）。**迁移冻结到 Rust 1.100.0 进 stable**
> （预计 2026-11 中旬），届时按本文档的范围执行；在此之前不动 WIT。
>
> **2026-09-29 更新：冻结的解锁条件已提前满足。** wit-bindgen 0.62 让 stable
> `wasm32-wasip2` 就能编出 `async func` + `stream` 的组件（无需 nightly、无需
> `-Zbuild-std`）——等的是 wasip3 的 **std**，不是 Component Model 的异步 ABI，
> 本文此前把两者绑在了一起。见文末「追加 Spike（2026-09-29）」；契约层面的重设计见
> `docs/wit-redesign.md` 与草案 `wit/next/tau.wit`。

## TL;DR

- **guest 侧被上游阻塞**：`wasm32-wasip3` target 在 stable 1.97/1.98 的
  target 列表里，但 rustup 不发预编译 std（low-tier）；官方
  platform-support 页写明 **stable 自 1.100.0 起可用**（Tier 2，需
  LLVM 23 / wasi-sdk-34）。
- **host 侧今天就绪**：wasmtime 49.0.1 有 `component-model-async`
  feature，wasmtime-wasi 49 有 `p3` 模块。
- **现状边界已覆盖现实负载**：SSE 增量读 + 提前关闭
  （`http.read-body`）、`audio-delta` 推送通道、>10 MiB 请求逐字节
  完整过边界——realtime 形态的提供者今天就能写。

## Spike 证据矩阵（2026-09-27）

| 组件 | 状态 | 证据 |
|------|------|------|
| wasmtime 49.0.1 | ✅ 就绪 | `component-model-async` feature 在 Cargo.toml |
| wasmtime-wasi 49.0.1 | ✅ 就绪 | `pub mod p3` 在 src/lib.rs |
| wit-bindgen 0.46（guest API） | ✅ 就绪 | `StreamReader/StreamWriter/stream()`、`StreamWrite` future、rt 带 `libwit_bindgen_cabi_wasip3` |
| rustc stable 1.97.1 / 1.98.0 | ❌ 无预编译 std | `rustup target add wasm32-wasip3` → "no prebuilt artifacts available (low-tier)"；构建报 `can't find crate for core` |
| nightly 2025-08-08（本机） | ❌ 太旧 | `rustc --print target-list` 不认识 wasm32-wasip3 |
| 官方 platform-support 页 | ✅ 解锁条件 | Tier 2；**first available on stable in Rust 1.100.0**；需 LLVM 23、wasi-sdk-34（本地链接时）、LLD |

最小 spike 工程在 `target/wasip3-spike/`（`spike.wit` 一个
`run: func(chunks: u32, size: u32) -> stream<u8>`；guest 用
`wit_bindgen::stream()` + `spawn` + `writer.write(data).await`）——
guest 代码本身通过了 wit-bindgen 0.46 的宏展开，死在 std 缺失，
证实阻塞点在上游发布物而非我们的用法。

## 追加 Spike（2026-09-28，nightly 1.101）：std 阻塞已解，async ABI 有工具链错位

`rustup check` 出现 nightly 1.101.0（2026-09-26）后重跑 spike，结论更新：

| 检查 | 结果 | 证据 |
|------|------|------|
| nightly 1.101 发 `wasm32-wasip3` 预编译 std | ✅ 已解锁 | `rustup target add wasm32-wasip3 --toolchain nightly` 成功（stable 1.97/1.98 仍 low-tier 无产物） |
| guest 编译出真 wasip3 组件 | ✅ | `spike_guest.wasm`（101 KB），`wasm-tools component wit` 见 `run: func(u32, u32) -> stream<u8>` 导出 |
| **sync lift 的导出返回 stream：调用通，但 spawn 的写任务永不驱动** | ❌ 死路 | host 侧 `call_concurrent` 正常返回 stream，但 `canon lift` 无 `async`/`callback` 的导出在返回后不再有事件循环入口；guest 内 `wit_bindgen::spawn` 的 writer 永远是 "uninteresting spawned thread"，host `poll_no_interesting_tasks` 立即 ready、consumer 零字节 |
| **async lift 全链路** | ❌ 上游错位，双向都堵 | ① `generate!(async: true)` 产出 `canon lift … async (callback)`，但组件类型里的 func type 仍标 sync → wasmtime 49.0.1 校验拒绝："the `async` canonical option requires an async function type"（wasmparser 0.258 规则）；② 改用 WIT 源注解 `run: async func(...)`（wit-parser 0.239+ 支持该语法）→ 组件类型正确标 async，但 wit-bindgen 0.46 把导出名编码为 `[async]run`，nightly 自带的 wasm-component-ld 拒收（"not in kebab case"）——wit-bindgen 0.46 与 LLVM 23 时代 componentizer 的编码约定错位 |
| host 侧消费 API 已探明 | ✅ 备档 | `store.run_concurrent(async |accessor| …)` + `func.call_concurrent` + `Val::Stream` → `try_into_stream_reader::<u8>()` + `reader.pipe(store, impl StreamConsumer)`；consumer 每次 accept 后唤醒 host future 注册的 waker（不能依赖 executor 自动重 poll）。注意 `Config` 需 `wasm_component_model_async(true)` + `concurrency_support(true)`；`async_support` 已废弃无效果 |

**冻结结论不变**：等 stable 1.100 + 与之对齐的 wit-bindgen/wasmtime 版本（届时 `[async]run`
编码与 async func type 校验应已对齐）。nightly 已能编译 wasip3 guest 意味着 1.100 进
stable 当天即可重跑本 spike 验证解锁。spike 工程（含 host 驱动器）在
`target/wasip3-spike/`（guest 一个导出 async stream 的组件；host 一个 wasmtime 49
concurrent API 驱动器，当前死于 guest 链接/宿主校验两道上游错位之一，视 WIT 注解而定）。

## 为什么等，而不是硬上

1. **分发故事不接受 nightly**：`docs/extensions.md` 给作者的承诺是
   `rustup target add wasm32-wasip2; cargo build`。stream 组件若要求
   nightly + `-Zbuild-std`，0.x 的「可被真人使用与分发」目标直接
   破产。
2. **没有输入侧负载在等**：tau-cli 是终端应用，无 mic/camera 采集；
   实时提供者的真实形态（duplex 走自己的网络连接 + SSE 增量 +
   audio-delta 推送）在现有边界上已成立。缺的不是能力，是一个更
   省拷贝的 ABI。
3. 解锁是**时间确定**的：Rust 六周一列，1.100 ≈ 2026-11 中旬，
   等两个月换「作者工具链不变」，值。

## 解锁后的迁移范围（设计草案）

1. **WIT**：provider 世界引入 stream 形态（候选：`run` 返回
   `stream<u8>` 取代 events.emit 推送，或 events 增加
   `emit-stream`）；`process`/`http` 的 `list<u8>` 块式读写可顺势
   换 stream。保持 JSON 事件通道可链接（0.x 允许破坏，但没必要
   人为制造）。
2. **host（tau-ext）**：wasmtime 开 `component-model-async`；linker
   换 async 变体；store 全面 async 化——现在
   `spawn_blocking + Mutex<SharedInstance>` 的调用模型要重审
   （stream 让一次调用跨多个事件周期存活，互斥粒度与 revive 语义
   都要重新设计）；canonical ABI 的 backpressure
   （`backpressure_set/inc/dec`）与取消传播要定策略。
3. **guest（五个 examples）**：target 换 `wasm32-wasip3`，推送侧换
   `stream()/StreamWriter`；门禁沿用——逐字节对账（probe/fnv1a 模式
   直接平移到 stream 读端）、trap 降级、revive。
4. **风险点**：async linker 与现有同步调用模型的冲突是最大的一
   块；stream 的半开/取消状态机比「一次 call_run 要么成要么 trap」
   复杂一个量级，测试要先于实现铺好。

## 触发条件

本机 stable ≥ 1.100 可用时：`rustup target add wasm32-wasip3`，
重跑 `target/wasip3-spike/guest` 的构建；绿，则立迁移项并按上面
的范围排期。


## 追加 Spike（2026-09-29，wit-bindgen 0.62 + **stable** wasm32-wasip2）：解锁条件已提前满足

本文的冻结理由之一是「stream 组件若要求 nightly + `-Zbuild-std`，`rustup target add
wasm32-wasip2; cargo build` 的分发故事直接破产」。**该理由不再成立。**

| 检查 | 结果 | 证据 |
|------|------|------|
| wit-bindgen 0.62（2026-09-10）修好 async 导出的 canon 编码 | ✅ | 2026-09-28 那条卡在 wit-bindgen 0.46 把导出编码成 `[async]run`、被 LLVM 23 的 componentizer 拒收；0.62 改成 `[async-lift]…` + `[callback]…`，与 wasm-tools/wasmtime 两侧一致 |
| **stable** 工具链（rustc 1.98.1）编 wasip2 的 async 流组件 | ✅ | `target/wit-probe/guest-realtime/`：`wit-bindgen = { version = "0.62", features = ["async-spawn", "inter-task-wakeup"] }`、`world realtime`、`cargo build --target wasm32-wasip2 --release` → `probe_realtime.wasm` 163 KB。**没有 nightly、没有 `-Zbuild-std`** |
| 产物里真是异步提升 | ✅ | `wasm-tools print`：`(canon lift (core func "[async-lift]tau:extension/models@0.7.0#run") … async (callback …))`（`[static]session.create` 同）；`wasm-tools component wit` 读回 `run: async func(request: request) -> tuple<stream<event>, future<result<_, error>>>` |
| host 侧（wasmtime 49.0.1）绑定 | ✅ 编译并运行 | 四个 world 全部 `bindgen!` 通过；async 按界面开——`imports: { default: async }` + `exports: { default: async }`（**没有** `async: true` 这个键） |
| host 侧驱动一条 guest→host 流 | ✅ 既有 spike | 本目录的 host 驱动器：4×4096 字节逐字节校验 |
| 非 Rust 生成器 | ✅ C / Go / JS，❌ C++ | `wit-bindgen c`：`…_execute_callback` / `…_execute_return` + `stream`/`future` 句柄 typedef；`wit-bindgen go`：`[async-lift]` + `StreamReader/StreamWriter/FutureReader` + `StreamVtable`；`jco types`：`Promise<[AsyncIterable<Event>, …]>`。**`wit-bindgen-cpp 0.62` 对 `future`/`stream` 是 `todo!()`**（`src/lib.rs:1750-1751`），C++ 访客暂时只能留在 `extension` world |

**结论更新**：wasip3 的 *std* 仍等 1.100（本文原判断没错），但 **Component Model 的异步
ABI 今天就能在 stable + `wasm32-wasip2` 上用**。于是「等」的理由只剩一条：等一个更省拷贝
的 ABI 不是必须的——因为异步 ABI 本身已经可用。

仍待验证（迁移 spike 的腿，逐条列在 `docs/wit-redesign.md` §5）：宿主向访客**写**流
（上行方向）、宿主持有访客资源句柄的运行时行为、`Store::run_concurrent` 下的多会话并发。
