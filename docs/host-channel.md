# Host 回调通道：extension world 的 guest→host 主动通道

> **状态：已落地（契约 `tau:extension@0.2.0`，随 tau 0.2.0 发布；
> 0.3.0 起 bridge world 也 import host，见下）。** 本文是设计记录；
> 实施事实见 CHANGELOG 的 0.2.0 / 0.3.0 段。
> 是 `docs/im-channels.md` 与 `docs/realtime-av.md` 的契约前置。
>
> **0.3.0 追加**：bridge world 也 `import host`（docs/im-channels.md
> 契约修正案）——IM 适配器的入站注入腿；steer/follow-up 走同一
> inject consent 门（`--allow-inject` / remembered grant），宿主侧
> 与 extension 共用同一组实现。
>
> **0.8.0 注**：inject consent 门（`--allow-inject` / remembered
> grant）与整套 consent store 一并删除——steer/follow-up 的授权就是
> 「安装了这个组件」，本文其余部分保留当时的形状。

## 动机

extension world 今天是纯 export（`tools` + `hooks`）：宿主只在调用点
同步调组件，guest→host 只能搭返回值的便车（tool-result、probe
verdict），没有主动通道。对照 wassette/MCP：MCP 是双向协议（server 可发
logging/progress 通知、sampling、elicitation），组件是能回嘴的对端。

契约形态上 tau 早支持双向——provider world 有 `import events`，bridge
world 有 `import process/http`——缺的只是 extension world 的 import。

## 接口（设计定稿 v2，类型化）

v1 是 JSON 信封版；v2 按「参考 pi 的简约设计——简约在机制不在数据
结构」改为类型化主干 + 最小类型集。content 四态：text / media /
tool-call / tool-result——image/audio/video 本质同为 raw 数据，
MIME 主类型即语义（image/* audio/* video/*，其余=file），不另立
kind 枚举。

下文片段记的是当时（0.2.0）的形状；现行契约以 `wit/tau.wit` 的
`package` 行为准（当前 `tau:extension@0.7.0`）。

```wit
package tau:extension@0.2.0;

/// ---- 消息主干：tau 拥有 schema，用 WIT 类型（在 interface types 内） ----

interface types {
    enum role { user, assistant, tool }

    record media {
        /// MIME，如 audio/pcm;rate=24000；主类型即媒体语义。
        media-type: string,
        source: media-source,
        /// 原始文件名（仅 file 语义；image/audio/video 带 name 宿主拒绝）。
        name: option<string>,
    }
    variant media-source {
        /// 裸字节：二进制边界永不过 base64（tau_core::types 的一贯要求，
        /// 信封版恰恰违反它，类型化顺带修正）。
        bytes(list<u8>),
        url(string),
        /// sha256:<hex>，blob store 引用。
        blob(string),
    }
    record tool-call {
        id: string,
        name: string,
        /// 唯一保留的 JSON 叶：模型产的任意 JSON，无 schema 可类型化。
        arguments-json: string,
    }
    record tool-result {
        call-id: string,
        /// 仍是文本：媒体结果属 F4 评估项（0.3.0），不进 0.2.0。
        content: string,
        is-error: bool,
    }
    variant content {
        text(string),
        media(media),
        tool-call(tool-call),
        tool-result(tool-result),
    }
    record message { role: role, content: list<content> }
}

/// ---- 宿主回调：扩展的主动回传通道 ----

interface host {
    use types.{message, content};

    /// 用户可见通知；渲染层画文本块、媒体块出占位。不进模型历史。
    /// level: "info" | "warn" | "error"。
    notify: func(level: string, content: list<content>) -> result<_, string>;

    /// 发布扩展事实到事件总线（observe-only）。扩展自定义事实的
    /// schema 外生于 tau，信封保留；与 events.emit 统一返回 result。
    emit: func(event-json: string) -> result<_, string>;

    /// 以下两个改变执行，consent-gated。宿主校验 role==user 与尺寸
    /// 上限；类型错误由 ABI 编译期消灭，语义错误经 result 回报。
    steer: func(message: message) -> result<_, string>;
    follow-up: func(message: message) -> result<_, string>;
}

world extension {
    import host;
    export tools;
    export probes;   // 0.1.0 叫 hooks，0.2.0 正名（F7）
}
```

仍走 JSON 信封的叶（及理由）：`arguments-json`（模型产任意 JSON）、
`parameters-json`（JSON Schema 本身是 schema 语言）、probe
`payload-json`（每点一个 schema、高速演化、`tau probes` 可发现）、
扩展自定义的 `emit` event-json。provider world 的 `events.emit` 类型化
（消灭 audio-delta 的 base64 热路径）与 F6 的 result 化同属 0.2.0
breaking 批次，见 wit-review.md 修订记录。

## 0.3.0 追加：高频流订阅

`host` 接口在 0.3.0 增加 `subscribe`/`poll`/`unsubscribe`——F2 观测腿
的高频段（拉取订阅，有界环 + lagged 标记，句柄实例作用域）。设计与
裁决记录独立成篇：`docs/stream-subscribe.md`；语义红线与本文一致
（校验即错误、观测无门、绝不阻塞运行）。

## 0.7.0 追加：类型化错误 + 订阅资源化（2026-09-29）

- 四个调用（notify/emit/steer/follow-up）的 `result<_, string>` 换成
  `result<_, types.error>`（`refused` / `failed` / `invalid`）——
  「校验即错误」红线不变，但 guest 分支的对象是 variant arm 而不是
  英文字符串；detail 字符串留给人看日志。注意 wit-bindgen 0.62 里
  类型化错误的 `Display` 印的是 Debug 形（`Refused("…")`）——别
  解析字符串，匹配变体。
- `level` 与 `topic` 从注释里的字符串约定变成真枚举——未知
  level/topic 从「调用时拒」变成「不可表示」。
- `subscribe` 返回 `subscription` **资源**：0.3.0 的
  subscribe/poll/unsubscribe 句柄三件套塌缩成所有权——
  `subscription.poll()` 同步 drain（有界环 1024 + `lagged(n)` 语义
  原样），drop 即退订，trap 重建自然带走它。

## 语义红线（从现有脊柱继承，不得发明新时序）

1. **enqueue-only**：host 调用一律排队，绝不同步执行。probe 中途调
   `steer` 不重入 agent loop；落地时机就是控制通道现有 checkpoint
   规则（`docs/events.md` 规则 4：steer 在当前 turn 工具结果后，
   follow-up 在 run 自然结束时）。
2. **事实/决定二分**：notify/emit 是事实（对位 MCP logging/progress），
   steer/follow-up 是决定（对位 sampling/elicitation 的 tau 版——
   不向模型发问，向控制通道排队）。与「events are facts, probes are
   decisions」同一套二分，两条通道永不分叉。
3. **能力边界不稀释**：steer/follow-up 进 consent 体系（per-fingerprint
   remembered grant；UX 明示「该组件可向会话注入消息」）；notify/emit
   纯观测，永远可用。
4. **校验即错误**：`result<_, error>`（0.7.0 起类型化；之前是
   `result<_, string>`）回报非法输入（role 非 user、JSON 不合法、
   超过尺寸上限），不得静默吞掉。

## 类型化 ↔ serde 的边界（v2 论证）

pi 兼容约束的是 session 文件与 provider HTTP 两个 JSON 边，**不约束
组件 ABI**——v1 以此为理由全信封是错的。类型化后：WIT 类型在
package 版本内冻结，演化走 minor bump（0.x 语义），过渡期宿主同时
链接新旧版本；typed↔serde 转换在宿主侧唯一实现，CI 用往返属性测试
钉死（message → ABI → JSON == 原值）。类型系统在组件边界做编译期
保证——七种语言的工具链实测（docs/wasm-languages.md）证明 bindings
生成器都能吃下这套 record/variant。

## 兼容性（修正：不做双版本）

v1 写的「旧组件照常实例化」在 package 版本提升下不成立——组件模型的
接口身份含版本（`tau:extension/tools@0.1.0` ≠ `@0.2.0`）。0.2.0 契约
是 breaking batch（host import + hooks→probes 正名 + events.emit
类型化/result 化 + process.kill 返回 result，见 wit-review.md 修订
记录）：

- 存量组件需按 0.2.0 重建；宿主 load 错误必须**点名版本错配**
  （如「组件导出 tau:extension/tools@0.1.0，本宿主需要 @0.2.0」），
  不静默、不含糊。
- 宿主侧不做双版本链接：tau 是 0.x（CHANGELOG 明示任何 minor 可
  breaking），现存组件只有本仓示例，release.sh 每次发版重建。
  等有真实存量再评估双版本。
- 0.1.0 契约冻结于 git tag v0.1.0/v0.2.0 的 `wit/tau.wit`，需要回看的
  从标签取。

## 落地清单（已逐项落地）

- [x] `wit/tau.wit` → 0.2.0（本文接口）；tau-ext 的 vendored 副本同步
      （`wit_vendored` 测试盯漂移中）
- [x] tau-ext 宿主侧链接：notify/emit→事件总线
      （`AgentEvent::ExtensionNotice`/`ExtensionFact`，渲染层订阅）、
      steer/follow-up→控制通道（过 consent 门类；sinks 晚绑定——
      扩展先于 agent 加载，`wire_host_channel` 后接线，trap 重建的
      实例共享同一接线）
- [x] typed↔serde 转换层（`tau-ext/src/convert.rs`，宿主侧唯一实现）
      + 往返测试（全 content 形态 + 具名非 file 媒体/坏 arguments-json/
      超尺寸三条拒绝路径）
- [x] consent 新门类「会话注入」（`RememberedConsent.inject`，sticky-on
      合并，`tau consent --list` 可见，`--revoke` 收回）
      ——**0.8.0 已删除**（门与 consent store 一并撤，安装即授权）
- [x] 演示示例 `examples/notifier`（既有示例不动）
- [x] `docs/extensions.md`/`docs/events.md`/`docs/probes.md` 更新
- [x] CHANGELOG；`scripts/validate.sh` 加 consent 门案例（step 10b）
