---
title: 'Task 与异步调用'
description: '启动任务、await、错误与生命周期。'
kuVersion: '0.0.18'
verifiedAt: '2026-10-21'
sources: ['docs/concurrency.md', 'docs/semantics.md']
sidebar:
  order: 1
---

Task 是异步函数调用返回的一次性句柄。调用启动任务，`await` 获取其结果；`T!` 的错误仍用 `?` 或 `try/catch` 处理。

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

保存为 `tasks.ku`，运行 `ku run tasks.ku`，输出 `40` 和 `42`。该例也在当前受限 native Task 范围内。

## 所有权与结束

Task 只能 move，不能 `.clone()` 或重复 await。所属作用域退出时，仍持有且未完成的子任务收到取消请求；取消会尝试执行 `finally`，但受共享的一秒清理期限约束。取消和超时不是普通 Result，不能被 `catch` 当作业务错误处理。

`&` 只能用于同步调用，异步函数不能声明借用参数。异步任务需要自行拥有传入的数据。

## 等待与后端

[time.sleep](/zh-cn/docs/stdlib/time/) 以及已接入的文件操作使用其对应执行路径。完整 M:N、通用网络可挂起调度与任意动态堆组合仍未实现；不要把这里的算术例子当作全部库都可异步调用。数据库和 `net` 也不能据此传入任意 native Task。

程序级 worker 管理由 `ku supervise` 提供，与 Task 句柄不同，见[支持与进程限制](/zh-cn/docs/support/)。
