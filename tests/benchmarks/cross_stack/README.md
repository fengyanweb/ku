# 跨语言业务与资源对照

本目录是 2026-10-22 任务书 T01—T08 的永久基准源码，不是性能结论。
集中结果见 `docs/multicore-capacity-acceptance.md` 和原有 evidence JSON。
候选运行时未公开发布，不能把这里的功能或成绩写成 v0.0.18-r1 已交付内容。

## 固定业务

`protocol.json` 冻结语料哈希、算法、数据库表、SLO、资源档和测量窗口。
四个服务分别使用 Ku native C、Go net/http、Rust Tokio/Axum、Node node:http；
Node 多核使用 cluster，长计算在正常的有界后台执行。锁文件保留准确依赖。
JSON 必须完整解码、构造新对象并重新编码；Ku 现有取值接口的子树复制成本也计入。
数据库使用私有 PostgreSQL 16 的 1024 行夹具，总池 16，不重试业务查询。
Ku 的 `/json` 在 `--ku-json text` 下使用 text/plain，`response` 模式通过公开
`http.json` 使用 application/json；`/db` 两种执行路径都保持原 text/plain。
Go/Java/.NET/C++ 的 JSON 响应带 UTF-8 charset；Rust/Node 使用无显式 charset 的
application/json。正文黄金规则相同，响应头差异按实际 API 记录，不能宣称完全等同。

HTTP 和计算的结果验证由公共 `client/` 执行。它按计划到达发起请求，不重放失败，
普通绝对期限从计划到达开始，包含压测端排队和完整正文验证。
窗口结束后不新增计划，已有计划在原绝对期限内仍可发出和完成；窗口外完成不回填 goodput。
客户端 v1 曾提前丢弃这类计划，相关准备数据保留并标明无效。
v2 的256个worker各持连接，低速轮转时超过500ms空闲期限，实际成为重连负载；
这批数据仅作诊断，不能混入持续连接主表。v3 保持并发、队列、期限和空闲期限不变，
改为共享有界LIFO空闲连接池，每条连接同一时间只服务一个请求；错误或close连接不复用、不重放。
报告实际dials和请求/连接，避免仅凭keep-alive头假定已经复用。
v3准备记录发现虚拟机墙钟回调，不能用Unix时间关联独立进程的资源和撤压窗口。
v4在到达起点捕获Linux CLOCK_MONOTONIC锚点及误差界；资源与恢复只使用该时钟，
Unix字段仅供阅读。锚点不可用或误差超过1ms时将测量标为不可判定，不改服务SLO；
撤压5秒边界保守包含两端锚点误差，不能借墙钟调整或实际发送尾部续期。
原始终态保存在有界 gzip CSV，报告同时列计划、已发送、正确、拒绝、超时和未发送。
不足的尾部样本标记不足；五轮合并原始分布，不平均各轮 p99。

## 独立 Linux 环境

参考环境为 8 vCPU、4 GiB 的 ARM64 Ubuntu 24.04 虚拟机。虚拟 CPU 不代表独占物理核。
正式四栈比较只在同一 Linux 环境内进行，macOS 黄金响应检查不用于排名。
当前 CLI 不能自动链接 Linux ARM64；这里保留其拒绝记录，并对未修改的生成 C 使用
clang-18 C11/O2/pthread 显式编译。它是实验基准构建，不新增官方发布平台。
Ku 文件基址按程序路径和编译时 locator 定位，运行目录使用独立 bin/ 与 ku-native/
布局放置程序和输入，避免误把 cwd 当文件基址，也不修改生成 C 来绕开语言语义。

- 服务完整进程树：CPU 0 至 cores-1；1/2/4 核；主档 1 GiB、小档 256 MiB；swap 0。
- 普通 Go 压测进程树：CPU 4—6；每个域 3 核、1 GiB。过载业务与独立普通请求探针
  使用两个域，合计额度不应写成 1 GiB。
- 私有数据库及其子进程：CPU 7；1 核、512 MiB。
- root 协调器单独观察并记录开销；不声称服务域限制覆盖整个虚拟机。
  等待组的 Python selector、受控 TCP peer 和观察线程固定在 CPU4—6，
  但没有单独的 1 GiB 内存硬限制；其成本在服务域外，不与普通 Go 客户端的限制混写。

所有测量进程先进入 cgroup v2 再执行服务。集群主进程、worker、GC 与后台线程继承同一域。
采样观察包含 cgroup CPU/内存峰值、anon/file、OOM、PSS、FD、线程和实际任务指标。
RSS 求和可能重复计算共享页，不能当作独占物理内存。
各框架准入配置不完全等价，报告其实际机制，不添加只拖慢某方的模拟层。

```sh
# 独立、可清理的 Linux guest；不要在真实数据库或生产主机执行。
sudo env BENCH_ROOT=/opt/ku-bench BENCH_USER=mac bash tests/benchmarks/cross_stack/provision-linux.sh
# 将确切候选源码复制到 /opt/ku-bench/src；不要复制构建缓存和 node_modules。
python3 -B /opt/ku-bench/src/tests/benchmarks/cross_stack/build_linux.py --root /opt/ku-bench
sudo python3 -B /opt/ku-bench/src/tests/benchmarks/cross_stack/database.py --root /opt/ku-bench start
sudo python3 -B /opt/ku-bench/src/tests/benchmarks/cross_stack/linux_harness.py \
  --root /opt/ku-bench --output /opt/ku-bench/results/golden --phase smoke
sudo python3 -B /opt/ku-bench/src/tests/benchmarks/cross_stack/linux_harness.py \
  --root /opt/ku-bench --output /opt/ku-bench/results/formal --phase all
sudo python3 -B /opt/ku-bench/src/tests/benchmarks/cross_stack/database.py --root /opt/ku-bench stop
```

构建使用夹具用户 uid 501/gid 1000；不同环境显式传入一致的 `--uid`/`--gid`。
`build_linux.py` 要求非 root；数据库和 cgroup 编排要求 root。依赖和生成物留在 ROOT/build、
ROOT/tools，源码目录保持可核对。安装脚本默认账号和路径由其开头的环境变量确定。
实测前保存 environment、manifest、源码清单和程序哈希。改变冻结输入时使用新结果目录，
不得把不同候选或配置的轮次拼在一起。

## PostgreSQL 执行路径对照

当前 Ku 默认源码使用具名局部 `async fn database` 捕获原初始化 pool：`query()` 挂起、
`value_int()`/`value()` 读取独立结果，再通过原 `json.encode` 和 `http.text` 返回。
`mixed` 组合直接访问同一个 `/db` 路由，不额外创建 client，也不使用测试专用数据库接口。
SQL、16 连接总池、DISCARD ALL、原绝对期限及不重试约定不变。
PG 的 `http.text` 仍与历史 Ku 对照列保持相同文本 MIME；不能将它写成 application/json。

`build_linux.py --ku-pg-execution sync --ku-pg-read value` 单独构建旧有界 HTTP 线程路径和
字符串→JSON 数字读取；`--ku-pg-execution sync --ku-pg-read int` 只分离 getter 的影响。
默认是 `--ku-pg-execution async --ku-pg-read int`；async 不支持动态 KuValue，不能选择
`value` 模式。三种组合分别使用隔离 benchmark root，manifest 记录执行模式、getter 模式、
原源码、展开源码、生成 C 和实际 ELF 哈希。不得把旧 baseline 的历史成绩重新标为 async。

## 分阶段执行与清理

`--phase` 支持 smoke、explore、formal、overload、sustained、waiting、batch、all；
`--case c4-m1073741824-json_object` 可选单个矩阵条目，其他参数默认对应冻结协议。
等待组完整观察真实 timer/TCP 的挂起与结束，不拿 QPS 代替并发等待数量。
批处理分别观察新进程启动和固定计算，不从计算结果中减去启动成本。
正式 L1 七栈统一使用 `iterations=512000, seed=3, repeat=32768`，逐项核对全部
checksum；历史 `repeat=64` 结果保留为 R&D，不进入正式表。每轮整个进程组实际墙钟
必须至少 30 秒，且每个进程的内部 `compute_ms` 都至少 30000；任何一项不足就保留
该窗口并停止该栈后续正式轮次，不加 sleep、不自动增加工作量，也不挑选重跑结果。
`repeat=0` 只观察新进程启动，不要求 30 秒。进程组从统一放行到最后自然退出的绝对
上限为 600 秒，每个 stdout/stderr 上限为 1 MiB；100 毫秒资源快照最多 6016 条，
覆盖整个执行窗口。采样可能遗漏间隔内的 RSS/current 瞬时峰值，OS `memory.peak`
仍单独保留。原有全部结果正确、自然退出、OOM、资源域和临时目录回收要求继续执行。
正式测量前完成黄金验证；失败的准备步骤和无效窗口保留原因。

公共客户端回归使用 `go test ./...` 和 `go test -race ./...`；
跨进程时钟/恢复的反例使用 `python3 -B -m unittest discover -s tests/benchmarks/cross_stack -p test_linux_harness.py`。
批处理时长、流上限和停止规则复用 `test_compile_variants.py` 的小回归；它使用合成结果
检查门槛，不真正执行 32768 次计算，也不生成性能结论。

保留本目录、锁文件、精简原始结果和必要失败证据。一次性探针、重复包、临时构建和
`node_modules`/`__pycache__` 用完删除。清理只作用于本次拥有的私有目录、进程和 cgroup。
不改官网，不重跑旧 36 次内存验证，不恢复两个暂停分析模块。
