# Native Task 调度器定量观察工具（维护者）

`examples/native_task_scheduler_probe.rs` 是内部固定工作量工具，不是 Ku 业务 API，
也不是 v0.0.18 高并发验收通过的声明。它复用验证后的 Task IR、生成的 Frame 和
实际 Driver7，提供一个专用 R2 适配器；不是源码 Start/Await 的端到端语言基准。

## 运行方式

先用仓库 Rust 工具链构建生成器，再生成一次 C：

```text
cargo +1.89.0 build --locked --release --example native_task_scheduler_probe
native_task_scheduler_probe emit probe.c
```

第二行调用上一步 `target/release/examples/` 中的可执行文件；Windows 带 `.exe`。
自定义 `CARGO_TARGET_DIR` 时使用实际路径。将同一份 `probe.c` 编译为两个二进制，
仅分别定义 `KU_BENCH_OBSERVE=0` 和 `=1`。两次必须使用相同编译器、优化、CRT 和
链接设置，例如 MSVC 的 `/O2 /std:c11 /utf-8 /MD`，或 POSIX 的 `-O2 -std=c11 -pthread`。
保留实际完整命令、编译器版本、C/二进制哈希。移除原 C 后再运行二进制，不能用
另一个编译器或带额外 sanitizer 的程序替代其中一侧。

```text
native_task_scheduler_probe run PATH_TO_BINARY 64 1024 3
```

三个数字依次为 tasks（1..128）、每个 Task 的 quanta（1..65536）、正式重复次数
（1..7）。worker 数取可用并行度、32 与 task 数约束内的去重 1/2/4/available；
每组另执行一轮明确标注的 warmup，并保留全部原始 JSON 行。不得只重跑失败样本。
命令逐个运行有界子进程；非零退出、stderr、无输出、身份/计数错误均失败。

## 测量与资源边界

- 每个真实帧持有一个 Owned 字符串；一个 quantum 做一次减计数，前 N-1 次实际
  Suspend/Yield，第 N 次完成。返回 poll 数必须等于 tasks × quanta。
- 工作区间包含 commit 启动分布、执行、终态 frame/Owned drop 和最后 idle 观察；
  不含 worker 创建、输入分配和 owner/结果/线程销毁。只有后续真实 receipt ACK、
  join/close/destroy 与分配台账为零后，才允许输出成功样本。
- startup/cleanup 各使用原有一个有限至多一秒 D，工作观察上限十秒，每个 C 进程
  外部 watchdog 二十秒。清理不按 Task 续期；失败没有成功性能行。该工具并未证明
  所有 OOM/OS 错误都能恢复，也未以事件控制的取消测试冒充真实十秒超时回收测试。
- observer OFF 无每 poll 观察调用，迁移与各 worker 计数为 null；ON 在已有锁内
  记录实际 worker 分布/迁移，增加 stores、分支与缓存开销。两侧帧布局与接纳计费
  相同，但不宣称观察零成本；原有分配台账也会影响终态释放的测量。
- `elapsed_ms` 使用运行时毫秒时钟，可能为零。零不等于无限吞吐；小样本不足以
  支持细微百分比收益。跨 revision 只能 OFF 对 OFF、ON 对 ON，并固定相同工作量、
  编译/链接设置与机器条件；另外记录环境负载、CPU/电源/亲和性中实际测得的部分。
- `frame_bytes`、`instance_bytes`、`fixed_bytes` 是结构体/显式缓冲计费，不是 RSS，
  不包含 OS 线程栈等全部运行时资源。不要据此声称完整内存预算完成。

## 正确性门禁

```text
cargo +1.89.0 test --locked --example native_task_scheduler_probe -- --nocapture --test-threads=1
```

三系统 workspace job 和独立 ASan/UBSan job 显式执行这个例子的测试，不能仅以
`cargo check --all-targets` 或普通 workspace 测试编译了例子为通过。测试包括严格
JSON 行验收、两观察模式下 1/2/8 quanta 与 1/2/4 worker 的真实回收，以及实际
Complete/take 与取消竞争：两次 driver poll，但 resume/cleanup/drop 各仅一次。
该竞争复用真实通知和清理，没有手写 phase、ACK、joined 来制造成功。

JSON 校验使用 Ku 自己的有界解析器，合同针对解析后的 12 个字段；不是唯一键的
规范化编码或二进制真实性证明。本机没有 C 编译器时会明确记录 skip，
CI 则必须失败。上述正确性用例和 sanitizer 耗时不计作性能样本。
