# 示例说明

大部分示例可直接用解释器运行:

```bash
ku run examples/hello.ku
```

## 数据库示例(native-only)

`pg_demo.ku` / `redis_demo.ku` / `mysql_demo.ku` / `http_pg.ku` 使用 `std.pg` / `std.redis` / `std.mysql`,这些驱动目前**只支持 native 后端**(`ku build --native`),解释器 `ku run` 暂不支持连库。

### 凭据放在 gitignore 的本地文件里

为避免把密码写进源码或误提交,这些示例从**运行目录**下的本地文件读取凭据。以下文件已在 `.gitignore`:

| 文件 | 内容 | 用于 |
|---|---|---|
| `db.conn` | libpq 连接串,如 `postgresql://user:pass@host:5432/db?sslmode=require` | pg_demo、http_pg |
| `redis.pw` | Redis 密码(无密码则留空) | redis_demo |
| `mysql.pw` | MySQL 密码 | mysql_demo |

Redis / MySQL 的主机、端口等非机密信息直接写在示例源码顶部,按需修改。

### 运行时依赖(动态库要在 PATH)

- PostgreSQL:`libpq.dll`(PostgreSQL 安装目录的 `bin`)。
- MySQL:`libmysql.dll`(MySQL 安装目录的 `lib`/`bin`)。
- Redis:无外部依赖(RESP 协议由 Ku 自实现,走 Winsock)。

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

- **注入安全**:`pg_demo` / `mysql_demo` 演示参数化查询,注入 payload(如 `'; DROP TABLE users; --`)只会被当作纯文本值返回,不会破坏 SQL。
- **http_pg 用 http.text 返回 JSON**:当前 native 后端的 `http.json` 尚不可用,示例改用 `http.text` 返回手工拼的 JSON 字符串。
- 查询只读元数据(版本、库名、表数量等),不改动任何数据。
