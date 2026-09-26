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
   `~/.tau/trust` 同级约定；暂无 GC。
3. **物化(请求边缘)**:agent 在每次模型请求前把 `Blob` 解析回 `Bytes`——
   模型契约保持 bytes-only,provider wire 编码器永远见不到 hash。blob 文件
   缺失时降级为文本占位符(`[media unavailable: …]`),run 不失败。

一个附带收益：probe 负载走 serde JSON，外置后 `TransformContext`/`BeforeRequest`
的 payload 里只有 hash 而不是几 MB 的 base64。
