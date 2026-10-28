# 跨语言业务与资源对照

本目录保留 Ku、Go、Rust、Node.js、Java、.NET、C++ 七栈基准源码及共享驱动，不是性能结论。
集中结果见 `docs/multicore-capacity-acceptance.md` 和对应 evidence JSON；英文说明见
[`README.en.md`](README.en.md)。每次运行须绑定候选提交、源码、CLI、实际服务二进制和协议哈希。
更新版本或本文不改变历史结果身份，也不证明某项功能已进入公开安装包。

## 固定业务

`protocol.json` 冻结语料哈希、算法、数据库表、SLO、资源档和测量窗口。
七个服务分别使用 Ku native C、Go net/http、Rust Tokio/Axum、Node node:http、
Java Vert.x、.NET Kestrel 和 C++ Drogon；Node 多核使用 cluster，长计算在正常的
有界后台执行。各栈锁文件及构建收据保留实际依赖与工具链身份。
原 Ku/Go/Rust/Node 矩阵保留 1/2/4 核和既有业务组合；Java/.NET/C++ 的正式 HTTP
范围为 4 核的 `ok`、`json_object`、`mixed`，其中 256 MiB 档含 `json_object`、`mixed`。
新增三栈不自动扩展为原 TCP 等待矩阵；批处理另按共同内核验收。
JSON 必须完整解码、构造新对象并重新编码。当前 Ku 用显式 `decoded.take("data")?`
消费根对象并转移子树；旧索引接口仍返回 owned 副本。旧／新必须分别绑定源码身份，
不能把历史索引路径成绩重新标为 take。
数据库使用私有 PostgreSQL 16 的 1024 行夹具，总池 16，不重试业务查询。
Ku 继续使用同一租约的 `DISCARD ALL`；Java/.NET/C++ 保留各自真实连接池的重置行为，
不人为补上相同语句。SQL 和结果黄金检查相同，不代表连接重置成本完全等价。
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
当前 v5 另记录 HTTP 状态、传输阶段、发送滞后、dial、冷连接与复用连接分布。
不足的尾部样本标记不足；正式比较至少七轮，合并相同身份与配置的分布，不平均各轮 p99。
完整驱动门禁与计费口径见 [`client/README.md`](client/README.md)；驱动无效或环境干扰
未排除的窗口不能证明服务容量或语言优劣。

## 独立 Linux 环境

参考环境为 8 vCPU、4 GiB 的 ARM64 Ubuntu 24.04 虚拟机。虚拟 CPU 不代表独占物理核。
正式比较只在同一 Linux 环境内进行，macOS 黄金响应检查不用于排名。
当前 CLI 支持 Linux ARM64 本机 native 链接；`build_linux.py` 使用实际 CLI 的
`--backend c --profile release --emit-c` 产物，并保存生成 C、ELF 和动态依赖身份，
不再将历史拒绝退出当成功，也不手工重编生成 C。旧显式 clang 链接结果保留原构建身份。
Linux ARM64 基准构建能力不等于新增该平台的官方完整 SDK 发布包。
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
python3 -B /opt/ku-bench/src/tests/benchmarks/cross_stack/build_linux.py --root /opt/ku-bench \
  --candidate "$(git -C /opt/ku-bench/src rev-parse HEAD)" \
  --ku-format helpers --ku-json text --ku-pg-execution async --ku-pg-read int \
  --only ku go rust node java dotnet cpp client
sudo python3 -B /opt/ku-bench/src/tests/benchmarks/cross_stack/database.py --root /opt/ku-bench start
sudo python3 -B /opt/ku-bench/src/tests/benchmarks/cross_stack/linux_harness.py \
  --root /opt/ku-bench --output /opt/ku-bench/results/golden --phase smoke \
  --stacks ku go rust node java dotnet cpp
# 按已批准范围显式选择一个共同正式条目；本命令会执行协议规定的正式轮次。
sudo python3 -B /opt/ku-bench/src/tests/benchmarks/cross_stack/linux_harness.py \
  --root /opt/ku-bench --output /opt/ku-bench/results/formal-json --phase formal \
  --case c4-m1073741824-json_object --stacks ku go rust node java dotnet cpp
sudo python3 -B /opt/ku-bench/src/tests/benchmarks/cross_stack/database.py --root /opt/ku-bench stop
```

`build_linux.py` 默认构建七栈和公共客户端；`linux_harness.py` 默认仍选原四栈，
七栈运行必须显式传 `--stacks`。上面的正式条目是使用示例，不是每次文档或版本更新都要执行。
单轮固定负载只验证该次窗口；不能写成七轮正式比较、完整矩阵或长期稳定性通过。

构建使用夹具用户 uid 501/gid 1000；不同环境显式传入一致的 `--uid`/`--gid`。
`build_linux.py` 要求非 root；数据库和 cgroup 编排要求 root。依赖和生成物留在 ROOT/build、
ROOT/tools，源码目录保持可核对。安装脚本默认账号和路径由其开头的环境变量确定。
实测前保存 environment、manifest、源码清单和程序哈希。改变冻结输入时使用新结果目录，
不得把不同候选或配置的轮次拼在一起。

## Ku 构建模式

默认组合为 `--ku-format helpers --ku-json text --ku-pg-execution async --ku-pg-read int`。
`helpers` 保留模板中的格式化调用路径；`direct` 使用公开 `str(int)` 与相同八位零填充，
属于单独记录的比较变体。`text` 使用 `json.encode` 加 `http.text`；`response` 使用
公开 `http.json`。变体的展开源码、生成 C、ELF、MIME 和完整调用成本分别记录，
不能把 direct/response 的结果当作默认 helpers/text，也不能重新标注旧成绩。
取消、期限、预算、正常值所有权和清理检查始终保留，不使用已撤回的实验实现。

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

`--phase` 支持 smoke、diagnose、explore、formal、overload、sustained、waiting、batch、all；
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
本基准入口不部署官网、不隐式重跑旧 36 次内存验证，也不恢复暂停的通用值复制／释放
及 AST 分析。官网内容与打包按独立授权进行，不能用它替代运行证据。
