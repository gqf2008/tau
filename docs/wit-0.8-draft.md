# WIT 0.8.0：减法草案、裁定记录与 OS 模块设计对账

> **状态：已落地（0.8.0 减法随本轮提交；独立审查与合并见 walgit 协作层 `wit-0.8-subtraction`
> 线程 `wit-0.8.0-subtraction`）。** 契约本体是 `wit/tau.wit` 与 `crates/tau-ext/wit/tau.wit`（均为
> `tau:extension@0.8.0`，两份逐字节相同）；中文阅读副本是 `wit/tau.zh.wit`（剥掉注释后
> 声明行 264 行与英文版逐字节相同）。本文是这一版的裁定与复核记录：三条裁定、Jev 五轮
> 第二意见、与 OS 模块设计的对账，以及 0.9.0 的候选。0.7.0 的文本在 git 历史里。

## 1. 一句话

三条裁定把契约从「四个 world、十二个 interface」减到「两个 world、十个 interface」：契约只保留
只有契约才能做的事——**工具、探测点、宿主回传通道，以及跟外部世界说话的那几个能力**。

## 2. 裁定（owner，2026-10-01）

| # | 裁定 | 当场记录的理由 | 主要后果 |
|---|---|---|---|
| 1 | 删掉所有运行时能力门，`--deny-wasi` 一并删除 | 门不是墙：ambient WASI 默认全开，组件直接 import `wasi:sockets` / `wasi:filesystem` 就绕开了受门保护的接口（docs/extensions.md §7、wit-review F1）。半真的门只让每个用户多学一套词汇，换不到安全 | 安装 / 信任签名成为唯一授权动作；组件以 tau 进程权限运行；签名只回答「这是哪个组件」，不回答「它能做什么」 |
| 2 | provider 删除，模型内置 | LLM 是大脑核心，不应是扩展点：模型集合封闭、由本仓库决定 | 删 `world provider` / `world realtime` / `interface models` / `interface session`；模型覆盖变成发布节奏，逃生舱是 OpenAI 兼容 `--base-url` |
| 3 | 音视频不跨 ABI | guest 没有可 await 的时钟，实时截止时间不是它能满足的东西——这是结构性事实，不是性能测量；设备 / 时钟 / 抖动缓冲 / 播放归宿主 | 媒体面全程宿主内部；订阅保留音频臂，但只给计数、永不给字节 |
| 4 | ABI 兼容政策 = **P2**：0.x 允许破坏，1.0.0 起承诺向后兼容 | 1.0.0 的语义本来就是「契约面稳定」；现在就做多版本接口并行（P3）成本最高，等真实第三方组件压力再说 | `CONTRACT_VERSION` 精确匹配保留到 1.0.0；**兼容承诺要写准**：新增接口 / 新增导出可以，给宿主→guest 的 variant 加臂**不行**（老组件解码撞上未知判别值直接失败，这点与 JSON 不同）；加载失败必须点名双方版本；不做多版本 shim |
| 5 | 调用约定统一（§6.5）落 **0.9.0**，不进 0.8.0 | 0.7.0 才发布一天，外面还没有第三方组件，「访客重建两次」的成本约等于零；0.8.0 保持「只删不改」的干净身份，机制单独一版、单独浸泡 | 0.8.0 文档必须给 `host.subscribe` / `ws.poll` / `bridge-io.turn` 标「0.9.0 待改」；§6.5 的形状进 0.9.0 路线图 |

裁定 2 的回来条件写在契约头里：长尾厂商或自建网关若被证明无法通过内置 provider 加 OpenAI
兼容端点触达，回来的路是**另一个包，不是这一份**，这样语言矩阵与 1.0.0 的契约冻结都不受扰动。

## 3. 草案改了什么（0.7.0 → 0.8.0）

| 维度 | 0.7.0 | 草案 0.8.0 |
|---|---|---|
| world | 4（extension / provider / realtime / bridge） | 2（extension / bridge） |
| interface | 12 | 10 |
| `types.error` | `refused` / `failed` / `invalid` | `failed` / `invalid`（没有生产者的臂不该留在契约里） |
| `host.topic` | `text-delta` / `audio-delta` | 仍是两臂，但 `audio-delta` 变成只观察、只计数 |
| async ABI 门槛 | 所有组件（模型侧要 `stream` / `future`） | 只有桥 world 要；工具与探测侧只剩 `async func`，没有 stream / future |
| 规模 | 1018 行 | 848 行（英文）/ 689 行（中文） |

删除：`world provider`、`world realtime`、`interface models`、`interface session`、
`types.error.refused`、`host.audio-segment` 的字节语义（计数臂保留）。
保留：`tools`（含媒体结果块）、`probes`（12 点，同步、payload / verdict 类型化）、
`bridge-io.turn`、`host`（notify / emit / steer / follow-up / subscribe）、
`process` / `http` / `ws` / `ingress` / `ingress-handler`。

## 4. 第二意见：Jev（五轮）

模型锁定 `typesafe/jev-1.13-20260917`。密钥来源：**用户级**环境变量 `OPENROUTER_API_KEY`
（`doctor` 直接跑会失败，只是因为新进程没继承它；从注册表读出即可——前几轮记录的「Jev 第二意见
未取得」由此解除）。前三轮合计约 $0.0003；第四、五轮（调用约定与入口形状）记录在
§6.5，五轮合计约 $0.00042。

### 第一轮：七问并行

| 问题 | 结论 | 概率 / 置信 |
|---|---|---|
| 草案下一步 | `ratify_with_changes` | 0.91 / 0.87 |
| world 结构 | `two_worlds` | 0.84 / 0.76 |
| provider 删除的后悔风险（12 个月内被迫加回） | p = 0.65 | 未过 0.7 阈值 |
| 删门降低实际安全 | p = 0.31 | —— |
| 保留只读音频计数臂 | p = 0.54 | —— |
| ingress 地址由组件声明 | p = 0.26 | —— |
| 草案质量（0–4 档） | 2.61，最可能第 3 档「明显改善」 p = 0.79 | 0.66；另有 0.10 落在第 0 档「明显倒退」 |

### 第二轮：对冲

| 问题 | 结论 | 概率 / 置信 |
|---|---|---|
| 独立实验性包能否消掉长尾风险 | p = 0.44 | —— |
| 现在最该做什么 | `side_contract`（落独立包对冲） | 0.86 / 0.81 |

### 第三轮：形态与时序（低于门限，未采纳为裁定）

| 问题 | 分布 | 置信 |
|---|---|---|
| 对冲形态 | `wit_side_package` 0.58 · `host_proc_protocol` 0.34 · `doc_only` 0.07 | 0.44 ⚠ |
| 现在做 0.8.0（接受浸泡期归零） | p = 0.60 | —— |
| 对冲与 0.8.0 同批还是之后 | `later` 0.61 · `same_batch` 0.38 | 0.41 ⚠ |

**已采纳**（第一、二轮高置信结论）：音频计数臂回归；保持两个 world；provider 不重开本契约，
但把回来条件写进契约头。**未采纳**（第三轮低于 0.6 门限，脚本自己提示转人工）：对冲的形态与
时序，留给 owner。

**用法边界**（技能文档明写，记录于此以免后人误读）：`noul` 返回的是「为真的概率」，不是校准
置信度；同一问题换问法答案会不自洽（第三轮已露出该现象）；中文准确率官方承认偏低。**这些数字
只用来看方向与相对大小，不做流水线门禁。**

## 5. 与 OS 模块设计的对账

| OS 模块设计 | tau 0.8.0 | 对齐 |
|---|---|---|
| Linux：`insmod` 就是信任决定，内核模块没有逐能力门（签名 / lockdown 是准入） | 安装 / 信任签名 = 唯一授权，运行时零门 | ✅ 准 |
| Windows：UMDF 低权进程 + HVCI / PatchGuard 一类运行时策略 | 无（ambient WASI 全开，组件 = tau 进程权限） | ⚠️ 明确选了 Linux 那侧 |
| LSM hook 链：同步、可否决、按序折叠、返回 0 即放行 | `probes`：同步、`block` 否决、按加载序折叠、trap 降级 `continue` | ✅ 几乎逐条对应 |
| tracepoint / ETW：只能观察 | `session-start` / `branch` / `session-end` | ✅ 准 |
| perf / trace 环缓冲 + 「events lost」 | `host.subscribe` 1024 环 + `lagged(n)` | ✅ 准 |
| 中断与定时器归内核，驱动用 DPC / workqueue 延后 | 设备 + 时钟 + 抖动缓冲 + 播放归宿主，guest 只做信令（裁定 3） | ✅ 准 |
| Windows：稳定驱动 ABI + INF + catalog，跨版本可用 | 分发像 Windows（签名、指纹、OCI），**ABI 像 2005 年的 Linux**（package 精确匹配，不匹配即加载失败；加一个 variant 臂要所有访客重建） | ❌ 半 |
| Linux：`vermagic` / `modules.dep` / `modinfo` / `rmmod`（依赖、别名、参数、优先级、卸载） | 无依赖与优先级元数据；同名工具 **last-wins**（docs/builtin-tools.md:81）；组件载入后无 unload / reload | ❌ 缺 |

两处没对齐的正是「灵活扩展」的瓶颈，见下一节。

## 6. 灵活扩展的缺口（0.9 候选）

能做的（覆盖面比预期宽）：任意工具；拦截与改写循环（12 个探测点的 `replace` / `block`：脱敏、
护栏、成本记账、上下文裁剪）；向会话注入（`steer` / `follow-up` / `notify` / `emit`）；外部协议桥
（MCP / IM / webhook / 私有 socket）；工具返回媒体（原始字节）；观察流式文本。

做不到或很别扭的——四个原语（契约里 `timer|sleep|wakeup|interval|schedule` 零命中，
`render|ui` 只出现在无关注释里）：

1. **定时 / 唤醒**。组件只在宿主调用它时活着，于是「每天汇总一次」「每 5 分钟轮询 CI」
   「空闲时重建索引」这类功能写不成组件（只能靠 ingress 被外部打，或做成桥去 spawn 自己的
   进程）。建议形状：`host.timer` 资源（到期由宿主回调），或让 guest 在自己的 async 导出里
   await 宿主提供的 sleep。定时必须由宿主驱动——guest 没有时钟（这正是裁定 3 的同一条理由）。
2. **UI / 渲染**。`host.notify` 只能画文本 + 媒体占位；自定义 TUI、状态栏、diff 视图、提示音
   只能改宿主。建议：要么明确写进「不做的事」，要么给一个只读的渲染事件订阅。
3. **存储 / 会话后端**。会话固定是本地 JSONL 树；团队共享、远端同步、加密存储都做不了。
   建议：`session-store` 接口，先做只读观察比先做写更值。
4. **凭据**。宿主持有 secret 的那条路随 provider 一起删了；调需要 OAuth 的服务（GitHub /
   Google）得组件自己管 token、自己存盘（能跑，ambient fs 全开），但没有统一的存取与刷新语义。
   内核 keyring / Windows Credential Manager 的位置现在是空的。

另外两条（第 5 节的两处未对齐）：

- **ABI 兼容范围**：组件声明 `host-api >= 0.8, < 1.0`，宿主在范围内做适配层，而不是精确匹配即
  失败。这条**要先定**，因为它影响 `CONTRACT_VERSION` 的匹配语义与 1.0.0 的冻结口径。
- **模块元数据与装载语义**：依赖声明、同名工具的提供者优先级（现在是 last-wins，等于不确定）、
  加载失败的局部回退、unload / reload。Linux 有 `modinfo` / `modules.dep`，Windows 有 INF；
  这一层比再删两个 interface 更值钱。

还有一条 world 拆分带来的限制值得记着：**只有桥能 spawn 进程**，所以「一个组件既提供工具又跑
后台任务」目前不成立。这是合并 world 的代价；上一轮 Jev 判保持两个 world，代价记在这里。

## 6.5 guest ↔ host 的调用约定

### 现状：四套答案、三个补丁

| 方向 | 入口 | 等待规则 |
|---|---|---|
| host→guest | `tools.definitions` / `tools.execute` | async |
| | `probes.probe` | 同步（宿主暂停等判定） |
| | `bridge-io.turn` | async —— 补丁 ① |
| | `ingress-handler.handle-request` | async |
| guest→host | `host.notify` / `emit` / `steer` / `follow-up` | 同步调用、入队、不重入 |
| | `host.subscribe` + `subscription.poll` | 拉 —— 补丁 ② |
| | `process` / `http` 资源 | await（stream / future） |
| | `ws.receive` 与 `ws.poll` | 两种都给 —— 补丁 ③ |
| | ambient WASI（fs/env/net） | 直连，不经契约 |

**三个补丁是两个根因，不是一个**（这点比初稿更准）：

1. **宿主推给 guest 没有异步入口** → 补丁 ② ③。入站泵跑在同步 probe 里，而同步降低的导出
   无法 await 流读取。这一类只要有一个异步入口就消失，**不需要翻任何案**。
2. **决策本身需要 guest→host 的 I/O** → 补丁 ①。只有把 probe 变 async 才能删掉它，也就是
   必须翻 `docs/wit-redesign.md` §7「不做的事：探针异步化」。

补充事实：契约已经要求 `definitions` / `execute` 是 async，所以「同步 guest」本来就不存在
——C++ 卡住的是 `future` / `stream` 类型，不是 `async func`。因此三个补丁买不到任何语言覆盖，
只在买一条哲学：决策点不做 I/O。

### 三个终局

| | 形状 | 代价 |
|---|---|---|
| A 一条规则 | 宿主调 guest 一律 async + 宿主给每次调用预算；guest 调宿主一律可 await。删 ① ② ③ | 翻 §7 的案；决策点变成「有预算的决策点」 |
| B 一条通道 + 两条规则 | probe 保持同步；宿主推给 guest 的一切收进一个异步入口，删 ② ③，保留 ① | ① 长期留在契约里当例外 |
| C 保持现状 | 三个补丁都留 | 每加一个 host 能力就要再配一个孪生 |

### 第二意见（Jev，第四、五轮）

第四轮（A / B / C）：

| 问题 | 结论 | 概率 / 置信 |
|---|---|---|
| 终局形态 | `B_dispatch` 0.60 · `A_one_rule` 0.34 · `C_status_quo` 0.02 | 0.46 ⚠ |
| probe 变 async 被滥用的风险 | p = 0.50（正好掷硬币） | —— |
| 现状的税会反复付 | p = 0.85 | —— |
| 约定按什么定 | `by_role` 0.64 · `by_waiting` 0.28 | 0.53 ⚠ |

第五轮（形状与落点）：

| 问题 | 结论 | 概率 / 置信 |
|---|---|---|
| 入口形状 | `resource_handlers` 0.50 · `both` 0.49 · `single_dispatch` 0.01 | 0.34 ⚠ 平局 |
| 静态声明兴趣够不够 | p = 0.74（不够） | —— |
| dispatch 是否也给扩展 world | p = 0.75（要给） | —— |
| 落哪个版本 | `in_090` 0.68 · `in_080` 0.25 | 0.52 ⚠ |

读法：C 死了（0.02），不统一的税被判为会持续付（0.85）；A 与 B 之间只是倾向，反对 A 的唯一
理由被判成 0.5，不是决定性证据。第五轮的平局本身就是信息——两个领先选项都以**资源注册**为
中心，纯变体 dispatch 只有 0.01，配上「静态声明不够」（0.74）。

### 选定形状（待 owner 确认）

资源当处理器 + 运行时注册，与 `ingress.listen` 同形：

```wit
// guest 导出
interface callbacks {
    resource handler { on-event: async func(e: event) -> reply }
}
// host import
register: func(h: own<handler>, interests: list<topic>) -> result<registration, error>
```

`own<handler>` 交出去、`registration` 拿回来；丢弃 `registration` 即注销；实例 trap 时该实例的
注册全部作废（0.7 已确立的所有权语义）。dispatch 的变体入口作为简化路径保留（`both` 0.49）。
两个 world 都要给（0.75）——`examples/streamer` 这类普通扩展也在用推送类能力，只给桥等于把
拉形态为扩展留着。

**三件配套约束**（不定这三件，形状不能落——它们正是 `host.subscribe` 当初做成拉形态的原因）：

1. 宿主持有**有界队列**；
2. 溢出时的**丢弃语义**（`lagged(n)` 挪进事件里，而不是留在拉接口上）；
3. **每次回调的预算**（超时即具名拒绝）。

### WIT 语法事实（wasm-tools 1.259.0 实测）

- **没有函数类型**：`type callback = func(x: u32) -> u32;` 会直接报错——
  `expected a type, found keyword func`。没有闭包、没有一等函数、不能在运行时注册匿名函数。
- 回调的身份只能是**导出名**或**资源方法**；资源参数默认 `own`（规范化输出把 `own<handler>`
  打成 `handler`），`borrow<>` 只在同一调用内借用。
- `future<T>` 可作参数（一次性承诺）；`stream<T>` 作参数是既有用法。
- 语法坑：world 里的内联接口 `import x: interface { ... }` 后面不能写分号。

### 落点：已裁定 0.9.0

Jev 选 0.9.0（0.68 / 0.52）。本文原先倾向 0.8.0，理由是「访客反正要为 0.8.0 重建，推迟等于
重建两次」——**该理由已撤回**：0.7.0 才发布一天，外面还没有第三方组件，重建两次的成本约等于
零，这条只在生态存在时才成立。

**owner 裁定（2026-10-01）：机制落 0.9.0。** 0.8.0 保持「只删不改」的干净身份（好评审、好
回退），本节的形状连同 0.9 四个原语单独成一版、单独浸泡。前提是 0.8.0 的文档必须给
`host.subscribe` / `ws.poll` / `bridge-io.turn` 标上「0.9.0 待改」，免得有人照着建东西。

### A 的重开条件

出现第 4 个补丁，或者 0.9 四个原语里有两个被迫配拉形态孪生，则翻 `wit-redesign.md` §7 的案
改走 A。

## 7. 待裁定（owner）

### 已裁定（2026-10-01）

- **ABI 兼容政策 = P2**（0.x 允许破坏，1.0.0 起向后兼容；不做多版本 shim）——见 §2 裁定 4；
- **调用约定统一落 0.9.0**（0.8.0 只删不改）——见 §2 裁定 5 与 §6.5；
- 接受 0.8.0 让 1.0.0 的浸泡期按已裁定规则（wit-redesign.md §9 条件 3）归零重计。

### 仍待裁定

1. ~~**对冲形态**：独立 WIT 包 / 宿主侧进程协议 / 只写回来条件~~ ——**按默认执行，不再阻塞本轮**：
   裁定 5 已定 0.8.0 只删不改，对冲天然落不到 0.8.0；Jev 第二轮给过 `side_contract` 0.86/0.81、
   第三轮形态只有 0.58/0.44（低于门限）。默认 = 本轮不建对冲包；回来条件已写进契约头（长尾
   厂商 / 自建网关被证明覆盖不了时，用**另一个包**）；重开时按实测在「独立 WIT 包」与「宿主
   侧进程协议」之间选。它是 0.9.0 的输入项，不是本轮的阻塞项；
2. **中文本是否进门禁**：`validate.sh` 加一条中英 parity 腿（脚本见第 9 节）；
3. **0.9 四个原语的排序**（定时 / UI / 存储 / 凭据）；
4. **资源处理器的三件配套约束**（有界队列 / 丢弃语义 / 每次回调预算）确认；
5. **A 方案（探针异步化）的重开条件**确认（§6.5 末）。

## 8. 落地清单（评审通过后）

- [x] 替换契约本体两份（`wit/tau.wit` + `crates/tau-ext/wit/tau.wit`），`CONTRACT_VERSION` → 0.8.0
- [x] 删 tau-ext / tau-cli 的 provider / realtime 面，删 `--provider-wasm` / `--provider-origin` /
      `--provider-auth` / `--deny-wasi` / `--allow-inject` / `--remember` / `tau consent`
- [x] 删示例 `echo-provider` / `http-provider` / `realtime-echo`（连带 `scripts/av_wasm_live_e2e.py`）
- [x] 同步文档：16 份 guides + README + CHANGELOG（连 `tutorial` / `im-channels` / `signing` /
      `builtin-tools` / `acp` / `acp-design-zh` / `host-channel` / `oci` / `tool-media` / `release` /
      `wasip3-streams` 都扫过一遍）
- [x] `scripts/validate.sh`：删 step 4 / 4b（wasm provider）与 step 6（remembered consent）、
      step 11d（wasm realtime），去掉 `--deny-wasi` / `--allow-inject` 腿，三处示例清单同步
- [x] 中文本移到 `wit/tau.zh.wit`（与契约本体同目录；`wit/next/` 已下线）
- [x] 0.8.0 只做减法；文档已给 `host.subscribe` / `ws.poll` / `bridge-io.turn` 标「0.9.0 待改」
- [ ] 1.0.0 起执行 P2 兼容承诺（新增接口 / 导出可以；给宿主→guest 的 variant 加臂不行）
- [ ] 0.9.0（另开一版）：两个 world 加 `callbacks` 注册协议 + dispatch 路径，删 `ws.poll` 与
      `host.subscribe` 拉形态（`bridge-io.turn` 保留），落地三件配套约束，与四个原语一起排

## 9. 附：中英 parity 检查

```bash
#!/usr/bin/env bash
# scripts/wit_zh_parity.sh —— 剥掉注释后，中英两份声明必须逐行相同
set -euo pipefail
cd "$(dirname "$0")/.."
strip() { grep -v "^[[:space:]]*//" "$1" | grep -v "^[[:space:]]*$"; }
if diff -u <(strip wit/tau.wit) <(strip wit/tau.zh.wit); then
    echo "OK: 声明一致（注释之外逐行相同）"
else
    echo "FAIL: 中英两版声明漂移——中文版只能改注释" >&2
    exit 1
fi
```

英文版是唯一权威；中文本是阅读副本。这条门禁的作用是让「翻译」永远不会变成第二个真相。

已在 `C:\Program Files\Git\bin\bash.exe` 下实跑验证（`PARITY_OK`）；本机 `bash` 命令指向 WSL 且
未安装发行版，`scripts/validate.sh` 一向也是靠 Git Bash / MSYS 跑的。
