---
title: 数据库客户端
description: PostgreSQL、Redis、MySQL 与 MariaDB 的配置、查询、结果、错误和生命周期。
kuVersion: '0.0.18'
verifiedAt: '2026-10-21'
sources:
  [
    'src/stdlib/metadata.rs',
    'src/backend/c.rs',
    'src/cli.rs',
    'docs/protocol-foundation.md',
  ]
sidebar:
  order: 9
---

Ku 通过 `std.pg`、`std.redis`、`std.mysql` 提供有界连接池客户端。这些是 **native C 接口**，解释器和默认打包 runner 尚未实现。构造函数返回 Result；client 只能转移，拥有它的作用域结束时释放，也可调用会消费它的 `client.close()`。

## 构建与依赖

把下方任意完整示例保存为指定文件，用 `ku check file.ku` 检查，再构建运行：

```sh
ku build --backend c -o database-example file.ku
./database-example
```

这些示例依赖用户自己的本地数据库服务和账号，不是离线示例。请使用可丢弃的数据库和 Redis 键命名空间。Windows 输出文件需要 `.exe` 扩展名。

| 模块        | native 依赖                                | 运行条件                                                                          |
| ----------- | ------------------------------------------ | --------------------------------------------------------------------------------- |
| `std.pg`    | 动态 libpq                                 | loader 能找到匹配目标的 libpq 及其依赖；需要时用 `KU_PG_LIB` 指定库目录绝对路径。 |
| `std.redis` | 内置 RESP2 实现                            | 可连接的 Redis 服务；无需 hiredis。                                               |
| `std.mysql` | Oracle MySQL client 或 MariaDB Connector/C | 同一安装来源的匹配动态库与头文件；需要时设置 `KU_MYSQL_LIB`、`KU_MYSQL_INCLUDE`。 |

这些环境变量用于 native 链接，不是数据库凭据。MySQL 自动链接支持 host 构建，显式非本机 target 尚无自动 MySQL 库合同。编译器检查库文件，不会安装或捆绑数据库服务，也不表示所有服务端、客户端与平台组合都已验收。

## 三个 client 的池配置

| 字段                     | 默认值 | 范围与作用                                    |
| ------------------------ | ------ | --------------------------------------------- |
| `max_connections:int`    | 8      | `1..256`，连接池最大连接数。                  |
| `max_waiters:int`        | 64     | `0..4096`，等待操作上限；0 表示繁忙时不排队。 |
| `connect_timeout_ms:int` | 5000   | `1..300000`，建立连接预算。                   |
| `acquire_timeout_ms:int` | 5000   | `1..300000`，等待池连接预算。                 |
| `query_timeout_ms:int`   | 30000  | SQL client 使用，`1..300000`，查询预算。      |
| `command_timeout_ms:int` | 5000   | Redis 使用，`1..300000`，命令预算。           |

只填写对应 client 接受的字段。未知字段、错误类型或越界配置返回 `invalid_config`。池操作可能返回对应模块的 `pool_busy`、`acquire_timeout`、`connect_timeout`、`connect_error`、`client_closed`、`out_of_memory`。期限不表示能硬中断所有同步 DNS 或外部库调用；单结果大小限制也不是进程内存上限。

## PostgreSQL

从 `std.pg` 导入 `pg`。除池字段外，`pg.client(config)` 必填 `conninfo:str`，接受 libpq 连接串或 URI。不提供另一套 `pg.connect` 或裸连接 API。

| 方法                                      | 返回             | 含义                                        |
| ----------------------------------------- | ---------------- | ------------------------------------------- |
| `client.query(sql:str, parameters:[str])` | result 的 Result | 使用 `$1`、`$2` 等占位符；无参数也传 `[]`。 |
| `result.rows()` / `result.cols()`         | `int`            | 脱离连接的结果行列数。                      |
| `result.value(row:int, column:int)`       | `str!`           | 复制一个非 NULL 单元格为文本。              |
| `result.is_null(row:int, column:int)`     | `bool!`          | 区分 SQL NULL 与空字符串。                  |
| `client.close()`                          | `null`           | 消费并关闭 client。                         |

将自己的连接串放到 `postgres.ku` 旁的私有 `db.conn`。本地测试填写本地 host、数据库与账号；远端连接应在 conninfo 中设置 `sslmode=verify-full` 和可信 `sslrootcert`。不要提交或打印此文件。

```ku
import pg from "std.pg"
import fs from "std.fs"

fn main(): null! {
    client = pg.client({
        conninfo: fs.read("db.conn")?,
        max_connections: 4,
        max_waiters: 16,
        connect_timeout_ms: 5000,
        acquire_timeout_ms: 5000,
        query_timeout_ms: 10000
    })?
    result = client.query("SELECT $1::text, NULL::text", ["中😀"])?
    println(result.rows())
    println(result.cols())
    println(result.value(0, 0)?)
    println(result.is_null(0, 1)?)
    client.close()
    return ok(null)
}
```

连接到可用 PostgreSQL 后，输出为：

```text
1
2
中😀
true
```

`query` 返回独立拥有内容的结果，读取结果不会继续占用池连接。对 SQL NULL 调用 `value` 返回 `pg/null_value`；坐标越界也返回可恢复错误。此共享 client 不支持显式事务、会话状态、COPY 或独占连接。

## Redis

从 `std.redis` 导入 `redis`。`redis.client(config)` 必填 `host:str`，`port:int` 默认为 6379。可选 `password:str` 让每条池连接自动 AUTH；可选 `username:str` 要求同时有 password，使用 ACL 形式 AUTH。不要把真实凭据发送到不可信或公网明文连接。

| 方法                             | 返回    | 含义                                       |
| -------------------------------- | ------- | ------------------------------------------ |
| `client.ping()`                  | `null!` | 发送 PING 检查连接。                       |
| `client.set(key:str, value:str)` | `null!` | 设置文本值。                               |
| `client.get(key:str)`            | `str!`  | 读取文本；缺键返回 `redis/key_not_found`。 |
| `client.exists(key:str)`         | `bool!` | 判断键存在。                               |
| `client.del(key:str)`            | `int!`  | 删除数量，0 或 1。                         |
| `client.close()`                 | `null`  | 消费并关闭 client。                        |

保存为 `redis.ku`。本例要求 `127.0.0.1:6379` 有无认证的本地可丢弃 Redis，只创建和删除 `ku:docs:greeting`。使用自己的实例时按实际配置修改。

```ku
import redis from "std.redis"

fn main(): null! {
    client = redis.client({
        host: "127.0.0.1",
        port: 6379,
        max_connections: 4,
        max_waiters: 16,
        connect_timeout_ms: 5000,
        acquire_timeout_ms: 5000,
        command_timeout_ms: 5000
    })?
    client.ping()?
    client.set("ku:docs:greeting", "中😀")?
    println(client.get("ku:docs:greeting")?)
    println(client.exists("ku:docs:greeting")?)
    println(client.del("ku:docs:greeting")?)
    try {
        client.get("ku:docs:greeting")?
    } catch (error) {
        println(error.code)
    }
    client.close()
    return ok(null)
}
```

```text
中😀
true
1
key_not_found
```

Redis 文本必须是有效 UTF-8；缺键不会静默变为空串。单个 bulk 值上限为 64 MiB。此 client 仅支持表中 RESP2 命令，未提供 TLS、任意命令、RESP3、cluster、sentinel。`std.net` 已有 TLS 不代表 Redis 已加密。

## MySQL 与 MariaDB

从 `std.mysql` 导入 `mysql`。`mysql.client(config)` 必填 `host:str`、`user:str`、`password:str`、`database:str`；password 可为空，其他三项不能为空。`port:int` 默认为 3306。

| 方法                                        | 返回             | 含义                                  |
| ------------------------------------------- | ---------------- | ------------------------------------- |
| `client.query(sql:str, parameters:[str])`   | result 的 Result | 使用 `?` 占位符，返回脱离连接的结果。 |
| `client.execute(sql:str, parameters:[str])` | `int!`           | prepared statement，返回受影响行数。  |
| `result.rows()` / `result.cols()`           | `int`            | 结果行列数。                          |
| `result.value(row:int, column:int)`         | `str!`           | 读取非 NULL 单元格为独立文本。        |
| `result.is_null(row:int, column:int)`       | `bool!`          | 检查 SQL NULL，并做相同的坐标校验。   |
| `client.close()`                            | `null`           | 消费并关闭 client。                   |

保存为 `mysql.ku`。准备名为 `ku_docs` 的本地可丢弃数据库与账号，源码旁的私有 `mysql.pw` 只放密码，末尾不要换行。本例仅读取数据，不创建表。

```ku
import mysql from "std.mysql"
import fs from "std.fs"

fn main(): null! {
    client = mysql.client({
        host: "127.0.0.1",
        port: 3306,
        user: "ku_docs",
        password: fs.read("mysql.pw")?,
        database: "ku_docs",
        max_connections: 4,
        max_waiters: 16,
        connect_timeout_ms: 5000,
        acquire_timeout_ms: 5000,
        query_timeout_ms: 10000
    })?
    result = client.query("SELECT ?, NULL", ["中😀"])?
    println(result.rows())
    println(result.cols())
    println(result.value(0, 0)?)
    println(result.is_null(0, 1)?)
    client.close()
    return ok(null)
}
```

预期输出与 PostgreSQL 示例相同。所有查询均使用 prepared statement，无参数时传 `[]`。参数仅接受 `[str]`，没有 typed bind 或 SQL NULL 参数绑定。读取 SQL NULL 返回 `mysql/null_value`，先用 `is_null` 判断。不支持事务、依赖会话的操作、独占连接或顶层 `CALL`。本版没有要求加密并校验证书和主机名的公开 MySQL 配置，不能宣称已具备该保证。

## 错误、释放与副作用

使用 `?` 传播错误，或在 `try/catch` 中检查 `domain`、`code`、`message`。client 在拥有者作用域结束时关闭，包括普通错误展开路径；`close()` 消费 client，之后再使用会被拒绝。SQL 结果独立持有数据，随其拥有者释放；`value` 返回的字符串也独立拥有内容。

SQL client **不自动重试语句**。`execution_unknown` 表示已经发送语句，但最终结果不确定；`execution_completed_without_result` 表示已完成执行，但未能提供可用结果；`session_state_unsupported` 可在发送前或执行后产生，消息会区分阶段。不能根据超时、worker 退出、reset 或这些错误码推断已经回滚或可以重试。不确定的写入必须通过业务标识和数据库状态确认后，再决定是否人工重试。

连接归还前的 reset 清理所支持的会话状态，不会撤销已提交写入或外部副作用。SQL 由应用控制，值用参数绑定，数据库账号仅授予必需权限。本页说明 v0.0.18 接口，不表示数据库完整平台矩阵已经完成。
