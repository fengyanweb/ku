# 多循环服务中的数据库与输出

普通入口保持 `pg.client(config)?`、`mysql.client(config)?`、`redis.client(config)?`。client 在初始化时创建一次，由 handler 捕获共享；不为每个事件循环重复初始化业务和连接池。

| 适配 | 实际执行方式 | 连接与结果归属 |
|---|---|---|
| PostgreSQL/libpq，同步 handler | 非阻塞 libpq 接口及 poll/WSAPoll，由有界 handler 线程推进 | pool lease 独占一个 PGconn；结果复制为 Ku 自有的有界表格，归还连接后仍有效 |
| PostgreSQL/libpq，局部 async HTTP handler | query、结果读取、DISCARD ALL 在 Task owner 上分步推进；池和 socket 等待挂起；连接建立使用有界后台任务 | 复用初始化时的同一个 pool；捕获 token 持有 pool 与原闭包环境直到清理，HTTP handle 继续持有环境直到物理任务退休 |
| MySQL/MariaDB | 同步 C prepared-statement API，运行在有界 handler 线程 | 全进程 library once、逐线程 init/end，pool lease 独占 MYSQL；结果脱离原生 statement |
| Redis | 同步 socket/RESP 操作，运行在有界 handler 线程 | pool lease 独占连接，命令与解析错误按既有规则丢弃失去协议同步的连接 |
| 解释器、默认打包 runner、LLVM | 沿用已有支持矩阵；数据库模块仍为 native-only | 不宣称本轮新增其数据库实现 |

libpq 允许连接在不同线程之间先后交接，禁止同一个连接同时被多个线程操作；MySQL 也需要独占连接及正确的线程初始化。这里使用已有 client 的锁与 lease；不将一个 raw handle 同时交给多个事件循环。native Task 的 PG 支持限于下述 checked capture 路径，不代表所有数据库、任意 async 参数或动态 JSON 值都已支持。

## Native async HTTP 中使用同一个 PostgreSQL pool

在同步初始化作用域内创建 client，再由具名的局部 `async fn` 只读捕获；注册方式仍是普通 `app.get(path, handler)`。`client.query(sql, params)?` 在这个 Task 内会挂起，不占用后台线程等待 SQL 或会话重置。每个查询仍独占一个连接 lease，不会为每个请求创建连接池。

```ku
import {http, time} from "std"
import pg from "std.pg"

fn main(): null! {
    client = pg.client({conninfo: "hostaddr=127.0.0.1 dbname=app",
        max_connections: 16, max_waiters: 128})?
    async fn echo(req: HttpRequest): HttpResponse! {
        time.sleep(1)?
        result = client.query("SELECT $1::text", [req.query.message.clone()])?
        return ok(http.text(result.value(0, 0)?))
    }
    app = http.server()
    app.get("/echo", echo)
    app.listen("127.0.0.1:8080")?
    return ok(null)
}
```

注册 async route 后，该 route 捕获的 PG binding 不能显式 `close()`、move 或替换。检查器按原 lexical binding 区分同名局部变量；普通 `query()` 仍借用原 pool。`listen()` 返回后也不解除这条静态冻结，应让原作用域与闭包环境正常结束释放资源；不要在该作用域末尾增加显式 `client.close()`。scope drop、正常退出和取消清理保持有效。

捕获时就增加实际 pool 的持有计数，因此先等待 timer、尚未开始 query 时也不会失去 pool。每次 query 的 SQL/参数表达式按原顺序求值一次；挂起后只接收同代次操作结果。`PgResult` 为 Task 内的独占表格；`rows`、`cols`、`value`、`value_int`、`is_null` 沿用原错误与所有权规则。结果已从 libpq 复制，成功或失败的 `DISCARD ALL` 不使已取得的字段失效。PG result 不作为跨 Task 的任意动态 payload。

HTTP 绝对期限在 handler 准入时形成，并与 pool/query 的原期限取最小值；取得连接、SQL、结果与重置不会重新计时。该 async 路径的取消会关闭并淘汰独占连接；SQL 已发出时报告 `execution_unknown`，不宣称服务端已回滚，也不自动重试。原同步路径的 libpq cancel/drain 行为见下文。

这个 Task 子集也支持 `json.encode({id: result.value_int(0, 0)?, label: result.value(0, 1)?})?` 和兼容别名 `json.stringify`。仅限静态 object literal、`int`/`bool`/`null`/`str` 字段；不扩展 float、动态 KuValue 或嵌套数组/对象的 Task 存储支持。字段表达式依源码顺序各求值一次，再按现有 JSON 的键顺序输出。重复字段继续由检查器和 native lowerer 拒绝。

literal 中的拥有型字符串按原规则移入临时字段；需要继续使用原变量时显式 `.clone()`。编码器只借读已经物化的字段，完成后释放临时 owner。使用原 JSON buffer、转义和整数 writer，输出上限仍为 1,000,000 字节。Task 会在分配前预留实际 buffer capacity；容量计算溢出、内存预算拒绝、分配失败和编码失败均返回错误 Result，失败 buffer 会清理并退款。该能力不更换 JSON 默认底层实现。

## PostgreSQL 整数字段

`result.value_int(row, col): int!` 直接读取独立结果表，不复制字段字符串，也不调用 JSON 解析器。它借用结果，读取后仍可继续使用 `result`；成功返回的整数独立于结果和连接的生命周期。原有 `value()` 仍返回拥有型字符串。

输入必须是规范 JSON 整数 token：`-?(0|[1-9][0-9]*)`，范围为有符号 64 位，`-0` 返回 `0`。不接受外围空白、`+`、前导零、小数、指数或内嵌 NUL；这比允许外围空白的 `json.decode(...).as_int()` 更严格。NULL 返回 `pg/null_value`；越界或无效结果存储返回 `pg/value_error`；格式非法或溢出返回 `pg/integer_value_error`。PostgreSQL 原生整数列的规范文本可直接使用：

```ku
result = client.query("SELECT id,label,score FROM items WHERE id=$1", ["1"])?
id = result.value_int(0, 0)?
label = result.value(0, 1)?
score = result.value_int(0, 2)?
```

该方法沿用 PG 的 native C 支持边界，并可用于上述局部 async HTTP 路径；不新增解释器或 LLVM 数据库实现，会话清理与 SQL 不自动重试的规则不变。

## 实例总额

`src/backend/c_database_budget.c` 为 PG、MySQL 和 Redis 共同发出一份进程账本，循环数不放大它。环境在第一次创建 client 时读取一次，运行中修改不生效；非法设置导致 client 准入失败。

| 设置 | 默认 | 含义 |
|---|---:|---|
| `KU_DB_MAX_CONNECTIONS` | 256 | 全部存活 client 声明的 `max_connections` 容量之和 |
| `KU_DB_MAX_WAITERS` | 4096 | 全部存活 client 声明的 `max_waiters` 容量之和 |
| `KU_DB_MAX_PENDING_BYTES` | 67108864 | 尚未物理退出的调用所持 SQL、参数、key/value 输入与固定操作记录字节 |

创建 client 时先预留容量，再分配和建连；部分失败会退款。消费 close 时仍有借用者或等待者，容量一直保留至最后责任人完成 dispose。操作字节在连接池等待、查询、取消和结果清理期间一直占用，不能由已经超时的 HTTP 请求提前归零。超额返回对应模块的 `resource_limit`；client 原有 `pool_busy`、`acquire_timeout`、连接期限与查询期限继续有效。

`reserved_connections`/`reserved_waiters` 是已承诺容量，不是实时已打开连接数。`pending_operations`/`pending_bytes` 是本地尚未退出的数据库调用。SQL/参数转换副本、原生驱动内部缓冲和数据库服务端执行资源不被冒充为此账本的逐 Task 精确堆用量；第三方库的本机内存仍依靠受控进程的 OS 限制。

两个服务实例和更新期间的新旧实例会分别拥有额度。部署总额必须相加，例如实例 2 个、更新最多额外 1 个，每实例连接容量 4，数据库需容纳至多 12 个客户端容量以及运维余量。数据库端另设专用非超级用户角色的 `CONNECTION LIMIT`、`statement_timeout` 和业务需要的锁等待期限，才能约束已经断开本地连接、但仍在服务端运行的工作。不要用超级用户测试角色限额，也不要将关闭 socket 等同于数据库已经停止。

## PostgreSQL 取消与结果未知

已发送语句的超时或断连不触发 SQL 重放。新请求可以重新取得连接，旧操作不会自动重试。

失败路径在有界 handler 内优先使用 libpq 17 起可用的 `PQcancelCreate`/`Start`/`Poll`/`Finish`，保留原连接的 TLS/GSS 要求；符号不存在的旧库只在原连接未使用 TLS/GSS 时使用 `PQgetCancel`/`PQcancel`。旧库的加密连接不发送降级取消，丢弃连接并报告结果未知，服务端期限与连接额度负责远端边界。取消后读取原连接直到最终 `PQgetResult == NULL`。清理使用独立、有限的既有 1000 ms 清理预算，不恢复查询执行期限。只有原连接返回终态，才累计 `confirmed_cancellations`。无论取消是否确认，失败连接均丢弃，原先的 `execution_unknown`/`execution_completed_without_result` 结果不被改写成“已回滚”。

旧库的 `PQcancel` 本身是不能强制中断的库调用；新接口的非阻塞发送和原结果排空共用同一个清理截止时间。其执行线程、连接 lease 和操作字节持续保留；库调用长时间不返回时，按服务进程退役/终止边界回收，不另开无限替代线程。发送取消成功但结果尚未排空，以及库调用自身未返回，都不能报告清理完成。

## 有界服务输出

含 HTTP 的 native 程序默认使用 `KU_SERVICE_OUTPUT=bounded`。普通非 HTTP 程序保持原来的同步 stdout 行为；显式选择 `sync` 的服务也保留同步行为，但不提供慢日志出口隔离保证。

一个独立输出线程持有 stdout/stderr 写入，使用固定 256 个记录，每条最多 4096 字节。满队列或超大记录记录 `dropped_records`/`dropped_bytes`；实际写失败记录 `failures`，不会用 stderr 同步输出一次新的失败日志。`queued_records`/`queued_bytes` 包含正在阻塞写出的队首，最多 256 条/1048576 字节；记录元数据、同步对象和线程栈另计。线程纳入统一线程数和栈预算。

正常返回会有限等待日志排空。底层 fwrite/fflush 永久不返回时，不能释放它持有的队列或无限 join；到进程退出边界用 `_Exit` 结束该进程，由 OS 回收仍持有的资源。服务 handler 与事件循环不持有 stdio 锁。bounded POSIX 服务在启动线程前将 SIGPIPE 设为忽略，关闭日志收集管道只计入 EPIPE 写失败；普通程序不改变其信号处理。写失败后退出也使用进程边界，防止 libc 在主线程再次 flush。此机制覆盖语言的 print/println、KuValue/Time 输出和已接入的 Task 输出；不可恢复的底层原生 fail-stop 诊断仍属于进程恢复边界。

## 隔离验收

```sh
python3 tests/acceptance/http_pg_lifecycle.py --ku /absolute/path/to/ku --loops 4 --output /absolute/path/to/report.json
```

脚本仅用新建的临时 PostgreSQL cluster，不读取工作区 `db.conn`，不接触已有数据库。它验证参数、中文/emoji、NULL、逐字段、写入副作用、10000 行结果、查询取消、数据库断连、有界依赖压力、新请求恢复，以及正常 drain 后连接归零。测试代理仅作用于该临时 cluster：丢失一个已完成 INSERT 的确认，断言 API 报结果未知、数据库实际恰好 1 行，防止把重连当作自动重放。

必要永久回归位于 `native_database_budget_test`、既有 PG/MySQL/Redis 测试及 `native_service_output_test`。后者故意不消费 stdout，核对生产者仍返回、真实 HTTP 请求继续、丢弃可观测和有限退出。临时源、可执行文件和 cluster 在结束时清理，保留精简报告。

线程契约参考：[PostgreSQL 17](https://www.postgresql.org/docs/17/libpq-threading.html)、[libpq 异步调用与排空](https://www.postgresql.org/docs/17/libpq-async.html)、[新版与旧版取消差异](https://www.postgresql.org/docs/17/libpq-cancel.html)、[MySQL C API 线程契约](https://dev.mysql.com/doc/c-api/8.4/en/c-api-threaded-clients.html)。
