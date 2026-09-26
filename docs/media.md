# 大媒体外置（blob store)

`MediaSource::Bytes` 在 session JSONL 里内联为 base64——小图片没问题，但一张 10MB
照片会让单行 ~13MB,而且每轮请求都跟着历史重读。超过阈值（`INLINE_LIMIT`,256KB)
的媒体在**写 session 时**外置：

```
~/.tau/blobs/sha256_<hex>        # 内容寻址，跨 session 去重
session.jsonl: {"source":"blob","hash":"sha256:<hex>"}   # 只留引用
```

三个环节各管一段：

1. **外置(写路径)**:`JsonlStore::with_blobs(store)` 后，`append` 自动把
   `Bytes > INLINE_LIMIT` 改写成 `Blob{hash}`。小媒体保持内联，已外置的幂等。
2. **存储**:`tau_core::blobs::BlobStore` 内容寻址 put/get，与 `~/.tau/oci`、
   `~/.tau/trust` 同级约定;GC 见下。
3. **物化(请求边缘)**:agent 在每次模型请求前把 `Blob` 解析回 `Bytes`——
   模型契约保持 bytes-only,provider wire 编码器永远见不到 hash。blob 文件
   缺失时降级为文本占位符(`[media unavailable: …]`),run 不失败。

一个附带收益：probe 负载走 serde JSON，外置后 `TransformContext`/`BeforeRequest`
的 payload 里只有 hash 而不是几 MB 的 base64。

## GC(mark-and-sweep)

blob 只增不减会磁盘膨胀。`tau gc` 做标记-清扫：

```
tau gc                          # dry run:报告会删什么(默认扫 .tau/session.jsonl)
tau gc --session a.jsonl --session b.jsonl   # 多个 session 的活引用取并集
tau gc --yes                    # 实际删除
```

- **标记**:`JsonlStore::live_blob_hashes()` 收集**整棵 session 树**引用的
  hash(不只 active branch——旧分支仍走原始消息，compaction 摘要里的引用也算)。
- **清扫**:`BlobStore::sweep(live, dry_run)` 删掉不在标记集里的 blob;
  文件名无法解码回 hash 的一律不碰。
- 安全网有两层：默认 dry run,`--yes` 才真删；真误删也不致命——materialize
  会把缺失 blob 降级为文本占位符，run 继续。但仍建议把还在用的 session 全部
  用 `--session` 列上(只被未列出 session 引用的 blob 会被收掉)。
