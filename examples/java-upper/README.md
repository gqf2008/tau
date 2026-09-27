# java-upper — 今天无可用路径（如实降级）

本目录故意不含代码：截至 2026-09，没有把 Java 编译为自定义 world
wasm 组件的维护中工具链，因此不存在可对齐 `examples/upper` 的最小示例。
不伪造成功、不以「能编译」冒充「能加载」。

权威依据（两条，详见 `docs/wasm-languages.md` 的 Java 一节）：

1. `wit-bindgen --help`（0.62.0）的子命令列表为 markdown / moonbit /
   rust / c / cpp / go / csharp / d —— 没有 java 生成器（历史上的
   teavm-java 生成器早已从上游移除）。
2. wasmCloud 语言支持矩阵
   （https://wasmcloud.com/docs/wash/developer-guide/language-support/）
   把 Java 列在 Tier 3「in progress or planned」；其条目指向的
   GraalWasm 是「在 JVM 里运行 wasm」的运行时，不是 Java→wasm 编译器；
   绑定生成器一栏为 `wit-bindgen-java, early stage`。

若未来出现可用工具链，补齐方式与其他六语言一致：
`tau --allow-unsigned -e target/java_upper.wasm --demo \
  -p "shout hello using the upper tool"`
的 transcript 必须出现 `tool → upper` 与
`tool ← upper: SHOUT HELLO USING THE UPPER TOOL`。
