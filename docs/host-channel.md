# Host 回调通道：extension world 的 guest→host 主动通道

> **状态：设计定稿，未落地。** 落地以本文为准；代码不得先行于本文。
> 目标契约版本 `tau:extension@0.2.0`，随 tau 0.3.0 发布。
> 是 `docs/im-channels.md` 与 `docs/realtime-av.md` 的契约前置。

## 动机

extension world 今天是纯 export（`tools` + `hooks`）：宿主只在调用点
同步调组件，guest→host 只能搭返回值的便车（tool-result、probe
verdict），没有主动通道。对照 wassette/MCP：MCP 是双向协议（server 可发
logging/progress 通知、sampling、elicitation），组件是能回嘴的对端。

契约形态上 tau 早支持双向——provider world 有 `import events`，bridge
world 有 `import process/http`——缺的只是 extension world 的 import。

## 接口（设计定稿）

```wit
package tau:extension@0.2.0;

/// 宿主回调：扩展的主动回传通道。始终链接；改变执行的两个函数
/// 按签名指纹走 consent（与 process/http 同一姿势）。
interface host {
    /// 用户可见通知。content-json：Content 块的 JSON 数组
    /// （[{"type":"text",...},{"type":"image","media":{...}}]），
    /// 渲染层画文本块、媒体块出占位。不进模型历史。
    notify: func(level: string, content-json: string) -> result<_, string>;

    /// 发布一条扩展事实到事件总线（observe-only，不进控制流）。
    emit: func(event-json: string) -> result<_, string>;

    /// 以下两个改变执行，consent-gated。
    /// message-json：完整 Message 线格式（role + 有序 Content 块，
    /// 文本/图片/音频/视频/文件混排）。宿主校验 role=="user"、
    /// JSON 合法、尺寸上限；失败经 result 回报，不静默丢弃。
    steer: func(message-json: string) -> result<_, string>;
    follow-up: func(message-json: string) -> result<_, string>;
}

world extension {
    import host;      // 新增；旧组件不 import 照常实例化
    export tools;
    export hooks;
}
```

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
4. **校验即错误**：`result<_, string>` 回报非法输入（role 非 user、
   JSON 不合法、超过尺寸上限），不得静默吞掉。

## 为什么参数在 WIT 层是 `string`

信封约定，与 `parameters-json`/`arguments-json`/`payload-json`/
`event-json` 一致：消息线格式有**单一事实源**——serde schema 同时服务
session JSONL、provider HTTP、probe payload（`before_compaction` 的
`messages: [Message]`）。在 WIT 里重建 variant 等于把同一 schema 抄两份
（且 `ToolCall.arguments` 本就是任意 JSON，variant 化不彻底）。
string 是信封，schema 才是真类型；schema 以 `tau probes` 同款方式可发现。

## 媒体路径

- inline：`{"source":"base64","data":...}`（与 provider 边界现状一致；
  validate.sh 4b 步实测 3 MiB 跨界逐字节完整）。
- 引用：`{"source":"blob","hash":"sha256:..."}`——但 guest 今天没有
  blob 写能力，产不出新引用。**blob-write 能力 = 后续演进项，不进
  0.2.0 契约。**

## 兼容性

宿主始终提供 `host` import ⇒ 0.1.0 旧组件照常实例化；新组件 import 了
`host` 而宿主太旧 ⇒ load 期报错点名缺失接口（可诊断，不静默降级）。

## 落地清单（开工时逐项打勾）

- [ ] `wit/tau.wit` → 0.2.0（本文接口）；tau-ext 的 vendored 副本同步
      （`wit_vendored` 测试会盯漂移）
- [ ] tau-ext 宿主侧链接：notify→渲染层、emit→事件总线、
      steer/follow-up→控制通道（过 consent 门类）
- [ ] consent 新门类「会话注入」+ remembered-grant 生命周期
- [ ] 演示示例（不动既有示例，新增一个）
- [ ] `docs/extensions.md`/`docs/events.md`/`docs/probes.md` 更新
- [ ] CHANGELOG；`scripts/validate.sh` 加 consent 门案例
