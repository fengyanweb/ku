# 示例说明

大部分示例可直接用解释器运行:

```bash
ku run examples/hello.ku
```

## Package 示例

`examples/package` 是可确定性打包的最小 package：

```bash
ku check examples/package/src/main.ku
ku package pack examples/package
```

第三方 registry 发布与消费统一使用 `ku.mod` 的 `registry.url`、`registry.public_key` 和 `dep.name`；token 只从 `KU_REGISTRY_TOKEN` 读取。完整流程见 `docs/package.md` 和 `docs/registry-api.md`。

## 数据库示例(native-only)

`pg_demo.ku` / `redis_demo.ku` / `mysql_demo.ku` / `http_pg.ku` 使用 `std.pg` / `std.redis` / `std.mysql`。三者只有一种普通业务写法：`module.client(config)?` 创建内部自动池化的 client，随后调用 receiver 方法；旧的 raw connection / 手动 pool 模块函数不再支持。这些驱动目前**只支持 native 后端**(`ku build --native`),解释器 `ku run` 暂不支持连库。

三个驱动的构造前配置错误统一为 `invalid_config`。业务 `catch(err)` 可以跨驱动统一处理 client/池阶段的 `client_closed`、`pool_busy`、`acquire_timeout`、`connect_timeout`、`connect_error`、`sync_error`、`out_of_memory`。命令或 SQL 已发送后的 `execution_unknown` / `execution_completed_without_result` 等错误不属于该集合，禁止自动重试。PG/MySQL 的 `session_state_unsupported` 也不能仅凭 code 自动重试：前置固定消息表示 SQL 尚未发送，后置固定消息表示语句已经或可能已经执行且 payload 已丢弃。

### 凭据放在 gitignore 的本地文件里

为避免把密码写进源码或误提交,这些示例从**运行目录**下的本地文件读取凭据。以下文件已在 `.gitignore`:

| 文件 | 内容 | 用于 |
|---|---|---|
| `db.conn` | libpq 连接串；生产远程连接建议 `host=... hostaddr=... dbname=... user=... password=... sslmode=verify-full sslrootcert=C:/certs/root.crt`，预算通过 `pg.client` 的 `connect_timeout_ms` / `acquire_timeout_ms` / `query_timeout_ms` 设置 | pg_demo、http_pg |
| `redis.pw` | Redis 密码，文件末尾不要带 CRLF/LF；无密码服务应从示例 client 配置中删除 `password` 字段。当前驱动无 TLS，只用于 loopback/受控私网或已验证的 TLS tunnel 后端 | redis_demo |
| `mysql.pw` | MySQL 密码；当前 API 尚不能强制跨 MySQL/MariaDB 一致的证书与主机名验证，只用于 loopback/受控私网或已验证的 TLS tunnel 后端 | mysql_demo |

Redis / MySQL 的主机、端口等非机密信息直接写在示例源码顶部,按需修改。

### 运行时依赖(动态库要在 PATH)

- PostgreSQL:使用同一 PostgreSQL 安装目录 `bin` 中的 `libpq.dll` 及其依赖 DLL；不要混用其他软件随附的 libpq/OpenSSL DLL。
- MySQL:`libmysql.dll`(MySQL 安装目录的 `lib`/`bin`)。
- Redis:无外部依赖(RESP 协议由 Ku 自实现；Windows 走 Winsock，Linux/macOS 走 POSIX socket/poll)。Windows 已本地验证，Linux/macOS 仍待对应真机 CI 首次跑绿。

### 构建运行

在**仓库根目录**执行(http_pg 需要从根目录读 `examples/http_pg_frontend.html`):

```bash
# PostgreSQL
ku build --native examples/pg_demo.ku -o pg_demo.exe
./pg_demo.exe

# Redis
ku build --native examples/redis_demo.ku -o redis_demo.exe
./redis_demo.exe

# MySQL
ku build --native examples/mysql_demo.ku -o mysql_demo.exe
./mysql_demo.exe

# HTTP + PostgreSQL 端到端(浏览器打开 http://127.0.0.1:8090/)
ku build --native examples/http_pg.ku -o http_pg.exe
./http_pg.exe
```

### 说明

- **注入安全**:`pg_demo` 使用 `PQsendQueryParams`，`mysql_demo` 使用 `MYSQL_STMT`；两者的参数都由服务端绑定，不做 SQL 字符串替换。注入 payload(如 `'; DROP TABLE users; --`)只会被当作值。
- **池内会话隔离**：PG 每次归还前执行 `DISCARD ALL`，MySQL 使用 `mysql_reset_connection()`；reset/statement 清理失败会淘汰该连接。安全隔离会为每次 SQL 增加一次协议往返，示例不依赖跨查询 session 状态。MySQL 顶层 `CALL` 在实现有界的全结果消费前会在借连接前拒绝。
- **禁止盲目重试**：PG/MySQL 的 `execution_unknown` 表示语句可能已执行，`execution_completed_without_result` 只用于已确认终态后本地结果无法交付；两者都不能自动重试非幂等写入。MySQL 有列 prepared result 只有读到 `MYSQL_NO_DATA` 才确认当前结果终态，`mysql_stmt_execute()==0` 本身不够。
- **http_pg 的 JSON 输出**：`http.text` 只设置文本响应，body 必须由 `json.stringify` 生成；数据库返回文本禁止直接拼进 JSON。前端只用 `textContent` 创建数据节点，不把响应值写入 `innerHTML`。
- 查询只读元数据(版本、库名、表数量等),不改动任何数据。
