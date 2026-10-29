# PostgreSQL 两种明确对照合同

[English](pg-comparison.en.md)

这些模式只用于私有基准。业务 SQL 和 normal 模式的会话清理合同不变；
后续开发候选的协议路径变化单列说明。
显式 `BENCH_PG_MODE` 只接受空值、`normal`、`reset-equivalent`；其他值启动失败。
不能把 reset-equivalent 当成 Go/Rust 的惯用默认成本。

| 表 | Ku | Go | Rust |
| --- | --- | --- | --- |
| A normal | libpq text/single-row → Ku 自有表 → `value_int` 读 id/score、拥有型 `value` 读 label；每 lease `DISCARD ALL` | 原 pgx Exec text → int/string → encoding/json；原归池检查，无新增 reset | 原 query_typed binary INT4/String → serde_json；原 Fast recycle，无新增 reset |
| B reset-equivalent | 相同 Ku 路径，不新增第二次 reset | 一次显式 Acquire → 同一 lease QueryRow/Scan → 同一 lease DISCARD ALL → 排空后归池 | 一次 pool.get → 同一 Object query_typed → owned int/String → 同一 Object batch_execute DISCARD ALL → ReadyForQuery 后归池 |

B 表 Go/Rust 从操作入口开始共用 2s 绝对期限，获取 lease 至多 250ms；
reset 使用原操作剩余时间。Go 原请求 context 可进一步缩短期限；Rust 外层
HTTP 的 45s 超时可以取消整条 future，不会延长 2s 操作期限。
A 表原期限不变：Go 沿用原 HTTP 45s context、连接 2s；Rust pool wait/create
各 1s、query 1s。两张表不能混成一个“完全相同协议”的排名。

查询只发起一次。查询/字段转换失败会驱逐该 lease；reset 失败驱逐连接但保留
已完整拥有的结果，不重放 SQL。Go 通过 Hijack 脱池后关闭；Rust 用 guard 在
失败及 future 取消时 Object::take 脱池，ClientWrapper drop 中止连接任务。
无论关闭是否被 PG 接收，都不代表先前操作必然回滚。这里的业务 SQL 是固定
只读 SELECT；不能把它的结果推广成写入自动恢复保证。

保留各自惯用文本/二进制结果 API；B 不要求 Go/Rust 仿造旧 value 模式的
两次数值 JSON 解析，也不删除 Ku 的 session reset。固定 SQL/1024 行私库/池总量16/返回值
与独立正文校验不变。session、协议、转换成本差异在两表中分别列明。

## Ku 结果读取独立候选

当前默认是 `--ku-pg-execution async --ku-pg-read int`，使用受支持的
具名 async HTTP handler、`result.value_int(0,0)?` / `result.value_int(0,2)?`；
label 仍用拥有型 `value`。这已经包含在 0.0.19，不能算作后续的新优化。
`--ku-pg-execution sync --ku-pg-read value` 是保留旧夹具的诊断方式，
对 id/score 使用 `value→json.decode→as_int`，不是当前默认；async 不接受 value。
SQL、参数、session reset、数据库错误响应、JSON 默认和 HTTP text API 不变。
它可以与 `--ku-format direct`、`--ku-json response` 组合，但各自产物/展开源
哈希及 `pg_read_mode` / `pg_execution_mode` 必须记录。做旧接口诊断时先在
同为 sync 的条件下单独比较 `value`/`int`，不把组合收益
全部算给整数 getter；正常 A/B 的 Go/Rust 不加入对应冗余转换。

## 后续固定 reset 的协议候选

开发分支仅将无参数常量 `DISCARD ALL` 的发送由 extended 改为 simple query；
同步与 async 两条 cleanup 共用此规则，业务查询仍使用参数绑定。
reset 仍发生在同一 exclusive lease 上，保留原操作期限、完整读到终止 NULL、
事务 IDLE / UTF8 检查和失败驱逐。它不删除 reset，不新增自动 SQL 重试，
也不将 reset 移到 HTTP 响应之后。该候选需单独对照服务与 PG CPU、
协议及分配账；协议路径减少不能替代已测出的端到端收益。

## 生成独立测量清单

先用当前冻结源码的 `build_linux.py --only go rust` 重编两个服务。旧 ELF 没有
新模式，派生器会拒绝没有 `pg_modes`、本栈源码/锁文件哈希、二进制哈希的清单。
源文件或二进制变动也拒绝；不能把新源码身份贴给旧 normal 产物。

```sh
python3 pg_variants.py --manifest /absolute/new-build/manifest.json \
  --output /absolute/pg-normal --mode normal
python3 pg_variants.py --manifest /absolute/new-build/manifest.json \
  --output /absolute/pg-reset-equivalent --mode reset-equivalent
```

原 manifest 不写入、不覆盖。新清单只保留 Ku/Go/Rust、显式钉住 Go/Rust 的
环境模式，并保存原清单/工具/本栈来源与 ELF 哈希。其 `build_complete=false`
表示不是一次新的七栈完整构建，`pg_comparison_ready=true` 只表示身份核对完成；
不是实库正确或性能通过。用已有 Linux harness 分别指向两个目录执行同样的
DB 计划和配对轮次，正文、物理退休、CPU、内存、尾延迟照旧检查。

## 必要回归与当前边界

```sh
cd go
go test -race -mod=readonly ./...
BENCH_PG_TEST_URL='private-fixture-url' go test -mod=readonly -run 'TestReset' -v
cd ../rust
cargo +1.89.0 test --locked --offline
BENCH_PG_TEST_URL='private-fixture-url' cargo +1.89.0 test --locked --offline -- --ignored
cd ..
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest test_pg_variants -v
```

外部 `CARGO_TARGET_DIR`/`GOCACHE` 由运行者固定到项目外。未提供私库 URL 时，
Go 两个实库测试明确 skip，Rust 两个实库测试默认 ignore；这些不算实库通过。
测试只改本连接的 GUC 与临时表，不写业务表。实库测试验证同一 backend 的
临时状态被清除，reset 期限耗尽后拥有结果仍可读取且连接脱池，Rust 被取消
future 不会把未清理 lease 交还 Fast pool。Go 用 QueryTracer 仅在测试中于
Scan 完成后取消，确认结果不丢、业务 SELECT 一次且连接被驱逐。

本次接线不启动数据库、VM、服务或负载。两模式的 Linux 实库回归、正式配对
性能结果，以及 PG 线协议 round-trip 分段测量尚待执行，不能从这次代码接线
宣称 Ku 的 PG 差距由 DISCARD ALL 唯一造成，或已达到 O08 数值门槛。
