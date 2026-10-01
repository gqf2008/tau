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
| `/new` | `/new` | 新会话文件（`session-<id>.jsonl`），旧文件留盘，可 `/resume` 或 `/import` 回去 |
| `/resume` | `/resume [#n\|name]` | 列出当前目录的会话（序号、修改时间、条目数、head 摘要），带参切换；行式列表代替 pi 的选择器 |
| `/name` | `/name <name>` | 显示名由会话文件名携带（净化后 `<name>.jsonl`），不动 JSONL 格式——见下「/name 存储裁定」 |
| `/session` | `/session` | 会话文件、条目数、head、模型 |
| `/tree` | `/tree` | 打印会话树，head 标记（与 `tau tree` 子命令同形） |
| `/fork` | `/fork [#index\|id-prefix]` | 在更早条目处分叉（tau 早已有） |
| `/clone` | `/clone` | 复制会话文件并在副本上继续 |
| `/compact [instructions]` | `/compact [instructions]` | instructions 追加进摘要请求 |
| `/import <path>` | `/import <path>` | 打开另一个会话 JSONL 并就地继续 |
| `/copy` | `/copy` | 最近一条 assistant 消息进系统剪贴板（平台工具：clip / pbcopy / xclip / xsel，不引入新依赖） |
| `/export [path]` | `/export [path]` | 写出会话；路径以 .html/.htm 结尾渲染自包含 HTML，否则 JSONL |
| `/settings` 等模型族 | —（T3） | 交互式模型管理，各自立卡 |
| `/trust` | `tau trust` 子命令 | tau 的信任对象是组件签名公钥，语义不同，不进 REPL |
| `/reload` | `/reload` | 按启动旗标重建 harness 并热换 agent（重接 host 通道）；live 会话中拒绝 |
| `/hotkeys` | `/hotkeys` | 键位说明（tau 键位固定，无 keybindings.json 体系——T3） |
| `/changelog` | `/changelog` | 就近找 CHANGELOG.md 显示前两节；找不到给仓库位置 |
| `/quit` | `/quit`（`/exit` 保留为别名） | |
| 命令菜单（`/` 触发） | Tab 补全 | 行式 REPL 的等价物：输入 `/` 按 Tab 列出候选 |
| 扩展注册命令 | tau extensions | `/mic`、`/live`（realtime-av 私有），按 pi 扩展命令惯例保留 |

mid-run 语义（`!text` 转向、纯文本排队 follow-up、Ctrl-C abort）在契约层
早已与 pi 的控制通道对齐（docs/architecture.md §1），不属于本卡。

## /name 存储裁定（thread repl-pi-alignment-t2）

显示名**由文件名携带**，不加 entry kind。理由：契约冻结浸泡期内不动
session JSONL 格式——`EntryKind` 是封闭枚举，新增 kind 会让 0.7.0 二进制
把命名过的会话读成 corrupt，downgrade 即拒；`JsonlStore::append` 每次新开
句柄，改名安全；pi 语义（/resume 里看到名字）由文件名满足。净化规则：
空白与 `<>:"/\\|?*` 及控制字符折叠为 `-`，Windows 保留名加 `session-`
前缀，上限 40 字符。

## 已知简化

- `/new` 切换会话文件，但不重发 `SessionStart` 探针——会话级探针按 REPL
  进程边界计，不按文件切换计。
- `/resume` 的列表按修改时间倒序；当前会话若尚未落盘（首次 append 才建
  文件）以内存态列在首位。
- `/reload` 重建整个 harness（含模型与 skills 发现），不只是扩展；不重发
  `SessionStart`；consent 提示行为与启动时相同。
- `/copy` 依赖平台剪贴板工具在 PATH 上；缺失时报出尝试过的工具名。
- `/clone` 与 `/new` 的新文件名取会话条目同源的随机 id 前 8 位，不带
  时间戳；按 id 排序即按创建序。
- `/changelog` 的已安装二进制不带 CHANGELOG.md（crates.io 包不含它），
  只在源码树/发布包邻旁能找到文件时显示内容。
