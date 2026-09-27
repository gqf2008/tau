# wasip3 stream 迁移：工具链现状与解锁条件

> Spike 结论（2026-09-27 实机验证）。**迁移冻结到 Rust 1.100.0 进 stable**
> （预计 2026-11 中旬），届时按本文档的范围执行；在此之前不动 WIT。

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
