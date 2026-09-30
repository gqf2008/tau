# REPL 与 pi 的命令面对齐

owner 裁定（2026-10-01，协作层 thread `repl-pi-alignment`）：tau 的交互式 REPL
与 pi（earendil-works/pi）**在命令面上对齐**——命令名称、语义、/help 分组、
斜杠补全。本文是对照表与口径。参照基线：pi @ `955cc666`
（packages/coding-agent/docs/slash-commands.md，2026-10-01 读取）。

不对齐的两件事（裁定的一部分，不是遗漏）：

- **全屏 TUI**。tau 的交互层是行式 REPL（rustyline + external printer），
  不做 alternate-screen 界面；对齐的是命令面，不是界面形态。
- **`/share` 与 `/bug`**。两者依赖 pi 的外部服务（会话上传、私有 bug
  报告通道），tau 没有也没有计划引入这类服务，声明永不对齐。

## 命令对照

| pi | tau | 说明 |
|---|---|---|
| `/new` | `/new` | 新会话文件（`session-<id>.jsonl`），旧文件留盘，可 `/import` 回去 |
| `/resume` | —（T2） | 需多会话管理，后续立卡 |
| `/name` | —（T2） | 会话显示名需 entry 字段，后续立卡 |
| `/session` | `/session` | 会话文件、条目数、head、模型 |
| `/tree` | `/tree` | 打印会话树，head 标记（与 `tau tree` 子命令同形） |
| `/fork` | `/fork [#index\|id-prefix]` | 在更早条目处分叉（tau 早已有） |
| `/clone` | `/clone` | 复制会话文件并在副本上继续 |
| `/compact [instructions]` | `/compact [instructions]` | instructions 追加进摘要请求 |
| `/import <path>` | `/import <path>` | 打开另一个会话 JSONL 并就地继续 |
| `/copy` | —（T2） | 剪贴板依赖，后续立卡 |
| `/export [path]` | `/export [path]` | 写出会话 JSONL；HTML 导出归 T2 |
| `/settings` 等模型族 | —（T3） | 交互式模型管理，各自立卡 |
| `/trust` | `tau trust` 子命令 | tau 的信任对象是组件签名公钥，语义不同，不进 REPL |
| `/reload` | —（T2） | 重载 skills/组件，后续立卡 |
| `/hotkeys` | `/hotkeys` | 键位说明（tau 键位固定，无 keybindings.json 体系——T3） |
| `/changelog` | `/changelog` | 就近找 CHANGELOG.md 显示前两节；找不到给仓库位置 |
| `/quit` | `/quit`（`/exit` 保留为别名） | |
| 命令菜单（`/` 触发） | Tab 补全 | 行式 REPL 的等价物：输入 `/` 按 Tab 列出候选 |
| 扩展注册命令 | tau extensions | `/mic`、`/live`（realtime-av 私有），按 pi 扩展命令惯例保留 |

mid-run 语义（`!text` 转向、纯文本排队 follow-up、Ctrl-C abort）在契约层
早已与 pi 的控制通道对齐（docs/architecture.md §1），不属于本卡。

## 已知简化

- `/new` 切换会话文件，但不重发 `SessionStart` 探针——会话级探针按 REPL
  进程边界计，不按文件切换计。
- `/clone` 与 `/new` 的新文件名取会话条目同源的随机 id 前 8 位，不带
  时间戳；按 id 排序即按创建序。
- `/changelog` 的已安装二进制不带 CHANGELOG.md（crates.io 包不含它），
  只在源码树/发布包邻旁能找到文件时显示内容。
