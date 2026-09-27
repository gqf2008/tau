# 工具媒体结果（F4）设计

> **状态：设计定稿，未落地。** 落地以本文为准；代码不得先行于本文。
> 目标契约版本 `tau:extension@0.3.0`（breaking），随 tau 0.3.0 发布。
> 上游依据：`docs/wit-review.md` F4（评估已完成，结论在此展开为设计）。

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

`types` 接口（消息模型）：

```wit
record tool-result {
    call-id: string,
    /// 原 string；多块结果，媒体块与消息媒体同规则
    ///（字节过 ABI，永不 base64）。
    content: list<content>,
    is-error: bool,
}
```

`tools` 接口：`use types.{content}`；`execute` 的返回改为同一形状
（`tool-result{content: list<content>, is-error}`，无 call-id——
call-id 由宿主侧配对，访客不需要知道）。bridge world export 同一
`tools` 接口，自动跟随。provider world 不动。

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

| content 块 | anthropic 边 | openai 边 |
|---|---|---|
| text | text block | text |
| image | image block（base64 物化） | image_url |
| audio / video / file | 文本占位符 `[audio: <media_type>]` 等 | 同左 |

降级发生在 provider 序列化层（tau-openai/tau-anthropic），不在
tau-core——核心模型保持全模态，只有厂商线格式收窄。

## 验收清单（实现轮逐项打勾）

- [ ] WIT 0.3.0；vendored 副本同步；0.2.0 组件拒载报错名版本
- [ ] convert.rs 往返测试含媒体 tool-result（bytes/blob/url 各一）
- [ ] 旧 session 文件（string content）**读兼容**测试：不迁移、可继续
- [ ] 超尺寸媒体 fail-closed 测试
- [ ] 新示例 `media-tool`（返回一张小 PNG）：validate.sh 跑通，
      session 中落 blob 引用，faux 模型断言收到的结果块形状
- [ ] provider 降级单测（audio → 占位符文本）
- [ ] 全部既有示例重建；`cargo test --workspace` / clippy /
      validate.sh 全绿
