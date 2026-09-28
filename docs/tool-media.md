# 工具媒体结果（F4）设计

> **状态：已落地（2026-09-27）。** 契约 `tau:extension@0.3.0`
> （breaking），随 tau 0.3.0 发布。实现严格按本文，唯一设计偏差是
> WIT 形状（递归 → 非递归 result-block），见下。
> 上游依据：`docs/wit-review.md` F4。

## 问题

工具结果只能是文本：`wit tool-result{content: string}` ↔
`ToolOutput{content: String}` ↔ `Content::ToolResult{content: String}`。
消息模型支持 image/audio/video/file 块，工具却无法返回媒体——截图
工具、文件生成工具、语音工具全部做不了，全模态故事断一环。

## pi 线格式事实（2026-09-27 查证）

- pi `ToolResultMessage.content = (TextContent | ImageContent)[]`——
  **多块，但仅 text + image**；image 在线上是内联 base64
  （`ImageContent{data: base64, mimeType}`），pi 自己的 read 工具
  即如此返回图片。
- tau session 的媒体本来就走 blob 引用（`sha256:<hex>`，请求边物化），
  与 pi 的字节级兼容本就不存在；兼容面在格式族（JSONL 树）。
  **F4 不引入新的 pi 不兼容。**
- 厂商边：Anthropic/OpenAI 的 tool_result 内容块只收 text/image——
  非 image 媒体必须在 provider 边降级，模型永远看不到音频字节。

## 契约改动（WIT 草样）

`types` 接口（消息模型）——落地形状与原草样有一处偏差：
原设计 `content: list<content>` 是**递归类型**（content 含
tool-result 含 content），wasmtime 的宿主侧 bindgen 对任何递归
WIT 类型直接拒编（"type depends on itself"，最小复现：
`record node { children: list<node> }` 同样失败）。落地改为专用的
**非递归 result-block 变体**——语义上也更正确：工具结果本就不该
含工具调用或嵌套结果，恰与 pi 的 `TextContent|ImageContent` 对齐：

```wit
/// 工具结果块。独立的非递归变体。
variant result-block {
    /// 文本块。
    text(string),
    /// 媒体块（image/audio/video/file）。
    media(media),
}

record tool-result {
    call-id: string,
    /// 多块结果；媒体字节过 ABI，永不 base64。
    content: list<result-block>,
    is-error: bool,
}
```

`tools` 接口：`use types.{result-block}`；`execute` 的返回改为同一
形状（`tool-result{content: list<result-block>, is-error}`，无
call-id——call-id 由宿主侧配对，访客不需要知道）。bridge world
export 同一 `tools` 接口，自动跟随。provider world 不动。
`content` 变体的 tool-result 分支同样收 `list<result-block>`；
核心侧 `Content::ToolResult.content: Vec<Content>` 不受此约束
（嵌套块在写回 WIT 时降级为文本投影）。

## 宿主改动面

- `ToolOutput{content: String}` → `{content: Vec<Content>}`；
  `ok/err` 文本便捷构造保留（内部包一层 `Content::Text`），
  新增 `ok_blocks(Vec<Content>)`。
- `Content::ToolResult{content: String}` → `Vec<Content>`。
- **session 读兼容**：旧条目该字段是 string。serde 自定义
  Visitor（或 untagged 中间表示）：string → `[Content::Text]`；
  写永远只写新形状。旧 session 文件必须无需迁移即可继续。
- wasm 访客返回的媒体字节：宿主落 blob store，session 持
  `sha256:` 引用，请求边物化——复用既有消息媒体路径，不发明第二条。
- 尺寸上限：按块沿用 host 通道同款上限思路；超限 fail-closed
  （工具结果进不了历史，错误返给模型侧为工具错误）。

## Provider 降级表

| content 块 | anthropic 边 | openai chat 边 | openai responses 边 |
|---|---|---|---|
| text | text block | tool 消息文本 | input_text |
| image | image block（base64 物化） | 尾随 user 消息携带 image_url 数据 URL（pi 同款模式） | input_image |
| audio / video / file | 文本占位符 `[audio: <media_type>]` 等 | 同左 | 同左 |

空结果补 `(no tool output)`；纯媒体无文本时 chat 边补
`(see attached image)`（均与 pi 一致）。

降级发生在 provider 序列化层（tau-openai/tau-anthropic），不在
tau-core——核心模型保持全模态，只有厂商线格式收窄。

## 验收清单（实现轮逐项打勾）

- [x] WIT 0.3.0；vendored 副本同步；0.2.0 组件拒载报错名版本
      （load 错误点名 "this host requires @0.4.0"）
- [x] convert.rs 往返测试含媒体 tool-result（bytes/blob/url 各一）
- [x] 旧 session 文件（string content）**读兼容**测试：不迁移、可继续
      （`string_or_blocks` untagged 反序列化；写永远只写块数组）
- [x] 超尺寸媒体 fail-closed 测试（块总和超 4 MiB 宿主通道上限 →
      TooLarge → 工具错误返给模型，不截断）
- [x] 新示例 `media-tool`（返回一张 1x1 PNG）：validate.sh 步骤 1b
      跑通；68 字节低于 blob 阈值，session 中落内联 base64；faux
      模型文本投影含 `[image: image/png]`
- [x] provider 降级单测（audio → 占位符文本；anthropic / openai
      chat / openai responses 三边各一）
- [x] 全部既有示例重建（Rust×7 + C/C++/Python/JS/TS/Go×6 真加载
      验收）；`cargo test --workspace` 140 绿 / clippy / validate.sh
