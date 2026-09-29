# 高频流订阅（F2 观测腿高频段）设计

> **状态：已落地（2026-09-27）。** 契约 `tau:extension@0.3.0`（与 F4
> 同一班 breaking 列车）。实现严格按本文。
> 上游依据：`docs/wit-review.md` F2（「两条腿都要」：observe-only
> probe 低频腿已落地，本文是高频拉取订阅腿）。
>
> **0.7.0 形态修正（2026-09-29）**：`subscribe` 现在返回
> `subscription` **资源**——subscribe/poll/unsubscribe 的 u64 句柄
> 三件套塌缩成所有权（`subscription.poll()` 同步 drain，drop 即
> 退订，trap 重建自然带走），语义（有界环 1024、`lagged(n)`、拉取
> 不推、观测无门）原样保留。下文 0.3.0 的接口片段与清单保留原样
> （落地时的真实形状）。

## 问题

观测腿只有低频一半：`session_start`/`branch`/`session_end`
observe-only probe 已接线，但高频 delta（assistant 文本片段、
provider 音频段）只在事件总线上流动，wasm 扩展无从观测。
`events.md` 规则 3 禁止在高频路径上放 probe（模型与用户之间不能有
wasm 往返），所以高频观测必须是另一种形态；`probes.md` 的
`text_delta`/`tool_progress` 保留槽等的就是本文。

## 形态裁决（jev 2026-09-27：pull_buffer，置信度 1.000）

| 候选 | 裁决 |
|---|---|
| **A. pull buffer**：`host.subscribe` + `host.poll` 拉取，宿主侧每订阅一条有界环 | ✅ 采用 |
| B. cadence tick：新增 guest export `tick()`，宿主定时器驱动 | 否——改动调用模型，把墙钟驱动的 wasm 调用塞进 run loop |
| C. 并入 bridge：高频观测归长驻 bridge | 否——违背 F2 已录裁决（观测两条腿都要） |

材料事实（裁决依据）：

1. guest 只在自己的调用点同步执行（tool execute / probe / host 调用
   栈内）；宿主**不能**异步推进 guest ⇒ 「订阅」只能是拉取：宿主侧
   每订阅挂一条有界环，guest 在自己被调用时 drain。
2. 时效粒度 = guest 自己的调用频率。不伪装实时性：一个只在
   `after_tool` 被调用的扩展，poll 到的就是上次调用以来的全部积存
   （或溢出后的 lag 标记）。这是同步 guest 模型的诚实推论。
3. 总线是有界 broadcast（`BUS_CAPACITY = 1024`），慢订阅者收
   `Lagged` 跳过——本设计把同一语义原样延伸到 wasm 边界。

## WIT（`interface host` 追加）

```wit
/// 一段音频流事件。字节本身不上此通道（与总线同规则：字节进
/// assistant 消息的 Content::Audio，观测者只见计数与类型）。
record audio-segment {
    /// 本段字节数。
    bytes: u64,
    /// 所属段的媒体类型。
    media-type: string,
}

/// poll 拉取到的一条高频流事件。
variant stream-event {
    /// 订阅环溢出：此条之前有 n 条事件被丢弃。
    lagged(u64),
    /// assistant 文本片段。
    text-delta(string),
    /// provider 音频段。
    audio-delta(audio-segment),
}

interface host {
    /// 订阅高频流，返回句柄。topic 目录："text-delta" /
    /// "audio-delta"；未知 topic 报错点名（fail-loud，不放行静默的
    /// 空订阅）。run 之外（如 definitions 期）调用报错——订阅需要
    /// 已接线的总线。
    subscribe: func(topics: list<string>) -> result<u64, string>;

    /// 非阻塞 drain 该订阅的积存；无积存返回空表。环溢出时返回批次
    /// 的首条是 lagged(n)。未知句柄报错。
    poll: func(subscription: u64) -> result<list<stream-event>, string>;

    /// 退订并丢弃积存。未知句柄报错。
    unsubscribe: func(subscription: u64) -> result<_, string>;
}
```

## 语义红线

- **句柄作用域 = 组件实例**：receiver 存在 `ComponentState`（不在
  跨实例共享的 `HostChannel`）。trap 重建产生全新 ComponentState，
  旧 receiver 随旧实例 drop（broadcast 自动摘除），旧句柄失效——
  与 bridge 句柄代数同款防护，但实例隔离已够，不需要显式代数。
- **环容量 1024**（镜像 `BUS_CAPACITY`）；溢出语义 = 下一批首条
  `lagged(n)`。慢 guest 丢事件，运行永不被拖住（总线规则 1：emit
  从不检查订阅者）。
- **poll 非阻塞**：宿主侧 `try_recv` 循环，绝不在 tokio 运行时线程
  上 `block_on`（既有教训：同步 host 函数 block_on 必 panic）。
- **字节不上此通道**：audio-delta 只报 bytes + media-type。
- **不加 consent 门**：观测的是本运行的模型输出与工具活动——扩展
  本就在 tool/probe 载荷里看到同级内容，与 notify/emit 无门一致。
  跨会话订阅若未来出现再议门。
- **不发明事件**：`tool_progress` 在总线上没有生产者（无对应
  AgentEvent），本设计不覆盖；probes.md 保留槽维持 reserved。

## probes.md 保留槽处置

落地时把 `text_delta` 保留槽改写为指向 `subscribe(["text-delta"])`；
`tool_progress` 维持 reserved（无生产者，见上）。

## 验收清单（实现轮逐项打勾）

- [x] WIT `host` 三函数 + `stream-event`/`audio-segment`；vendored
      副本同步（`wit_vendored` 测试盯漂移）
- [x] tau-ext：subscribe/poll/unsubscribe 落在 ComponentState
      （实例作用域句柄：trap 重建 → 新 ComponentState → 旧句柄失效、
      旧 receiver 随实例 drop）；未知 topic / 未知句柄 fail-loud；
      lagged 标记正确
- [x] 测试（`stream_subscription_tests`）：订阅→注入事件→poll 往返
      （含 topic 过滤）；未知 topic 拒；未知句柄拒；环溢出产生
      lagged(n) 且首条留存事件是正确幸存者；run 外（未接线）
      subscribe 报错；unsubscribe 后重订阅只见新事件
- [x] 示例 `examples/streamer`：session_start 订阅 text-delta，
      before_run_end poll 并 notify 观测计数（after_tool 太早——
      faux 首轮工具调用前还没有文本 delta；before_run_end 时 renderer
      尚未 detach）；validate.sh 步骤 1c 断言
- [x] probes.md / events.md / wit-review F2 / CHANGELOG 更新
- [x] `cargo test --workspace` / clippy / validate.sh 全绿
