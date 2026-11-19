# HTTP + PostgreSQL 任务清单

需要已安装的 Ku 0.0.20、C 编译器、匹配目标的 PostgreSQL `libpq` 和 `psql`。把 `http_pg.ku`、`http_pg_schema.sql` 和本说明复制到空目录。需要原有的数据库信息页面时，再复制 `http_pg_frontend.html`。

## 启动

选一个专用开发库，用其中允许建表和读写的账号。下面的连接参数是示例，请换成你的开发库。密码可以交给 PostgreSQL 的密码文件，避免出现在命令历史中。

```sh
export KU_TASKS_DSN='host=127.0.0.1 port=5432 dbname=ku_dev user=ku_dev'
umask 077
printf '%s\n' "$KU_TASKS_DSN" > db.conn
printf '%s\n' '127.0.0.1:8090' > http.addr
psql -X -v ON_ERROR_STOP=1 -d "$KU_TASKS_DSN" -f http_pg_schema.sql
KU_PG_LIB="$(pg_config --libdir)" ku build --release http_pg.ku -o task-list && ./task-list
```

`db.conn` 和 `http.addr` 放在可执行文件旁边。修改 `http.addr` 即可换端口。服务直接监听该地址，建表由上面的 `psql` 命令显式完成。

Windows 使用 PowerShell 7 写入 UTF-8 配置。将 `KU_PG_LIB` 指向匹配目标的导入库目录，确保同一套 PostgreSQL 的运行时 DLL 可用。MSVC 使用 `libpq.lib`，MinGW GNU 使用 `libpq.dll.a`。请先把它的实际绝对目录设为 `$env:KU_PG_LIB`。

```powershell
$env:KU_TASKS_DSN = 'host=127.0.0.1 port=5432 dbname=ku_dev user=ku_dev'
$env:KU_TASKS_DSN | Set-Content -Encoding utf8NoBOM db.conn
'127.0.0.1:8090' | Set-Content -Encoding utf8NoBOM http.addr
psql -X -v ON_ERROR_STOP=1 -d $env:KU_TASKS_DSN -f http_pg_schema.sql
ku build --release http_pg.ku -o task-list.exe && .\task-list.exe
```

## 创建和查询

```sh
curl -i -X POST http://127.0.0.1:8090/tasks \
  -H 'Content-Type: application/json' \
  --data-binary '{"title":"整理任务🙂"}'
curl -i http://127.0.0.1:8090/tasks/1
```

创建返回 `201` 和数据库生成的 `id`、`title`。用实际返回的 ID 替换查询命令中的 `1`。查询成功返回 `200`。标题缺失、类型错误或 ID 非法返回 `400`，找不到合法 ID 返回 `404`，数据库请求失败返回 `503`。

创建使用一次参数化 `INSERT ... RETURNING`。客户端断开或未收到响应时，数据库可能已提交。服务不会自动重放写入。重启 Ku 服务时沿用同一个开发库，已有记录仍可查询。

## 私有库验证

从仓库运行以下定向验证。它只使用自己启动的临时 PostgreSQL，跑完会排空 Ku 服务、关闭数据库并清理临时目录。

```sh
python3 -B tests/acceptance/http_pg_lifecycle.py --tasks-example \
  --ku "$(command -v ku)" --output /tmp/ku-tasks-verification.json
```

`std.pg` 当前由默认 native 后端提供。显式 Runner 可以构建该示例，启动时缺少 PostgreSQL 支持。
