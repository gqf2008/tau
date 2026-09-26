# OCI 组件分发

tau 的扩展就是 wasm 组件，OCI registry 就是它们的分发渠道：任何能推 OCI artifact 的
工具（`oras`、`crane`、docker 的 OCI 布局导出）都能发布 tau 扩展，tau 只负责**拉**。

## 引用形式

```
oci://ghcr.io/org/upper:0.1.0        # 可变 tag
oci://ghcr.io/org/upper@sha256:…     # 不可变 digest（可复现加载的推荐形式）
oci://127.0.0.1:5000/test/comp       # 省略 tag = :latest（loopback 走 http）
```

任何接受组件路径的 CLI 参数（`-e/--extension`、`--provider-wasm`、`--mcp-bridge`）
都接受 `oci://` 引用；其余参数一律按本地路径处理。

## 拉取链路（pull-only）

1. `GET /v2/<repo>/manifests/<ref>`（Accept: OCI + docker manifest 类型）。
2. 401 → 解析 `WWW-Authenticate: Bearer realm=…,service=…,scope=…`，匿名换 token，重试。
3. 从 manifest 选 wasm layer（mediaType 含 `wasm` 者优先，否则取唯一 layer）。
4. `GET /v2/<repo>/blobs/<digest>`，**先校验 sha256 再落盘**。
5. 内容寻址缓存：`~/.tau/oci/blobs/<digest（: → _）>`。命中缓存则跳过 blob 下载
   （manifest 每次都重新取，可变 tag 才能看到新 digest）。

推送（push）不在 tau 的职责内——那是 `oras push` / `crane push` 的工作。
loopback registry（127.0.0.1 / localhost / [::1]）允许明文 http，其余一律 https。

## 与签名/信任/授权的关系

拉回来的字节走的是**和本地文件完全相同**的加载路径：签名校验、信任策略
（默认 RequireTrusted，`--allow-unsigned` 逃逸）、按指纹的记忆授权全部原样生效。
OCI 解决「组件从哪来」，签名解决「组件是谁的」，两者正交。

tag 是可变的：用 tag 引用时 CLI 会打印解析出的 digest，并提示用
`@sha256:…` 固定以获得可复现加载。
