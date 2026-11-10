---
title: 'Task 与异步调用'
description: '启动任务、await、错误与生命周期。'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources: ['docs/concurrency.md', 'docs/semantics.md']
sidebar:
  order: 1
slug: zh-cn/v0.0.18/docs/guide/concurrency
---

Task 是异步函数调用返回的一次性句柄。调用启动任务，`await` 获取结果；`T!` 中的可恢复错误继续使用 `?` 或 `try/catch` 处理。

## 启动与等待

保存为 `tasks.ku`，运行 `ku run tasks.ku`：

```ku
async fn Double(value: int): int! {
    return ok(value * 2)
}

async fn main(): null! {
    first = Double(20)
    second = Double(21)
    println((await first)?)
    println((await second)?)
    return ok(null)
}
```

预期依次输出 `40` 和 `42`。这个程序也属于 0.0.18 的受限 native Task 子集，可用 `ku build --backend c tasks.ku -o tasks` 构建；Windows 输出名使用 `tasks.exe`。它不是任意标准库都可异步调用的例子。

## 数据如何转移

Owned 参数传入异步函数时转移所有权，需要保留来源时显式 clone。Task 只能 move，不能 clone，`await` 消费句柄一次。`&name: T` 只用于同步借用参数，异步函数不能声明借用参数；不能把借用保留到 await 之后。普通业务没有 `task.spawn`、`Task.new`、`thread.spawn` 或 detach 入口。

## 哪些操作可以等待

下表对应 **0.0.18**。默认 runner 使用解释器路径，native Task 是独立的有界 typed 子集。

| 操作                       | 解释器 / 默认 runner                    | 受限 native Task                                                     |
| -------------------------- | --------------------------------------- | -------------------------------------------------------------------- |
| async 调用与 await         | 支持一次性 Task                         | 已列出的 primitive、字符串、平面数组、单层 Result 和函数值组合       |
| 整数毫秒 `time.sleep(ms)?` | 通过 blocking worker 等待               | driver timer 等待；不占文件后台 worker                               |
| 文件与目录                 | 对应解释器操作；不承诺全部操作都可挂起  | `read/try_read/write/try_write/exists/read_dir` 使用固定后台池       |
| TCP / TLS client           | 不提供对应 native client                | 局部 `net.client/read/write/close` 使用固定后台池；TLS 需要匹配 pack |
| HTTP                       | 独立 HTTP I/O 线程，Ku handler 串行执行 | 没有通用 Task HTTP 接口；同步 native 服务端是另一条路径              |
| 数据库                     | 不支持连库                              | 不支持通用异步数据库调用；同步 native client 不能据此变成 async I/O  |
| JSON / 动态值              | 解释器支持 JSON                         | JSON、任意动态堆组合和 Task 容器不在该子集内                         |

文件、TCP 和 TLS 已进入系统调用或外部库的操作不能被强杀。后台池在操作实际完成并清理迟到结果后才归还资源。native Task 的连接和 bytes 只能由当前帧局部拥有，不能作为 Task 参数、返回值或容器元素。

## 调度与执行

解释器和 native Task 使用有界工作线程与队列。HTTP 使用单个 I/O owner 和独立的 handler 执行路径；并非任意库调用都能挂起。

## 取消与清理

拥有者作用域退出时，仍持有且未完成的子任务收到取消请求。合法 move 同时转移清理责任；已完成但未 await 的结果也会释放。取消尝试执行 finally，嵌套清理共享默认一秒的绝对截止期限；预算到期不保证执行剩余用户语句。

取消和超时是控制终止，不是普通 Result，不能由 catch 作为业务错误捕获。取消清理期间不能新建 Task、await，或提交新的 timer、网络等待和 blocking job。已经开始的系统调用不能靠 Task 取消证明其副作用已撤销。

`ku supervise` 管理整个程序 worker，与 Task 句柄不同，见[支持与进程限制](/zh-cn/v0.0.18/docs/support/)。详细操作范围见 [并发说明](https://github.com/fengyanweb/ku/blob/v0.0.18-r1/docs/concurrency.md)。
