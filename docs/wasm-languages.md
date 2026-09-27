# 多语言 wasm 扩展实测矩阵

`docs/architecture.md` 声称「语言中立：任何能编到 wasm32-wasip2 的语言都能
写扩展」。本文档是该声称的实证：以 `examples/upper`（Rust）为唯一对齐目标
——工具名 `upper`，把入参 `text` 转大写——用七种主流语言各写一个最小
扩展，并用同一条验收命令**真加载**验证。

契约版本 `tau:extension@0.2.0`（0.1.0 → 0.2.0：`hooks` 接口正名
`probes`，extension world 新增 `host` import）。0.1.0 契约的旧产物
会被宿主点名拒载（版本错配写进 load 错误），重建即迁移。

## 验收标准

只「能编译」不算数，必须真加载：

```bash
tau --allow-unsigned -e examples/<lang>-upper/target/<file>.wasm \
    --demo -p "shout hello using the upper tool"
```

transcript 必须同时出现：

```
[tau] tool → upper
[tau] tool ← upper: SHOUT HELLO USING THE UPPER TOOL
```

## 矩阵

| 语言 | 工具链 / 版本 | 产物 | 构建命令 | tau 加载 | 断点或依据 |
|---|---|---|---|---|---|
| C | wit-bindgen 0.62（c）+ clang 22.1.8 + wasm-tools 1.259 | `c_upper.wasm` 7.0 KB | `bash examples/c-upper/build.sh` | ✅ 跑通 | — |
| C++ | wit-bindgen 0.62（cpp）+ clang++ 22.1.8 + wasm-tools 1.259 | `cpp_upper.wasm` 7.3 KB | `bash examples/cpp-upper/build.sh` | ✅ 跑通 | 0.2.0 需 -std=c++23 + 新增 expected/variant 垫片，见下文 |
| Python | componentize-py 0.25.1（pip） | `py_upper.wasm` 18 MB | `bash examples/python-upper/build.sh` | ✅ 跑通 | 实现类命名坑，见下文 |
| JavaScript | jco 1.35.0（npx，node 22.14） | `js_upper.wasm` 12.5 MB | `bash examples/js-upper/build.sh` | ✅ 跑通 | 必须 `--disable http fetch-event` |
| TypeScript | jco 1.35.0（npx，node 22.14） | `ts_upper.wasm` 12.5 MB | `bash examples/ts-upper/build.sh` | ✅ 跑通 | 同上 |
| Go | TinyGo 0.42 + go 1.25.7 + wit-bindgen 0.62（go）+ Binaryen 133 + preview1 reactor adapter 48.0.3 | `go_upper.wasm` 3.3 MB | `bash examples/go-upper/build.sh`（需 env，见下文） | ✅ 跑通 | 四处补丁 + reactor 构建模式，见下文；0.2.0 实现包改名 `export_tau_extension_probes` |
| Java | — | — | — | ❌ 无可用路径 | 权威依据见下文 |

Rust 本体（`examples/upper` 等 5 个既有示例）不在本轮范围内，由
`scripts/validate.sh` 覆盖。

## C —— 零依赖 freestanding 路线

不需要任何 wasi sysroot：clang 直出 freestanding 核心模块
（`--target=wasm32-unknown-unknown -nostdlib -mexec-model=reactor
-Wl,--no-entry`），`src/shim.c` 提供最小 libc（1 MiB arena first-fit
分配器 + mem*/str*），`wit-bindgen c` 生成绑定，`wasm-tools component new`
包装成组件。产物不 import 任何 WASI 接口，7 KB。

## C++ —— 同一路线加最小 std 垫片

复用 C 的 shim.c，`src/cxxshim/` 提供 13 个最小标准库替代头
（cstdint/cstdlib/utility/optional/string/string_view/span/memory/map/
new/assert.h + 0.2.0 新增的 expected/variant），`clang++
-fno-exceptions -fno-rtti -std=c++23`。`wit-bindgen cpp` 生成的绑定
在 `exports::tau::extension` 命名空间下。

0.2.0 的 `host` import 让生成代码引用 `std::expected`（result 返回）
与 `std::variant`（content 四态）——clang 自由standing 工具链没有
libc++，这两头必须手写垫片（只覆盖生成代码实际用到的 API 面；
`optional`/`span` 垫片也要补 `emplace`/`data`/`const value`）。
垫片只服务编译与链接：本示例不调用 host 接口，垫片代码路径不执行。

## Python —— componentize-py 的类命名契约

```bash
componentize-py -d ../../wit/tau.wit -w extension componentize -p src upper -o target/py_upper.wasm
```

**坑（0.25.1 实测）**：运行时按**模块属性名**查找导出接口的实现——
app 模块里必须定义名字恰为 `Tools` / `Hooks` 的类，分别继承
`wit_world.exports.Tools` / `wit_world.exports.Hooks`（mixin 在
`wit_world/exports/__init__.py`，不在 `wit_world.exports.tools`）。
把 mixin 名 import 进模块作用域会让查找抓到抽象基类，报
"Can't instantiate abstract class Tools"。绑定时由 runtime 注入到
`/world/wit_world`，`-p src` 一个路径即可，不需要本地预生成绑定。
产物 18 MB（内嵌 CPython 运行时）。

## JavaScript / TypeScript —— jco（StarlingMonkey）

```bash
npx jco componentize src/upper.js --wit ../../wit/tau.wit --world-name extension \
    --disable http fetch-event -o target/js_upper.wasm
```

WIT→JS 映射：接口 → 命名 ES export（`export const tools = {...}`），
kebab-case → camelCase（`parametersJson`、`isError`），action enum →
字符串 `"continue"`。jco 直接吃 TS（自动打包），二者同一条命令。

**必须 `--disable http fetch-event`**：否则 StarlingMonkey 默认链接
`wasi:http/types@0.2.10`，tau 的 ambient-WASI linker 不提供该接口，
实例化直接失败。产物 12.5 MB（内嵌 SpiderMonkey）。

## Go —— TinyGo core-module 路线（四处补丁 + reactor 构建模式）

工具链准备（各 ≤10 分钟）：TinyGo 0.42 发行包、Binaryen 133（wasm-opt，
TinyGo 外部调用）、reactor adapter 从 cargo registry 缓存的
`wasi-preview1-component-adapter-provider-48.0.3.crate` 解出
（`artefacts/wasi_snapshot_preview1.reactor.wasm`）。

```bash
WASMOPT=<wasm-opt> TINYGO=<tinygo> ADAPTER=<reactor.wasm> bash examples/go-upper/build.sh
```

流程：`wit-bindgen go` 生成绑定（实现包 `export_tau_extension_*` 手写）→
`go mod vendor` → `python patch_tinygo.py` →
`tinygo build -buildmode=c-shared -target=wasi -opt=0` →
`wasm-tools component embed --world extension` →
`wasm-tools component new --adapt wasi_snapshot_preview1=<reactor>`。

`patch_tinygo.py` 修四处 TinyGo 0.42 与 wit-bindgen go 输出的不兼容：

1. `runtime.Pinner` / `runtime.AddCleanup` 在 TinyGo 不存在 → 无操作垫片；
2. `//go:linkname sbrk runtime.sbrk` 在 TinyGo 不存在 → 16 MiB 静态
   bump 分配器；
3. vendored `cabi_realloc` 的 `panic("todo")`（realloc 带旧指针分支）→
   分配-拷贝-返回；
4. 删掉 `init() { useGCAllocations = true }` 与
   `adapter_monotonic_clock_set_paused` 调用：TinyGo 的 command 入口
   **先 initRand 后 initHeap**（`scheduler_none.go:24-25`），而 initRand
   经 wasi-libc `arc4random` → adapter `random_get` → adapter 回头调
   guest `cabi_realloc` 分配栈——此刻堆未初始化，任何 Go 分配必 trap；
   48.0.3 adapter 的 `set_paused` 又会在自身 State 未就绪时 assert。
   因此 cabi_realloc 永远走 bump 路径（每次调用泄漏几 KB，16 MiB 预算，
   最小示例可接受，勿照抄进长会话生产扩展）。

**必须 `-buildmode=c-shared`**（reactor）：它的 `_initialize`
**先 initHeap 后 initRand**（`runtime_wasmentry.go:34-36`），上述
pre-heap 调用因此安全；command 模块（`_start`）顺序相反，且 main 返回后
`proc_exit(0)` 会以 "Exited with i32 exit status 0" 杀死实例化。
asyncify 调度器不可关：`scheduler=none` 下 reactor 入口里的
`go initAll()` 编译期拒绝（"attempted to start a goroutine without a
scheduler"）。另外 TinyGo 的 `-target=wasip2` 不会 lift 自定义导出
（留在核心层），所以走 core-module + embed + adapt 路线。

## Java —— 今天无可用路径（权威依据）

1. **wit-bindgen 0.62.0 没有 java 生成器。** `wit-bindgen --help` 的
   子命令列表：markdown、moonbit、rust、c、cpp、go、csharp、d。
   （历史上曾有的 teavm-java 生成器早已从上游移除。）
2. **wasmCloud 语言支持矩阵**（行业参考，
   <https://wasmcloud.com/docs/wash/developer-guide/language-support/>）
   把 Java 列在 Tier 3「in progress or planned」：条目指向的
   [GraalWasm](https://www.graalvm.org/latest/reference-manual/wasm/)
   是「在 JVM 里运行 wasm」的**运行时**，不是把 Java 编成 wasm 的编译器；
   绑定生成器一栏写的是 `wit-bindgen-java, early stage`。
3. TeaVM 的 wasm 后端仍是实验性质，在其上手工实现 canonical ABI
   （像本仓库 C 示例那样）是研究项目，不是「最小示例」。

结论：截至 2026-09，没有把 Java 编译为自定义 world 组件的维护中工具链。
本矩阵如实记为 ❌，不伪造成功。

## 复现环境

本节版本均为 2026-09-27 实机实测：wit-bindgen-cli 0.62.0、
wasm-tools 1.259.0、clang/clang++ 22.1.8、componentize-py 0.25.1
（0.17.2 亦验证通过，机制相同）、jco 1.35.0 + node v22.14.0、
TinyGo 0.42.0 + go 1.25.7 + Binaryen 133、
wasi-preview1-component-adapter-provider 48.0.3（reactor adapter）。
六条跑通路线的 transcript 尾部一致：

```
[tau] loaded extension: <lang>_upper
[tau]   tool: upper
[tau] tool → upper
[tau] tool ← upper: SHOUT HELLO USING THE UPPER TOOL
```
