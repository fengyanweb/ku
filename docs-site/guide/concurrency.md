# 并发

Ku 的业务并发模型是：

```ku
task = AsyncFunc()
result = await task?
```

用户不能手动 `task.spawn`、`Task.new`、`runtime.schedule` 或 `thread.spawn`。`std.task` 只提供 runtime 诊断和压力测试能力。

解释器与 native HTTP 已由单 I/O 线程推进 socket 读写及 handler 等待，等待读写不创建逐连接线程。两端默认复用 HTTP/1.1 连接；HTTP/1.0 仍拒绝。每连接仅执行一个 handler，最多保留一次 socket 读取产生的 8192 字节后续请求，按顺序处理流水线；显式 Connection: close（大小写无关的完整 token）、错误/超时及 1xx 响应结束连接。已有 pending 时，纯空闲复用连接让出活动槽，响应边界也可主动关闭连接，客户端须处理服务端关闭。各请求沿用原 header/body/handler/write 绝对期限，复用空闲等待使用 idle_timeout_ms，不增加 handler worker。

`max_buffer_bytes` 默认 67108864（64 MiB），必须为正整数，最大 1073741824（1 GiB）。它限制每个 HTTP listener 的 transport 保留额度，覆盖受控 header/body/carry、请求与响应字符串缓冲及保守的临时预占；额度不足返回 503 并关闭连接。该额度不等于整体 Ku 堆、allocator 元数据、容器桶、线程栈、内核 socket 缓冲、外部库内存或进程 RSS 的上限，也不提供公开 stats 方法。

受限 native Task 的文件 read/write、exists 与 read_dir 已接入固定后台池；read_dir 沿用排序、10000 项和路径文本合计1000000字节的上限，分配与取消释放计入原 Task 预算。数组字面量仍最多64个元素，运行期目录数组可承载10000项。通用 Task 网络等待、完整可挂起 M:N 调度、跨模块整体资源预算与正式长期负载/Go 对照验收仍未完成；不以本页的已接入能力推导为当前提交已通过验证。

## HTTP 千万请求压测 demo

```powershell
ku run examples\http_capacity_10m.ku
```

另开终端：

```powershell
powershell -ExecutionPolicy Bypass -File examples\http_bench.ps1 -Url http://127.0.0.1:8080/health -Requests 10000000 -Concurrency 1000 -TimeoutSeconds 600
```

该 demo 是普通开发者视角：代码只写 HTTP handler 并返回 `http.text/json(...)`，不导入 `std.task`，不手动创建或调度 task。

这不是 10,000,000 个活跃 HTTP 连接同时常驻的承诺。真实千万连接压测需要多机客户端、操作系统参数、网络和服务端部署一起设计。

完整说明见仓库 `docs/concurrency.md`。
