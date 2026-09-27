# IM 通道：微信 / 飞书 / 钉钉 / WhatsApp 接入设计

> **状态：设计草案，未落地。** 代码不得先行于本文。
> 契约前置：`docs/host-channel.md`（入站注入）+ 本文新增的 `ws` /
> `ingress` 能力。

## 架构对位：不为 IM 发明新概念

一个 IM 通道 = **一个 bridge 组件（协议翻译）+ 三条路径**：

```
IM 平台                    tau
  │  消息事件               │
  ├──────────────► bridge 组件 ──► host.steer/follow-up（入站注入，
  │   (长连接/      （协议翻译，     consent-gated，见 host-channel.md）
  │    webhook)      Content 拼装） │
  │                             │  agent loop 跑出回复
  │  发消息 API                 │
  ◄────────────── bridge 组件 ◄── after_response 观测点
  │               （http 能力，    （probe 拿组装好的 assistant 消息，
  │                origin 白名单）   verdict 恒为 continue）
```

- **出站零契约改动**：`after_response` 是九个已接线 probe 点之一，
  payload 即组装好的 assistant 消息（`docs/probes.md` #4）。
- **入站依赖 host-channel**：`steer`/`follow-up` 收完整 Message 线格式
  （文本/图片/文件块混排），是 IM 消息的天然落点。
- **会话映射**：channel 配置维护 `chat_id → session 文件`；话题/线程
  按平台线程键（飞书 `thread_id`）映射到分支或独立 session；图片/文件
  进 blob store 以 `Blob{hash}` 引用。
- **身份是 consent 问题不是协议问题**：IM user 白名单 / pairing 流程
  决定谁配跟这个 agent 说话。

## 平台现实矩阵（决定实现形态的关键差异）

| 平台 | 入站官方通道 | 要公网入口？ | 组件内纯客户端可跑？ |
|---|---|---|---|
| 飞书 | **长连接（WebSocket）**或 webhook；回复走 reply API，话题按 thread_id | 长连接**不要** | ✅（需 `ws` 能力） |
| 钉钉 | **Stream 模式（WebSocket，官方推荐）**；发送走 OpenAPI | **不要** | ✅（需 `ws` 能力） |
| WhatsApp | Business Cloud API **只有 webhook**；发送走 Graph API；企业主动发受模板消息 + 24h 窗口限制 | **要**（或隧道） | ❌（需 `ingress`） |
| 微信 | **个人号无官方 bot API**（灰色协议，封号风险，**明确不做**）；正规路＝企业微信应用消息 + callback（webhook）/ 公众号客服消息 | 企微 callback **要** | ❌（需 `ingress`） |

**wasm 硬约束**：WASI p2 没有 listen——组件里开不了 webhook 服务器。
因此平台分两类，各要一个新能力：

1. **`ws` 能力**（长连接平台：飞书/钉钉）：组件作为纯客户端主动连出。
   现有 `http.read-body` 已支持增量读 + 提前关（SSE/长轮询可用），但
   stream 模式是 WebSocket 帧协议——加 `ws.connect/send/recv/close`，
   origin 白名单同款 consent，宿主只做帧管道，协议解析全在组件里。
   与 `process`/`http` 同一切法：bridge 翻译协议，宿主只授窄能力。
2. **`ingress` 能力**（webhook 平台：WhatsApp/企微）：consent-gated
   端口监听，宿主按路由把请求体喂给对应组件（UX 明示
   「该组件要监听 :8080/im/whatsapp」）。宿主依然不懂任何 IM 协议。

## 已知运维坑（来自实机 lesson，直接进适配器设计）

- 飞书长连接断线窗口期会丢消息 ⇒ 入站注入必须补「重连后拉取遗漏」
  逻辑，不能只依赖推送。
- 飞书话题会话的键是 `thread_id`，回复必须走 reply 接口（不是
  create），否则出不了话题。

## 媒体映射

IM 语音 ↔ tau 音频块有格式转换问题（飞书要 opus、WhatsApp 要
ogg/opus、tau 内部 PCM）。ffmpeg 不进组件也不进核心——放宿主侧
media 工具能力，或交给 provider 侧（realtime API 多直接吃 PCM）。
图片/文件：入站下载 → blob store → `Blob{hash}` 引用；出站从
`Content` 块物化 → 平台 media 上传 API。

## 落地顺序

1. **前置契约**：host-channel（steer/follow-up/notify/emit + consent）
   + `ws` 能力 → `tau:extension@0.2.0`。
2. **飞书先行**：长连接无需公网；一个适配器同时验证入站注入、出站
   观测、thread 会话映射、媒体入 blob 四件事。
3. **钉钉**：模式复制（同属 stream），差异在卡片/富文本映射。
4. **企微 / WhatsApp**：等 `ingress` 能力；WhatsApp 注意模板消息与
   24h 会话窗口。
5. **个人微信：不做。** 无官方通道，灰色协议风险不可控。

## 落地清单

- [ ] host-channel 落地（见 docs/host-channel.md 清单）
- [ ] `ws` 能力：WIT + 宿主帧管道 + consent 门类
- [ ] 飞书 bridge 组件（新示例，不动既有示例）
- [ ] 会话/身份映射配置文件格式
- [ ] `ingress` 能力（排到企微/WhatsApp 之前）
- [ ] validate.sh 加一条 IM 回环案例（loopback mock 平台）
