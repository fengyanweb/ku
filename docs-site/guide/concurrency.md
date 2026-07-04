# 并发

Ku 的业务并发模型是：

```ku
task = AsyncFunc()
result = await task?
```

用户不能手动 `task.spawn`、`Task.new`、`runtime.schedule` 或 `thread.spawn`。`std.task` 只提供 runtime 诊断和压力测试能力。

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
