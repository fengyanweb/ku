# Ku

Ku 是一个正在开发中的小型编程语言和解释器。当前版本是 `0.0.16`，重点是补齐 native 标准库，并把 PostgreSQL / Redis / MySQL 收敛为统一、自动池化的数据库 client。Ku 仍处于实验阶段，0.0.x 会主动删除重复 API，目前不可用于生产。

> **0.0.16 数据库驱动均为 native-only（`ku build --native`），解释器 `ku run` 暂不支持连库。** 旧驱动底层曾分别通过 PostgreSQL / Redis / MySQL 实库验证；本次统一 client 与 MySQL `MYSQL_STMT` 路径必须以新的自动化和实库验收结果为准，不能沿用旧结果冒充新 API 已验证。详见 [0.0.16 版本记录](docs/v0.0.16.md)。

第一层协议的真实完成度、生产部署边界和通用 TLS 落地顺序见 [协议地基状态](docs/protocol-foundation.md)。

## 快速开始

```powershell
cargo build --release
.\target\release\ku.exe -h
.\target\release\ku.exe run .\examples\hello.ku
.\target\release\ku.exe check .\examples\index.ku
```

如果已经把 `ku.exe` 所在目录加入 `PATH`，可以直接：

```powershell
ku -h
ku create HelloWorld --template http
cd HelloWorld
ku check
ku build
ku run
```

在仓库根目录仍可以直接运行单文件示例：

```powershell
ku run examples/hello.ku
ku check examples/index.ku
```

## 命令

```txt
ku <file.ku>          Run a Ku source file
ku create <name>      Create a new Ku project directory
ku create <name> --template <template>
                      Create a project from a built-in template
ku create --list      List built-in project templates
ku init               Initialize the current directory as a Ku project
ku init --template <template>
                      Initialize the current directory from a template
ku template list      List built-in project templates
ku run [--locked|--offline] [file.ku]
                      Run a package entry or Ku source file
ku check [--locked|--offline] [file.ku]
                      Check a package entry or Ku source file without running
ku check --deny-unused [file.ku]
                      Treat unused local bindings as errors
ku check --json       Check nearest ku.mod package and emit JSON Lines diagnostics
ku check --json [--deny-unused] <file.ku>
                      Check and emit JSON Lines diagnostics
ku ir <file.ku>       Print checked Ku IR draft
ku llvm <file.ku>     Emit prototype LLVM text IR
ku build [file.ku]    Build a runnable executable package
ku build .            Build the nearest ku.mod package
ku build -o <path> [file.ku]
                      Build to an explicit executable path
ku build --release [file.ku]
                      Build with release profile
ku build --profile <debug|release|small|fast> [file.ku]
ku build --emit-c [file.ku]
                      Also emit prototype native C under .ku/build
ku build --emit-ir [file.ku]
                      Also emit checked Ku IR under .ku/build
ku build --backend c [--target <target>] [file.ku]
                      Build one native binary for host, x86_64-linux,
                      x86_64-windows, or aarch64-darwin
ku build --native [--locked|--offline] <file.ku>
                      Without -o, emit native C beside the source; with -o,
                      compile and link the native binary
ku package gc [path]
                      Remove unused package cache entries for a package
ku package pack [path]
                      Create a deterministic source package artifact
ku package publish [path]
                      Publish through the configured HTTPS registry
ku package yank [path]
                      Withdraw one version without deleting its immutable artifact
ku package resolve [path] [--locked|--offline]
                      Resolve and cache the complete dependency graph
ku version            Print version
ku -h | -help         Print help
```

`ku check` 会检查词法、语法和基础语义错误，并输出文件名、行号、列号和源码片段。`--deny-unused` 是严格 unused 第一阶段，会把未读取的本文件局部变量/常量报成 `E0905`；`_` 或 `_name` 表示有意丢弃。

## 0.0.15 支持的核心语法

```ku
struct User {
    name: str
    age: int
}

enum Result {
    Ok(value: int)
    Err(message: str)
}

fn main() {
    user = User { name: "Ku", age: 1 }
    user.age = 2

    values:[int] = [1, 2, 3]
    values[0] = 9

    result = Result.Ok(values[0])
    text = match result {
        Result.Ok(value) => str(value)
        Result.Err(message) => message
        _ => "none"
    }

    prefix = "Hello "
    greet = (name) => {
        return prefix + name
    }
    prefix = "Hi "

    println(text)
    println(greet(user.name))
}
```

可恢复错误使用 `T!`、`?`、`try/catch/finally`、`fail`：

```ku
fn read_name(): str! {
    fail "name missing"
}

fn main() {
    message = "none"

    try {
        message = read_name()?
    } catch (err) {
        message = "caught: " + err.message
    } finally {
        println("cleanup")
    }

    println(message)
}
```

基础类型固定为：

```txt
int
float
bool
str
null
```

Ku 不使用 `let` / `let mut`。首次赋值即声明变量，带类型写作 `name:type = value`。

## 0.0.16 新增

**native 标准库补齐**(以下均已对齐解释器 + 通过 CRT/ASan 验证):

- `str()`:整数/布尔/字符串/null 转字符串。
- 字符串方法 7/10:`len`(Unicode 码点)/`chars`(按 Unicode 标量拆分)/`contains`/`starts_with`/`ends_with`/`replace`/`slice`;`trim`/`lower`/`upper` 因需 Unicode 表暂以明确错误提示,不静默偏离。
- struct 复杂字段:`[int]`/`[str]` 基本数组、`[Person]` 结构体数组、`[[int]]` 嵌套数组、`enum` 字段。
- 模板字符串 `` `Hello {name}` `` 在 native 下正确插值。
- 修复非 ASCII/非打印字符字面量在 native 下的错误转义。

**数据库驱动**(native-only,详见 [0.0.16 版本记录](docs/v0.0.16.md),各驱动验证程度见上文说明):

```ku
import pg from "std.pg"
fn main(): null! {
    client = pg.client({
        conninfo: "host=... dbname=... user=... password=...",
        max_connections: 8,
        max_waiters: 64,
        connect_timeout_ms: 5000,
        acquire_timeout_ms: 5000,
        query_timeout_ms: 30000
    })?
    res = client.query("SELECT name FROM users WHERE id = $1", ["42"])?
    println(res.value(0, 0)?)
    client.close()
    return ok(null)
}
```

- **统一写法**：三个驱动都只公开 `module.client(config)?`，client 内部自动维护有界连接池；业务代码只调用 receiver 方法。连接数和等待者数有硬上限，所有操作共享绝对预算；同步 DNS、libpq/SSL 内部调用和 libmysqlclient FFI 不能被 portable C 硬抢占，只能在返回后复核 deadline 并淘汰过期连接。0.0.x 直接删除旧 raw connection、手动 pool 和模块级 query 入口，不提供兼容别名。构造前配置校验统一返回 `invalid_config`；client/池层跨驱动统一为 `client_closed`、`pool_busy`、`acquire_timeout`、`connect_timeout`、`connect_error`、`sync_error`、`out_of_memory`，阶段和重试边界见[版本记录](docs/v0.0.16.md#数据库驱动stdpg--stdredis--stdmysql)。
- **std.pg**（libpq 9.2+）：`client.query(sql, params)` 始终走服务端参数绑定，无参数也传 `[]`；`result.rows()` / `cols()` / `value()` / `is_null()` 读取与连接脱钩的 owned 结果。内部复用 nonblocking poll、单行增量聚合、严格 UTF-8/NULL/边界与 64 MiB 文本上限。明确识别出的 transaction/session-control SQL 在借连接前返回 `pg/session_state_unsupported`；成功响应若留下非 IDLE 状态会丢弃 payload、返回同一错误并淘汰连接。其余成功查询在原 deadline 内执行 `DISCARD ALL`，reset 失败、超时或协议失步淘汰对应连接，不自动重放 SQL。
- **std.redis**（自实现 RESP2-over-socket，零外部依赖）：client 配置中的用户名/密码会应用到每条懒创建连接；提供 `ping/get/set/exists/del`。`get` 只有一种严格语义：缺键返回 `redis/key_not_found`，不再把缺键折叠为空串。坏帧或 I/O 超时只淘汰对应连接；AUTH 拒绝、建连 transport、建连 timeout、池 timeout、命令 timeout 和 OOM 分阶段返回固定错误，服务端文本不直接进入诊断。
- **std.mysql**（libmysqlclient）：`client.query(sql, params)` 和 `client.execute(sql, params)` 都使用真正的 `MYSQL_STMT` 参数绑定，不再做 escape 后的 SQL 拼接；结果复制到 Ku owned、有上限的表后再归还连接。明确识别出的 transaction/session-control SQL 在执行前返回 `mysql/session_state_unsupported`；执行后若 server status 显示事务未结束或 autocommit 被关闭，会丢弃 payload、返回同一错误并淘汰单槽。普通完成路径仍调用 `mysql_reset_connection()`，清理/reset 失败也淘汰单槽；自动 reconnect 与 `LOCAL INFILE` 都显式关闭。client library 一次性初始化前会核对 header family/ABI major 与运行库的 numeric/string version，错配固定返回 `mysql/client_abi_mismatch`，且不会进入 library init、连接或 statement API。当前不支持 SQL NULL 输入参数和任意 binary 列。

前置 SQL 识别是 fail-closed 的有限语法防线，不是任意 SQL “session purity”的证明。存储过程、扩展函数或表达式仍可能产生驱动无法从 SQL 文本完整判定的会话、全局或外部副作用；普通 pooled client 不支持依赖跨调用保留的 session 状态，也不提供 transaction/exclusive 逃生门。后置状态检查和 reset 负责保护下一位借用者，但不能把已执行 SQL 的副作用回滚成“从未发生”。共享 client 不接受不受信任方提供的任意 SQL；生产账号必须最小权限，PG 不使用 superuser，MySQL/MariaDB 不授予 `SYSTEM_VARIABLES_ADMIN`、`RELOAD`、`FILE` 等业务查询不需要的管理权限。

SQL 错误有一条必须遵守的重试边界：语句发送前的配置、连接和借用失败保留各自错误；语句发送后无法确认终态返回 `execution_unknown`，已收到成功终态但本地结果无法交付返回 `execution_completed_without_result`。这两个错误都表示不能自动重试，否则 INSERT/UPDATE 可能重复执行。驱动自身从不重放 SQL。各结果上限是单次结果的防护，不是进程总内存硬上限；并发持有多个 detached result 时仍应使用进程级资源限制。

Redis 当前是明文 TCP，MySQL 当前也没有跨 Oracle/MariaDB 一致、fail-closed 的证书和主机名验证配置；二者只能直连 loopback/受控私网或已验证的 TLS tunnel。PostgreSQL 远程连接必须在 conninfo 中显式使用 `sslmode=verify-full` 和可信 `sslrootcert`。

client 的 `close()` 消费 Ku owned 句柄并立即拒绝新借用；已经在池内登记的借用由最后一个归还者完成延迟销毁，不用无界等待来“关池”。该保证以 Ku checker/closure 生命周期为边界：client 不可 clone、close 后源值清空、只读 HTTP handler 不能消费外层 client。生成的数据库 helper 全部是 translation-unit 内部 `static` 实现，不是第三方 C ABI；绕过 checker 后让裸指针调用刚进入、尚未登记时与 close 并发，属于明确不支持的调用方式。

连接恢复不会形成重拨风暴：一个 client 同时只执行一次懒建探测；失败退避窗口从 25ms 指数增长并封顶 1000ms，实际 equal-jitter 延迟落在 `ceil(window/2)..window`（首次 13～25ms）；健康空闲连接仍可立即借用。新请求必须先进入等待集合，不能以未排队身份直接夺取空闲槽；集合内由平台 condition variable 无序选择，不保证 FIFO 或无饥饿。`close()` 线性化前已开始的归还清理可能继续到完成，活动借用计数保证底层 client 不会被提前释放。

**稳定与安全**:路径级所有权 checker(部分 move 分析)作为第一道防线;对抗式审计发现并修复了循环内 `catch` 错误绑定、`?` 借用解包、`array.push` 字面量的内存泄漏。

## 模块和 Package

导出规则：顶层名字首字母大写对包外可见，小写只在当前文件内部使用。

```ku
import math from "./math"
import { Add, User } from "./math.ku"
import "./math.ku"
```

namespace import 支持函数、结构体和 enum：

```ku
import lib from "./lib.ku"

fn main() {
    user = lib.User { name: "Ku" }
    state = lib.State.Ready
    println(lib.Format(user))
}
```

有 `ku.mod` 时可以固定 package import root。推荐且唯一的依赖声明是：本地开发覆盖写绝对 `file://` URL；第三方 registry 依赖省略 `source`，并由项目固定 HTTPS origin 与 Ed25519 公钥：

```txt
name = "demo_pkg"
version = "0.1.0"
root = "src"
cache = ".ku/cache"

dep.util = "1.0.0"
dep.util.source = "file://C:/work/util"

registry.url = "https://packages.example/v1/"
registry.public_key = "ed25519-<64 hex digits>"
dep.math = "^1.2.0"
```

```ku
import { Value } from "@util/util"
```

包内 root import 使用 `"util"`，相对 import 使用 `"./util.ku"`，跨包 import 只使用 `"@<包名>/<路径>"`；不提供另一套 package import 别名。

`ku check` / `ku run` / `ku build` 共用同一个 resolver，并且都直接接受一个可选的 `--locked` 或 `--offline`；两个选项不能同时或重复出现。默认模式可求解并原子更新 `ku.lock`；`--locked` 只接受 lock 固定图且不改写 lock，缺少 registry cache 时仍可按 lock 的 exact HTTPS URL/checksum 下载；`--offline` 进一步禁止 registry 网络和 `file://` source 回读，只使用已校验 cache，也不改写 lock。file dependency 使用实际版本与快照 checksum，放进 `<package>/.ku/cache/packages/<name>/<name>-<version>-fnv64-<digest>/`；registry package 使用精确版本与 SHA-256 的内容寻址目录。refresh 只安装新的 immutable cache root，不覆盖或顺手删除旧 root；`ku package gc .` 才会清理当前 lock/manifest 之外的缓存和过期 staging。
file dependency 的常规写法只需要版本和绝对 `file://` source；resolver 会计算 `ku-fnv64-` 快照 checksum 并写入 lock。只有需要在 manifest 额外固定本地快照时才写 `dep.<name>.checksum`。运行快照只包含根 `ku.mod`（bare package 可无）和 `src/`，并递归排除 VCS、`.ku`、`target`、`node_modules`、`ku.lock`、`.env` 与 `.env.*`；未显式固定 checksum 时，快照变化会生成新的内容寻址 cache。

第三方作者使用 `ku package pack .` 生成确定性 `.tar.zst`；由交互输入或 CI secret store 设置 `KU_REGISTRY_TOKEN` 后运行 `ku package publish .`，不要把 token 字面量写进命令。问题版本只用 `ku package yank .` 单向撤回：签名 index 不再展示它，但不可变 artifact 和已有 lock 仍保留，不提供 delete/unyank。`ku package resolve .` 刷新兼容版本；消费者用 `--locked` 或 `--offline` 做可重复/无网络验收。协议见 [Registry API v1](docs/registry-api.md)。

仓库已提供可自行部署的有界参考服务 `ku-registry`，并用真实 TLS listener 验证作者 publish、消费者 resolve/check/run/native、locked/offline、ACL、幂等、冲突、并发竞争和重启恢复。它不是官方托管 registry，也没有生产吞吐量或“超级高并发”基准结论。联网解析、相关锁等待、分块校验和重试共享一个 300 秒绝对预算；单次内核文件 I/O 与同步 DNS 不能硬取消，但返回后会再次检查且超时后不会开始 cache quarantine/安装。依赖图最多 256 个包、求解最多 20000 步，同一 package cache 根的全进程全局下载槽固定为 8 个。artifact 只接受稳定、无 query 的 HTTPS `.tar.zst` URL；签名不匹配时 index 与 `.sig` 最多重新配对获取 3 次。部署配置和掉电持久性边界见 [Registry API v1](docs/registry-api.md)。

自托管 registry 已提供单一的离线治理路径：开发者 token、团队成员增删、包名认领/转移与 hash-chain 审计都通过 `ku-registry governance|developer|team|package|audit ...` 管理；token 由 OS 随机源生成，只保存 SHA-256，明文仅向 stdout 输出一次。旧 ACL 必须显式 `governance migrate`，迁移 token 保留原精确包 scope，不扩大权限。所有变更共享跨进程锁与同目录原子替换；运行中的服务不热加载，修改后必须重启。这里的“开发者/团队”是受信本机 operator 管理的自托管记录，不是在线注册/登录、外部不可抵赖审计或官方托管身份平台，完整命令、迁移和回滚边界见 [Registry API v1](docs/registry-api.md#启动自托管服务)。

## 示例

仓库内置示例在 `examples/`：

```txt
index.ku
hello.ku
main.ku
types.ku
constants.ku
template_string.ku
builtins.ku
fib.ku
loop.ku
function.ku
closure.ku
control_flow.ku
match.ku
arrays.ku
structs.ku
enum.ku
object.ku
mutation.ku
imports.ku
import_all.ku
math.ku
module.ku
compiler_pipeline.ku
result.ku
try_read.ku
stdlib.ku
v002_features.ku
package/
http_server.ku
http_bench.ps1
pg_demo.ku            # PostgreSQL:连接 + 参数化查询(native-only)
redis_demo.ku         # Redis:SET/GET/DEL,\r\n 值安全往返(native-only)
mysql_demo.ku         # MySQL:查询 + 参数化(native-only)
http_pg.ku            # HTTP + PostgreSQL 端到端 + 前端页(native-only)
http_pg_frontend.html # http_pg 的前端页面
```

> 数据库示例是 native-only(`ku build --native`),需要各自的连接凭据与运行时库,准备步骤见 [examples/README.md](examples/README.md)(凭据放 `db.conn`/`redis.pw`/`mysql.pw`,均已 gitignore)。

## 文档

- [语法文档](docs/syntax.md)
- [自举状态与 bootstrap 路线](docs/self-hosting.md)
- [并发模型与 HTTP 千万请求压测 demo](docs/concurrency.md)
- [0.0.16 版本记录](docs/v0.0.16.md)
- [0.0.15 版本记录](docs/v0.0.15.md)
- [0.0.14 版本记录](docs/v0.0.14.md)
- [0.0.13 版本记录](docs/v0.0.13.md)
- [0.0.12 版本记录](docs/v0.0.12.md)
- [0.0.11 版本记录](docs/v0.0.11.md)
- [0.0.10 版本记录](docs/v0.0.10.md)
- [0.0.9 版本记录](docs/v0.0.9.md)
- [0.0.8 版本记录](docs/v0.0.8.md)
- [0.0.7 版本记录](docs/v0.0.7.md)
- [Package 管理与 registry 客户端](docs/package.md)
- [IR 草案](docs/ir.md)
- [版本和解释器历史](docs/history.md)
- [待决策问题与路线草案](docs/roadmap-decisions.md)
- [0.0.6 版本记录](docs/v0.0.6.md)
- [0.0.5 版本记录](docs/v0.0.5.md)
- [0.0.4 版本记录](docs/v0.0.4.md)
- [0.0.3 版本记录](docs/v0.0.3.md)

## 当前边界

`ku build` 当前生成解释器打包型可执行文件。单文件默认输出到源文件旁的 `.ku/build/<profile>/<name>`；有 `ku.mod` 时可以直接 `ku build` 或 `ku build .`，入口来自 `root + main`，默认 `src/main.ku`，输出目录来自 `out`，默认 `.ku/build`。支持 `-o` 指定输出、`--debug` / `--release` / `--profile debug|release|small|fast`、`--target` 目标目录分层、`--emit-ir`、`--emit-c`、`--emit-llvm` 和 `--backend c`。

跨系统 native 发布只有一种用户写法：在对应目标系统，或具备匹配 target 编译器、sysroot 和动态库的构建环境中，从同一份源码分别执行 `ku build --backend c --release --target x86_64-windows .`、`ku build --backend c --release --target x86_64-linux .`、`ku build --backend c --release --target aarch64-darwin .`。只有各命令成功且产物校验通过后，才能得到三个独立产物；不存在同时兼容三个系统的“万能二进制”。默认输出分别位于 `.ku/build/<target>/release/`，只有 Windows target 自动加 `.exe`。IR/C/LLVM 中间产物统一位于 `.ku/build/[<target>/]<profile>/{ir,c,llvm}/<binary-stem>.<ext>`；Windows 的 `app.exe` 对应 `app.ir`、`app.c`、`app.ll`。显式 `-o` 时三类目录都会增加完整输出路径的 SHA-256 层，即 `{ir,c,llvm}/<output-path-sha256>/<binary-stem>.<ext>`。这样同目录多入口、不同目录同名输出都不共享中间产物，也不会覆盖用户输出目录里的 `.c` 文件。缺 compiler、sysroot 或目标库时链接明确失败并保留 C artifact，不会降级生成 host 二进制。显式 target 的链接结果会检查完整的 PE32+ x86_64、Linux ELF x86_64 或 macOS Mach-O arm64 头、表和可加载段；使用数据库时还会有界解析 PE import、ELF `DT_NEEDED` 或 Mach-O dylib command，要求最终产物实际动态导入 libpq 以及本次选定的 MySQL/MariaDB family，静态或跨 family 回退不会安装。数据库链接输入从已打开且验证过的句柄复制到随机、私有、`create_new` 的临时目录，编译器只接收该副本；临时目录由 RAII 清理，不再按名字扫描或删除用户输出目录中的文件。`KU_CC` 可指定 `zig cc`、Clang，或已经配置好目标的交叉 compiler；普通 host `cc/gcc` 不会被当作自动支持 `--target`。

动态依赖验证不等于把数据库 runtime 打进二进制。发布机器仍必须让产物依赖表中记录的 libpq/libmysql/libmariadb 及其传递依赖对系统 loader 可见，并满足构建时的 target ABI；MySQL 在进程启动后的首次 `client()` 还会执行 header/runtime family-major 握手。Windows MySQL 常见安装需要把同一安装根的 `lib` 与 `bin` 都加入 `PATH`，因为 OpenSSL 等传递 DLL 可能位于 `bin`。

注意：不带 `--native` 的默认 build 仍是解释器打包型二进制，会把入口源码嵌入 Rust wrapper；它用于稳定生成可运行 exe，不等价于 native ABI，带 import 的程序仍应保持源码依赖路径可访问。`ku build --native` 已通过本地 import graph 展开生成不依赖源码目录的 C/二进制，并已具备 `KuString`、array、dynamic object、Result/Error、closure/函数值以及 fs/json/time 的已实现 native ABI 子集；剩余边界见下段，不能把“ABI 主路径存在”解释成所有 payload、捕获形式和动态组合都已完成。

`ku build --native <file.ku>` 不带 `-o` 时保留旧的单文件兼容模式，只在源码旁写出 `.c`，不执行链接；`ku build --native -o <path> <file.ku>` 则进入与 `--backend c` 相同的生成、编译、链接和产物校验流程。普通跨系统发布使用上面的 `--backend c --target` 命令，避免把“只生成 C”误认为已经得到目标二进制。

native C 后端可用 MSVC 或匹配目标的 C 工具链编译独立二进制，覆盖 `int` / `bool` / `str`（正式 `KuString` owned ABI，支持拼接与 `str()`/`len`/`chars`/`contains`/`slice` 等方法）、struct（含数组/嵌套/enum 字段）、带长度和越界检查的 array、enum tag/payload、嵌套 match、基础控制流、统一 `KuError` / Result、`try/catch/finally`、闭包（env 引用计数）、native HTTP 服务以及数据库驱动（std.pg/redis/mysql）的已实现子集。array/named/Result/struct/闭包按默认 move、显式 `clone()`、自动 drop 生成所有权代码，并由 checker 做路径级 move 分析。核心同步 ABI 与 `std.fs/std.json/std.time` 的 C 路径面向 Windows/Linux/macOS；native HTTP 与 Redis 也已有 Windows Winsock、Linux/macOS POSIX socket/poll/pthread 分支，其中 Windows 已本地验证，Linux/macOS 仍待对应真机 CI 首次跑绿。`std.mysql` 目前只在 host build 自动配对 client library，显式 non-host target 会提前拒绝；三系统发布因此应在各目标系统分别构建。`std.pg` 构建必须通过绝对、专用的 `KU_PG_LIB` 目录提供匹配目标的 shared/import libpq；compiler/sysroot 还必须满足其传递依赖。仍明确报不支持的：动态 object 的部分复杂场景、从 dynamic object 取回闭包后调用、闭包捕获 struct/enum/Result/Task 等 owned 类型、async native lowering，以及 str 的 `trim`/`lower`/`upper`（需 Unicode 表）。

已完成到 0.0.15 的关键前置：

```txt
支持 str | int 这类联合类型，用于参数、变量和返回值检查。
支持 break / continue，并修复 for + continue 作用域弹出问题。
支持 `for i in 10` 非负整数迭代，语义为 `0 <= i < 10`。
支持单语句控制体：`if (ok) break`、`while (i < 10) i++`、`for i in 4 total += i`。
支持 `++i`、`--i`、`i++`、`i--` 和 `+=` / `-=` / `*=` / `/=` / `%=` 复合赋值。
支持位置解构赋值 a, b = 1, 2、对象解构赋值 `{ name, city: place, missing = fallback, ...rest } = obj` 和丢弃占位符 _。对象解构会消费右侧 object；要保留原对象时写 `obj.clone()`。
支持可选字段访问 user?.name。
支持带参数/返回类型的箭头函数，例如 `(a:int, b:int): int => a + b` 和 `x:int => x * 2`；函数保持第一公民。
支持数组链式 map：nums.map(x => x * 2)。
支持泛型函数：fn id<T>(value:T): T。
支持 string / array 标准库实例方法：text.trim()、items.try_get(0)?。
支持严格对象字符串键索引和字符串 int 索引：`object["name"]`、`text[0]`；对象缺键默认 panic，显式 `object["missing"]?` 返回可恢复的 `object/missing_key` 错误。只有 `object.get_or(key, default)` 是带默认值的宽松读取。
可恢复错误统一为 Error 对象：{ domain, code, message }，catch (err) 后使用 err.message / err.domain / err.code。
运行时闭包使用精确 capture map，不再把整个 Env 存进函数值。
IR 已有 ResultBranch / BindOk / JumpErr / PropagateErr。
native C 后端已有统一 Error 对象 ABI、复杂 Result payload 和 try/catch/finally。
package 已有 ku.mod、绝对 file:// 开发覆盖、确定性 pack、HTTPS publish/resolve、签名 index、传递依赖、有界回溯、portable registry ku.lock、完整性校验和 cache GC；绝对 file source 是显式的本地覆盖例外。registry 公钥由项目显式 pin；未配置 trust、签名不符、lock/cache 缺失或内容篡改都会 fail-closed。仓库提供 `ku-registry` 自托管有界参考实现并有真实 TLS 闭环测试；它不等于官方托管服务，也不据此声明生产并发能力。
async runtime 已有 blocking shutdown drain、累计指标和内部百万并发需求压力测试；开发者侧提供 HTTP 千万请求压测 demo。
仓库根目录的 `test.ku` 和 `run-test.ps1` 是 runtime 内部诊断入口，前者通过 `std.task` 打印百万并发需求测试的前后时间与 runtime 指标，后者额外采集进程 CPU、峰值内存和线程数。普通开发者示例使用 `examples/http_capacity_10m.ku`：业务代码只写 HTTP handler 和返回值，不直接管理 task；压测由 `examples/http_bench.ps1` 发起。
`std.time` 的 `time.now()` 返回 Unix epoch 毫秒整数，`time.steady_millis()` 提供进程内单调毫秒；Time object 由 `time.instant()` 创建，`time.elapsed(previous)` 计算到当前的毫秒差。日期、格式化、解析、时间段和固定偏移 zone API 继续使用 Time/Date/Duration object。
match 已修正 guarded wildcard 误判，并诊断重复未带 guard 的字面量分支。
match 支持嵌套 enum payload 模式、绑定、字面量和 `_` 的递归检查。
标准库可以用 `import { fs, http, time } from "std"` 一次导入多个模块。std.http 必须显式 import，当前提供 http.get/post/request，返回 `{ status, headers, body }` Response 对象；默认 client 复用连接，并提供 http.client/http.text/http.html/http.json/http.empty/http.redirect/http.status/http.statusText/http.service()/http.server() 配置与响应 helper。`http.text/html/json(body)` 默认 200，`http.text/html/json(status, body)` 显式协议状态码，`http.empty()` 默认 204，`http.redirect(location)` 默认 302；业务 `body.code/msg/data` 由开发者自己维护。必须用 `app = http.service()` 创建 HTTP service；旧的 `http.service` 属性式写法不再兼容。service.get/post/put/del(path, handler) 已支持注册路由并写入 service.routes，路径参数使用 `{id}`，`del` 对应 HTTP `DELETE`，不提供 `delete` 别名。handler 支持顶层函数名、`fn(){...}` 和 `fn(req){...}`；普通 handler 不读请求时写 `fn()`，读取请求时写 `fn(req)`，`_req` 只保留给适配器/测试 mock 等签名必须带参数但暂时不用它的场景；不允许第二个 `res/writer` 参数，也不允许 `res.write/res.end/reply.send/writer.write` 这类副作用式响应 API。handler 返回 `{ status, headers, body }` 或 `{ status, headers, body }!`，常规写法直接 `return http.text/json/html/empty/redirect(...)`。bind/listen 只接收 address，配置来自 service/server 对象，会先真实绑定端口并编译运行时路由表。解释器支持 `bind`、`listener.run` 与 `listener.close`；native C 三平台分支只支持会消费 service 句柄的阻塞式 `listen`，native 编译 `bind` 会提前报错。fs 需要 `import "std.fs"` 或 `import { fs } from "std"` 后使用，`read/write` 返回 Result，`try_read/try_write` 是同类型兼容别名。`json.parse` 返回 `KuValue!`，`json.stringify` 返回 `str!`。std.config 需要显式导入后使用，并提供 env/env_file/yaml 第一版配置读取。VS Code formatter 已支持 4 空格缩进、空行压缩、运算符/逗号空格、`++/--`、复合赋值和 `} else/catch/finally` 合并。
工具链新增 `ku create <name> --template <template>`、`ku init --template <template>` 和 `ku template list`；内置 basic/cli/http/json/fs/lib 模板。项目目录名允许大小写字母、数字、`_`、`-`，但 `ku.mod` 的 package `name` 仍保持小写。`ku run` / `ku check` 无参数时读取当前 `ku.mod` 的入口，带 `.ku` 文件路径时仍运行/检查指定文件。

`print(value)` 不自动换行；需要逐行输出时使用 `println(value)`。

HTTP 服务端示例和压测脚本：

```powershell
cargo run -- run examples\http_server.ku
powershell -ExecutionPolicy Bypass -File examples\http_bench.ps1 -Url http://127.0.0.1:8080/json -Requests 10000 -Concurrency 100
```
native C 输出会把 Ku main 改成 ku_main，并生成系统 int main(void) wrapper。
async fn 调用会立即启动一次性 task 句柄，必须显式返回 T!；await task? 等价于 (await task)?，并且 await 会消费 task，普通 task 只能 await 一次。
Ku 不提供 task.spawn、Task.new、runtime.schedule 或 thread.spawn；HTTP server 内部可以使用 task，但 handler 用户不需要手动管理。
async runtime 默认最多 1024 个 task；blocking worker 为 min(32, max(4, CPU 核心数))，blocking queue 最多 1024，超限返回结构化 task Err。
registry resolver 支持 exact/caret、签名传递依赖元数据和有界回溯；`check/run/build/package resolve` 已统一接入 HTTPS-only 获取、Ed25519 验签、SHA-256、受限 `.tar.zst`、内容树校验、内容寻址 cache 和单飞安装租约。联网解析、锁等待、重试与分块校验共用 300 秒绝对预算（同步 DNS/已进入内核的文件操作不能硬取消），依赖图上限 256、求解上限 20000 步；同一 package cache 根跨进程共享 8 个下载槽，index 与 `.sig` 最多获取 3 个完整配对。
LLVM 文本后端已支持非递归 struct 和基础/struct Result。
标准库 root import 允许小写导出，例如 `import { task, time } from "std"`；用户自定义文件的顶层 `fn/struct/enum` 仍必须首字母大写才对外导出。import/export 诊断会给出位置、问题描述和修改方向。
`std.time` 会拒绝超出 chrono 支持范围的毫秒值，不再静默回退到当前时间。
`ku.mod` 增加 `main`、`out`、`template` 和 `type` 字段，供 create/init/build 和库项目记录使用。
```

仍未完成：

```txt
LLVM array/enum、闭包和高级控制流 lowering
registry v2 自动 signed-roots、在线 key 吊销和透明轮换（v1 使用项目显式公钥 pin）
完整 match guard 模式矩阵和跨 guard 的穷尽性证明
native C 动态 object 的部分复杂场景、str 的 trim/lower/upper(需 Unicode 表)
native closure 捕获 struct/enum/Result/Task 等 owned 类型，以及从 dynamic object 取回闭包后调用
native async ABI / async 函数值
数据库驱动的解释器(`ku run`)支持；Redis/MySQL 新 client 的真实服务与三系统实测；Redis/MySQL 可强制证书与主机名验证的 TLS 配置
```

## VS Code 插件

插件目录：

```txt
editors/vscode-ku
```

统一打包解释器和 VS Code 插件：

```powershell
powershell -ExecutionPolicy Bypass -File scripts\package-release.ps1
powershell -ExecutionPolicy Bypass -File scripts\package-release.ps1 -InstallExtension
```

已提供：

```txt
Ku 语法高亮和 snippet(0.0.16 已加入 pg/redis/mysql 模块高亮与补全)
ku.mod / ku.lock 高亮
保存/打开时运行 ku check，并把错误放进 Problems 面板
命令面板：Run / Check / Show IR / Build / Build Native C / Package GC / Show Version
编辑器右上角 Run / Check / IR / Build 按钮
右键菜单：编辑器和文件列表里的 .ku 文件都会显示 Ku Run；没有 fn main() 时点击会提示
解释器查找：优先使用 PATH 里的 ku，找不到再回退到工作区 release/target
状态栏解释器版本检查
Hover、补全、定义跳转、Outline、Quick Fix、基础格式化
Ku 文件默认保存时格式化；import path 补全会替换引号内路径，避免 `std.std.fs`；成员补全会识别 `http.` / `fs.` / `json.` 等上下文，只插入成员名，避免 `http.http.server`。
```

> 说明:`editors/vscode-ku/ku-language-0.0.16.vsix` 已打包(含 pg/redis/mysql 高亮与补全、0.0.16 版本号)。需要自行重打时用 `npx @vscode/vsce package`(会先跑 `npm run compile`)。仓库同时保留了旧的 `ku-language-0.0.15.vsix`。

图形界面安装方式：VS Code 扩展页 `...` -> `Install from VSIX...`，选择 `editors/vscode-ku/ku-language-0.0.16.vsix`。

命令安装方式：

```powershell
code --install-extension editors\vscode-ku\ku-language-0.0.16.vsix --force
```

## 开发验证

```powershell
cargo test --no-fail-fast
cargo clippy --all-targets -- -D warnings
cargo build --release
```

更新本地解释器和历史版本快照：

```powershell
.\scripts\archive-release.ps1
```

该脚本会把 `target\release\ku.exe` / `libku.rlib` 同步到 `release\`，并归档到 `history\v当前版本\`。详细规则见 [版本和解释器历史](docs/history.md)。
