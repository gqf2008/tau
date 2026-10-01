# IM 通道：微信 / 飞书 / 钉钉 / WhatsApp 接入设计

> **状态：设计中，部分落地（ws 能力 + bridge world 三条腿 +
> 飞书回环示例，均 0.3.0）。** 代码不得先行于本文。
> 契约前置：`docs/host-channel.md`（入站注入）+ 本文的 `ws` /
> `ingress` 能力。
>
> **契约修正案（2026-09-28，jev 裁决 extend_bridge @ 0.990）**：
> bridge world 追加 `import host` + `export probes`——IM 适配器需要
> 三条腿同在（ws/http 网络 + host.steer 入站注入 + after_response
> 出站观测），原 bridge world 只有网络一条腿。裁决理由：host 链接与
> probe 调度代码路径既有，复用即可；world 文档新增 import 对已编译
> 旧组件零影响；probes 缺席即 no-op 是既有语义。否决项：独立
> im-bridge world（与 bridge 定位同物、契约面 +1）、本轮只加入站腿
> （出站降级为模型显式调 send 工具，偏离本文既定的 after_response
> 自动回话）。export probes 成为 bridge world 必需导出 ⇒ 既有
> bridge 示例须补空 probes 实现（0.3.0 本就是 breaking 列车）。

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

   **keepalive / idle 超时语义**（wit-review F9，写进契约注释）：
   bridge 长连是断线敏感场景，语义必须显式——
   - `ws`：宿主发 ping 保活（默认 30s，可调），P 秒无 pong 判定死亡
     并 `close`（带 reason）；组件侧 `recv` 在 idle 超时时收到显式
     error 而不是永远阻塞——「永远阻塞」会让断线窗口静默扩大
     （飞书断线窗口丢消息的 lesson）。重连与补拉是组件职责，
     宿主不代劳。
     - `ws::send` 的 Ok 语义（钉钉 ack 逼出来的修正案）：**Ok =
       已写进 socket**（actor 落盘后回执），不是「已入队」。诊断
       实录：异步入队语义下，print 模式会话在 actor 的 250ms 读
       滴答内退出，钉钉 ack 永远上不了线（mock 侧零帧 + 退出时
       RST）——与 webhook 侧的 honest-ack 红线同族，Ok 必须意味
       着事真的成了。代价是每次 send 最多等一个读滴答（≤250ms），
       IM 回执速率下无感。
   - `http`：每个 `read-body` 增量读带 idle 超时（无字节即超时 error），
     连接级 keepalive 由宿主 HTTP 栈负责；组件可用「提前关」主动
     断流。SSE 长连接同样适用 idle 超时——静默挂起的 SSE 与断线
     不可区分。**已落地（2026-09-28，`tau:extension@0.4.0`）**：
     `read-body(handle, max, timeout-ms)` 与 `ws.recv` 同形（0 拒绝、
     超时显式 error），宿主两条单测（0 拒绝；静默对端在预算内返回且
     句柄可续读）+ validate.sh 静默路由腿。
     **0.5.0 补齐同类面（wit-review F11）**：`http.request` 等响应头、
     `ws.connect` 等握手也各带 `timeout-ms`（0 拒绝、无响应即显式
     error）——IM 侧的现实是「连上了但平台不发帧/不回头」，与静默 SSE
     同族。本仓四条 bridge 示例统一 30s（`NET_MS`）。
     **0.7.0 形态修正（2026-09-29）**：F9/F11 的语义（永不永阻、静默
     即显式错误、honest-ack）全部保留，载体换了——guest 在
     wit-bindgen 0.62 下没有可 await 的时钟，契约删掉全部
     `timeout-ms` 参数，预算改宿主旋钮（`TAU_WS_CONNECT_TIMEOUT_MS`
     30s、`TAU_HTTP_REQUEST_TIMEOUT_MS` 30s、
     `TAU_HTTP_IDLE_TIMEOUT_MS` 120s），每条拒绝自报预算名与时长。
     接口同时资源化：`connect` 改 async、返回 `connection` 资源
     （drop 即 close）；`send` 改 async（await 完成 = 已写进 socket，
     钉钉修正案语义原样）；接收分两条腿——能 await 的消费者用
     `receive()`（返回 `tuple<stream<frame>, future<…>>`），同步
     probe 里的泵用 `poll()`（同步、永不等待；一个连接一个消费者，
     receive/poll 不分家，第二个调用者吃 `invalid`；到达过的帧先于
     终止原因交付）。
2. **`ingress` 能力**（webhook 平台：WhatsApp/企微）：consent-gated
   端口监听，宿主按路由把请求体喂给对应组件（UX 明示
   「该组件要监听 :8080/im/whatsapp」）。宿主依然不懂任何 IM 协议。

## 同步 guest 模型下的入站泵（实施时补录的现实约束）

组件只在自己的调用点同步执行（与 stream-subscribe.md 材料事实 1
同源）：宿主不能异步推进 guest ⇒ **组件内的 ws 帧只在组件被调用时
才能 drain**。IM 入站泵因此落在既有调用点上：`session_start` probe
建立 ws 长连接，此后每次 probe/tool 调用顺带 `ws::recv`（短超时）
drain 积存并 `host::steer` 注入。

- **保活不依赖 guest 调用频率**：宿主 ws actor 线程 30s ping /
  60s 无入站即判死（F9），空闲期连接照样活着，帧在宿主侧积存。
- **空闲期不泵是如实局限**：agent 完全静止时没有 probe 调用，入站
  消息要等下一个调用点才注入。生产级 IM 适配器需要一个 cadence
  driver（宿主定时调用点）——那是未来契约项，本轮不发明。
- loopback 验收（validate.sh IM 案例）因此设计为：turn 1 的
  `after_response` drain 到 mock 推来的消息并 steer ⇒ steer 落在
  当前回合后触发 turn 2 ⇒ turn 2 的 `after_response` 把回复 POST
  回 mock——全链路只用既有调用点，零新驱动机制。

**0.7.0 更新（2026-09-29）**：泵的载体与调用点都升级。`ws::recv`
（短超时）的角色由 `connection.poll()` 接任——同步、永不等待，
正是为「泵不能等」而生（sync 降低的导出不能 await 流读，派生的
任务也不会被调度，实测两次）。调用点在 probe/tool 之外新增两个：
webhook 的 `handle-request` 改 async（可 await），且宿主在每个
probe 应答后随即在同一上下文调 `bridge-io.turn(point, payload)`
——判定仍归 probe（同步、快进快出），等待（drain 帧、发回帖）
挪进 turn。「宿主 ping 保活不依赖 guest 调用频率」与「agent 完全
静止时没有调用点」两条如实结论不变；变化的是活跃期几乎每个
probe 点都附带一次可等待的 drain 机会。

## `ingress` 能力设计（webhook 平台入站，0.3.0 定稿）

WASI p2 没有 listen ⇒ **宿主起 HTTP 监听，按路由把请求喂给组件**。
宿主只做请求管道（方法/路径/头/体原样过手），不解析任何平台语义。

**形态（jev 裁决 push_export @ 1.000，2026-09-28）**：推送，不拉取。
bridge world 加 `import ingress`（`listen`/`close`，consent-gated）
+ `export ingress-handler`（`handle-request(request) -> response`）。
webhook 由平台主动发起 ⇒ 宿主收请求当即同步调组件，**入站泵问题
整个消失**（ws 腿「组件只在被调用时才能 drain」的空闲局限不存在；
host→guest 同步调用是既有原语，probes/tools 同款，含 trap 重建的
实例互斥锁）。HTTP 响应即组件返回值：WhatsApp 的 200 ack、企微
callback 的同步回包都自然落地。组件在 handle-request 里
host::steer 注入会话；回复仍走 after_response → 平台发消息 API。
否决项：拉取形态（recv/respond-by-request-id）——pending-request
簿记 + 应答超时 + 空闲不泵局限照搬进一个本可避免它的场景；
hybrid（listen import + 回调 export 拆两处）——契约面最大。

**宿主 HTTP 服务器（jev 裁决 tiny_http_dep @ 0.890）**：引入
tiny_http 0.12（钉死版本，依赖变更按仓规视同评审代码）——成熟
解析、同步模型贴合既有 actor 线程（process/ws 同款）。否决项：
std::net 手写最小 HTTP/1.1——手写解析器的长期安全维护责任归仓内，
为零新依赖不值。

### 契约与红线

- 只进 bridge world（IM 适配器是 webhook 的唯一消费者；普通
  extension 不给监听能力）。`export ingress-handler` 成为必需导出
  （0.3.0 breaking 列车同班）；无 webhook 的 bridge 补一个恒 501
  的桩实现（不 listen 就永不会被调用）。
- consent 维度是**监听地址**（新维度，与 origin 白名单正交）：
  CLI `--ingress <addr:port>`（可重复）；组件 `listen(route)` 时
  校验已授权地址，UX 明示「组件要监听 127.0.0.1:8080 的
  /im/whatsapp」；remembered consent 按指纹携带、merge sticky-on。
- TLS 终结不在组件也不在宿主 ingress——公网部署由隧道/反代终结，
  宿主只监听明文回环或内网地址。
- 请求/响应直通：方法、路径、头表、原始体原样过手；签名校验
  （WhatsApp 的 X-Hub-Signature-256、企微的 msg_signature）是
  组件职责——它持有平台 secret，宿主没有也不该有。
- 与 ws 腿并存不耦合：平台适配器按平台现实二选一。
- 并发语义：请求在实例互斥锁下同步调组件——组件正执行长 tool 时
  webhook 排队（平台重试是既有事实，如实记录；不是丢失）。

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

## 会话/身份映射配置文件（2026-09-28 定稿，jev 裁决 component_reads_file @ 0.79）

通道级单文件，JSON（仓内状态文件一律 JSON：consent/keys/trust 同款，
组件侧 serde_json 解析有 mcp-bridge 先例）。**组件自读**：宿主经环境
变量 `TAU_IM_CONFIG` 把路径交给 bridge，组件用 ambient WASI fs 读
（0.8.0：ambient WASI 是唯一的 posture，`--deny-wasi` 已删除）。
否决项：宿主解析后经 env 内联（宿主开始懂 IM 通道 schema，违背
「宿主不懂 IM 协议」分层红线）；只写规范不落地（清单不前进）。

```json
{
  "version": 1,
  "channels": [
    {
      "id": "feishu-main",
      "platform": "feishu",
      "endpoint": "wss://open.feishu.cn/...",
      "chats": {
        "oc_abc": {
          "session": ".tau/sessions/feishu-oc_abc.jsonl",
          "threads": "branch"
        }
      },
      "users": { "allow": ["ou_xyz"] }
    }
  ]
}
```

语义红线：

- **`endpoint` 必须与 consent 的端点一致**（组件对照 TAU_MCP_URL，
  不匹配 fail-loud）——配置不能偷渡一个没 consent 的端点。
- **未知 chat 的消息忽略**（notify 记录，不 steer）：映射是显式的，
  不存在「默认会话」。
- **`users.allow` 缺席或空 = 无人可说话**（fail-closed：身份是
  consent 问题，不是协议问题）。白名单外的 user 消息忽略。
- `session` 是会话文件路径：tau 是单会话 CLI，该路径给 supervisor
  /重启恢复用；单 run 内组件不切换会话（如实在示例里只回显）。
- `threads`: `branch`（平台 thread 键映射到会话内分支）|
  `session`（每话题独立会话文件）。本轮只记录不执行——分支导航是
  宿主侧能力，组件够不着，如实留白。
- 版本门禁：`version` 不认识即报错，不猜。

## 落地顺序

1. **前置契约**：host-channel（steer/follow-up/notify/emit + consent）
   + `ws` 能力 → 随 `tau:extension@0.3.0` 落地（0.2.0 未赶上，同班列车）。
2. **飞书先行**：长连接无需公网；一个适配器同时验证入站注入、出站
   观测、thread 会话映射、媒体入 blob 四件事。
3. **钉钉**：模式复制（同属 stream），差异在卡片/富文本映射。
4. **企微 / WhatsApp**：等 `ingress` 能力；WhatsApp 注意模板消息与
   24h 会话窗口。
5. **个人微信：不做。** 无官方通道，灰色协议风险不可控。

## 飞书回环协议（loopback mock，validate.sh IM 案例）

无真实飞书租户时的验收形态：mock 平台说**飞书形**协议（语义对齐
飞书长连接 + reply API，传输用 JSON 帧替代私有二进制帧格式——帧
编解码差异如实记录，协议翻译层结构不变）。

```
组件 →(ws connect, TAU_MCP_URL)→ mock：长连接建立
mock →(text 帧)→ 组件：{"type":"message","chat_id":"c1","user":"u1",
                        "text":"..."}        # 入站消息事件
组件 →host::steer(Message{Text})→ 会话       # 入站注入（会话注入 consent）
agent loop 跑出新回合
组件 after_response probe：从 assistant 消息抽文本
组件 →(http POST /reply, origin consent)→ mock：
       {"chat_id":"c1","text":"..."}         # 出站回复
```

会话映射：示例内内存映射 `chat_id → 当前会话`（loopback 单 chat）；
配置文件格式是清单独立项，不在示例里发明。媒体入 blob：示例不覆盖
（组件无 blob 能力，如实留白）。

## 企微回环协议（loopback mock，validate.sh 5e）

> 优先级：企微 adapter 先于钉钉（jev @ 0.980）——钉钉是飞书机制的
> 协议形复制，企微的增量是**组件侧签名校验 + AES 解密**：ingress
> 设计红线「签名校验（企微 msg_signature）是组件的职责，宿主不代劳」
> 在 whatsapp-bridge 里如实留白（loopback 无签名），企微把它做实。

企微 callback 的真实线格式（示例照实实现，不发明简化版）：

- **密钥派生**：EncodingAESKey 是 43 字符 base64，补 `=` 解码得
  32 字节 AES-256 密钥；IV = 密钥前 16 字节；CBC 无填充裁剪——
  明文尾部是 PKCS#7 填充。
- **明文帧**：16 字节随机前缀 + 4 字节大端消息长度 + 消息 +
  receiveid（必须等于本企业 corpid，防串企业投递）。
- **签名**：`msg_signature = sha1(sort(token, timestamp, nonce,
  encrypt_msg) 顺序拼接)`，query 参数携带，**每个请求都验**。
- **URL 验证**（GET）：query 带加密 `echostr`，验签 + 解密 +
  corpid 检查后**回明文** echostr。
- **消息推送**（POST）：body 是 XML 信封 `<xml><ToUserName/>
  <Encrypt/><AgentID/></xml>`；Encrypt 解开是内层 XML（文本消息
  含 `<FromUserName>`/`<Content>`）。立即 ack 回明文 `success`；
  异步回复走应用消息 API `POST /cgi-bin/message/send?access_token=…`
  （JSON 明文，**出站不加密**——签名/加密只管 callback 入站）。

回环形态（`scripts/wecom_mock.py` + `examples/wecom-bridge`）：

- mock 说**真实密码学**：纯 Python AES-256-CBC（stdlib 无 AES，
  内嵌实现 + 启动时 NIST 测试向量自证——仪器先证自己）+ sha1
  签名；token/AESKey/corpid 由环境变量配置（组件读
  `WECOM_TOKEN`/`WECOM_ENCODING_AES_KEY`/`WECOM_CORP_ID` 环境，
  缺失即报错不 listen——密钥不进配置文件、不进 consent 存储）。
- 验收腿（pty 驱动交互 REPL，同 5d 的理由）：
  1. **坏签名负对照先行**：mock 首发一个篡改签名的 POST ⇒ 组件
     403、不 steer、零回复（签名校验真的挡东西，不是摆设）；
  2. GET URL 验证：验签 + 解密 echostr ⇒ 200 回明文；
  3. POST 加密文本消息 ⇒ 200 `success` + steer 入会话 +
     idle REPL 唤醒回合 + after_response 走 send API 回帖；
     mock 的 `/cgi-bin/message/send` 打印 `WECOM SEND:` 断言。
- 无 `--ingress` 的拒绝腿与 5d 同机制（同一 consent 门），不重复
  验收；5e 的新断言全在密码学门上。

与 whatsapp-bridge 的结构差异如实记录：企微的 ack 可以携带加密被动
回复（5 秒窗口），示例仍选「立即 success + 异步 send API」——被动
回复要求 ingress-handler 同步等回合结束，与 push 模型的「请求在实例
锁下排队」语义冲突，如实不实现。

## 钉钉回环协议（loopback mock，validate.sh 5f）

> 优先级：企微之后补钉钉（jev @ 0.670，置信 0.560——0.3.0 发布是
> owner 拍板项，钉钉是路线图内可自主闭环的最后一格）。机制是
> 飞书的复制（ws 长连入站 + http 回帖），钉钉形的**真实增量**有两
> 个，示例就演示这两个：

1. **帧内回执（ack）**：钉钉 stream 模式要求客户端在**同一条 ws
   连接**上回 ack 帧（`{"code":200,"headers":{...},"message":"OK",
   "data":...}`），不回执平台会重投。飞书 loopback 只收不发，钉钉
   是 `ws::send` 的第一个真实用例。
2. **双层 JSON**：stream 帧是
   `{"specVersion","type":"CALLBACK","headers":{...,"topic":...},
   "data":"<转义后的 JSON 字符串>"}`——`data` 是**字符串装的
   JSON**，要解两层（外层帧 → data 字符串反转义 → 内层消息体
   `{"msgtype":"text","text":{"content":...},"senderStaffId":...}`）。

回环形态（`scripts/dt_mock.py` + `examples/dingtalk-bridge`）：

```
组件 →(ws connect, TAU_MCP_URL)→ mock：长连接建立
mock →(text 帧)→ 组件：钉钉形 CALLBACK 帧（data 为转义 JSON）
组件 →(ws send)→ mock：ack 帧 {"code":200,...}   # mock 打印 DT ACK
组件 →host::steer→ 会话（pump 语义同飞书：调用点 drain）
组件 after_response →(http POST /reply)→ mock：机器人回帖 API 形
```

- 真实 gateway 握手（POST /v1.0/gateway/connections 换 wss
  endpoint+ticket）不模拟——那是**获取连接地址**的步骤，回环里
  TAU_MCP_URL 直连即等价；差异如实记录。
- 会话/身份映射配置文件仍是飞书的演示，钉钉示例保持最小（单
  chat 内存映射）；卡片/富文本映射（文档说的真差异）超出文本
  回环范围，如实留白。
- 验收（print 模式即可——ws 泵语义与飞书相同，不需要 pty；
  前提是上面的 `ws::send` 同步修正案，否则 ack 会在进程退出前
  丢失）：mock 日志断言 `DT ACK:`（回执帧真的回了）+
  `DT REPLY:`（回帖到达），tau 输出断言 `steer: [IM dingtalk`
  （双层解码出的文本进入了会话）。

## 落地清单

- [x] host-channel 落地（见 docs/host-channel.md 清单，0.2.0 已落地）
- [x] `ws` 能力（0.3.0 落地）：bridge world `import ws`——
      connect/send/recv/close + text|binary 帧；宿主 actor 线程只做
      帧管道；与 http 同一套 origin 语义（ws:→http:, wss:→https:；
      0.8.0 起两边都没有 origin 白名单）；`--mcp-url` 接受 ws(s) URL；
      F9 语义入契约注释
      （ping 30s / 60s 无入站即 close 报因 / recv 必须带显式超时）。
      示例 `examples/ws-echo-bridge` + `scripts/ws_echo_mock.py`
      回环验收（validate.sh 步骤 5b）
- [x] bridge world 追加 `import host` + `export probes`（契约修正案，
      见文首；2026-09-28 落地）：宿主侧复用既有 host 链接与 probe
      调度（共享 free fn + BridgeProbes over SharedBridge），bridge
      steer 与 extension 同权（0.8.0：门与 `--allow-inject` 已删，
      安装即授权；落地时是 BridgeConsent↔RememberedConsent 双向携带、
      merge sticky-on）；
      既有 bridge 示例（mcp-bridge / ws-echo-bridge）补空 probes 导出
- [x] 飞书 bridge 组件（`examples/feishu-bridge`，2026-09-28 落地）：
      session_start 建连、after_response 先回帖后泵入站、steer 注入、
      chat_id 内存映射；真机教训入码——serde 内部标签 Content 线格式
      字段在标签前（`{"text":…,"type":"text"}`），抽取勿假设键序；
      mock 平台必须 threaded（ws 长连 handler 终身阻塞，回帖 POST 要
      并发服务）
- [x] 会话/身份映射配置文件格式（2026-09-28 定稿 + 落地）：规范见
      本文「会话/身份映射配置文件格式」节；feishu-bridge 经
      TAU_IM_CONFIG 读文件，endpoint 对照 consent、未知 chat 忽略、
      users.allow fail-closed；validate.sh 5c 加未授权 user 拒绝腿
- [x] `ingress` 能力（2026-09-28 落地）：bridge world `import ingress`
      + 必需 `export ingress-handler`；宿主 tiny_http 监听
      `--ingress <addr:port>`（0.8.0：监听地址是宿主配置，不是按
      组件发的 grant），把每个 webhook 请求同步推进组件导出
      （push 模型，无 idle 泵窗口）；`examples/whatsapp-bridge` + `scripts/wa_mock.py`
      回环；validate.sh 5d 双腿（pty 驱动交互 REPL：deliver→ack 200→
      idle steer 唤醒回合→reply POST；无 --ingress 时 listen 拒、零回帖）。
      配套修复：REPL 空闲时注入的 steer/follow-up 现在直接成为下一回合
      （此前只在运行中转发，空闲注入永远排队——idle-wake gap）
- [x] 企微 bridge 组件（`examples/wecom-bridge`，2026-09-28 落地）：
      组件侧 msg_signature 验签 + AES-256-CBC 解密（红线「签名校验是
      组件职责」做实）；契约修正——ingress request 记录补 `query`
      原始串字段（签名参数在 query 里，宿主是管道不解析；jev
      raw_string @ 1.000）；validate.sh 5e 三腿全绿：坏签名 403
      负对照 / echostr 验签解密回环 / 加密消息 steer→idle 唤醒→
      send API 回帖；mock 内嵌纯 Python AES 启动即 NIST 向量自证
- [x] 钉钉 bridge 组件（`examples/dingtalk-bridge`，2026-09-28 落地）：
      同连接 ack 回执帧（ws::send 首个真实用例）+ data 双层 JSON
      反转义；验收逼出契约修正案——ws::send 的 Ok 改为「已写进
      socket」（actor 回执确认，jev sync_send @ 0.540/0.45 弱分歧，
      按 honest-ack 红线族裁定同步）；validate.sh 5f 全绿（print
      模式：CALLBACK → ack → steer → 回帖 POST）
- [x] validate.sh IM 回环案例（步骤 5c，`scripts/im_mock.py`）：
      注入→steer→turn 2→回帖 POST 全链断言 + （0.8.0 前）无
      --allow-inject 时 steer 拒、零回帖的拒绝路径；该门已删
- [x] 0.7.0 资源/流式化（2026-09-29）：ws/http/process/ingress 的
      u64 句柄全部资源化（connection/response/child/registration，
      drop 即释放），`timeout-ms` 参数全部下线（预算改宿主旋钮，
      每条拒绝自报预算名与时长），`handle-request` 与 `ws.send` 改
      async，ws 增 `poll` 同步 drain 与 `bridge-io.turn` 异步调用点；
      语义红线（honest-ack、永不永阻、签名校验归组件、consent 维度
      不变）原样保留
