# SDK 的 tiny_http 0.12.0 窄补丁

来源：crates.io `tiny_http` **0.12.0** 发布源码（上游仓库
https://github.com/tiny-http/tiny-http）。保留 `src/`、原始 `Cargo.toml.orig` 对应的
Cargo.toml、README 及原 MIT/Apache-2.0 许可证；未复制示例、基准和外部测试目录。
依赖由 SDK 根 Cargo.toml 的本地 path patch 和 Cargo.lock 固定，不跟踪移动分支。

原因：上游 Request 的 Drop/into_writer 会析构 EqualReader；未读完 Content-Length
时析构同步读取剩余请求体。慢上传可以占满响应线程，并令拒绝超载请求的 owner 陷入读取。
公开 API 没有 socket abort。只限制线程数不能解决这一生命周期问题。

仅修改：

- `src/util/refined_tcp_stream.rs`：提供只供库内部使用的真实 socket 克隆控制引用。
- `src/client.rs`：每条连接持有同一关闭标志；给每个真实请求附加控制引用。
- `src/request.rs`：新增 `RequestAbortHandle`、`abort_handle()`、`abort()` 与
  `respond_and_close()`。abort 先 shutdown(Both)，再释放响应 writer 与 body；
  相同连接的流水线请求共享关闭标志。完成标志在 body 清理之后发布，截止时间仍能唤醒
  正在清理的慢请求。普通 `respond()`/`into_writer()` 保持原行为。
- `src/response.rs`：内部显式 Connection: close 头，只由 respond_and_close 使用。
- `src/lib.rs`：导出 RequestAbortHandle。

SDK 用 respond_and_close 完成单次 HTTP 往返；不支持同一连接的流水线请求。
响应最多 16 个并发线程，满载先 abort 实际 socket。服务 owner 保存最多 64 个请求
控制引用，按单调时钟 10000 ms 截止关闭慢上传/慢下载；网络等待不持有设备 Client。
截止仅在 owner 可调度时处理；P1/P2 的同步 SDK 握手/查询仍是已声明的 P4 前限制。
完成或过期引用及时移除；没有泄漏 Request、额外清理线程、代理或第二套 HTTP 解析器。

第三方代码保留上游风格和数值类型，不适用自有 Rust 业务 binary32 扫描；SDK 原扫描范围
本就只包含 src/crates/targets/examples/tests，因此无需放宽检查脚本。
对应真实 loopback 回归见 `crates/tool/src/service/http_io.rs`。

## 入口资源限制与析构补修（2026-10-14）

本次仍基于 tiny_http **0.12.0**，不修改版本、上游许可、默认 `Server::http/new/from_listener`
的 keep-alive/pipeline 行为。SDK 单独使用新增 `Server::http_bounded`：

- `bounded.rs` 在 accept 前预留最多 64 个连接许可；每个许可跟随真实 socket 读写句柄，
  parser 返回 Request 后仍不释放。许可满时停止 accept，未 accept 的连接留在操作系统有限 backlog。
  不为超额连接创建 parser/task/socket 控制对象。
- 最多 64 个 Request 的队列是显式硬上限，超额或服务已关闭时先 socket shutdown，再析构 Request。
- 每个连接最多一个请求，不解析下一条 keep-alive/pipeline；响应使用 `Connection: close`。
- 首部（含请求行）累计最多 16 KiB；从 accept 起的 10000 ms 绝对连接寿命由独立的有限监控任务
  执行真实 socket shutdown，字节到达不会续期。它同时覆盖首部解析、小 body 预读、owner 排队、
  body 读取和响应写入；原 owner 的 64 个 Request 控制引用只作为第二层保障。
- 非阻塞 listener 每 10 ms 检查关闭；服务 Drop 会停止许可等待、关闭当前连接、关闭并清空队列。
  不依赖可能在 backlog 满时阻塞的自连接唤醒。默认上游关闭行为不变。
- `client.rs` 限制首部累计字节，bounded Request 放弃时先关闭 socket；不支持 HTTP 版本在原 Request
  上响应 505，避免第二个顺序 writer 等待第一个未析构 writer。
- `util/equal_reader.rs` 保留上游 drain 语义，但使用固定 8192 字节缓冲，任何 Content-Length 都不会
  让 Drop 按剩余长度分配内存。shutdown 后巨大 Content-Length 的 abort 不再容量溢出。

SDK `service/http_io.rs` 的响应包装器由闭包整体捕获。即使线程创建失败，闭包析构也会先 abort
真实 socket，再清理未读 body；不能等 `spawn(...).map_err(...)` 才处理。ready stdout 写入失败
也不再 panic 或退出已经监听并发布发现记录的服务。连接限额与 deadline 不改变设备会话、控制
请求身份或业务错误合同。

### Chunked 元数据界限

独立复审发现 chunked_transfer 1.5.0 的 chunk size 行可在产生任何实体字节前增长 Vec；实体
JSON 上限不能限制这条分配路径。新增 `ServerLimits.max_body_wire_bytes`，SDK 设置为 **8 MiB**，
只对 chunked body 的原始编码字节生效（含 size 行、分隔符、extensions 和实体）。Request 创建
时将限额传到 decoder 前的 `WireBudgetReader`；非空读取时预算耗尽明确返回 `InvalidData`，
不能将上限转换成普通 EOF。完整终止 chunk 恰好到上限仍由现有 decoder 正常结束。
Content-Length 与实体 JSON 上限保持原处理路径。默认上游 `new_request` 接口签名不变、不设
该原始编码限额，不引入第二个 parser 或新的 vendor 依赖。

8 MiB 可容纳 SDK legacy 原有 1 MiB 实体上限的逐字节 chunk 编码（约 6 MiB），v2 实体上限
仍为 64 KiB。额外长扩展/size 行可能先触及原始编码限额并失败，调用者不得自动重发动作。
