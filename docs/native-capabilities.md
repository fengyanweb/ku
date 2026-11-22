# Native C 能力与平台

[English](native-capabilities.en.md)

原生 C 构建使用目标 C 编译器生成独立程序，不通过 Runner 回读 Ku 源码。当前源码的 `ku build` 默认选择原生 C；已发布的 0.0.20 安装包仍默认选择 Runner。跨版本使用显式后端命令：

```sh
ku build --backend c --release hello.ku -o hello
ku build --backend runner --release hello.ku -o hello-runner
```

生成 C、完成链接和在目标机器运行是三个不同步骤。`ku build --native hello.ku` 不带 `-o` 的兼容用法只生成 C。

## 已有能力

下表限定原生后端的实际范围，不表示解释器的全部功能都可原生构建。回归入口供核对具体行为；已有测试不代表每次开发组合都已完成平台验收。

| 能力             | 范围与回归                                                                                                                                                                       |
| ---------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 类型与运算       | 已检查的整数、浮点、布尔、null、字符串和比较；[整数边界](../tests/native_sync_integer_matrix_test.rs)。                                                                          |
| 容器与自定义类型 | 数组、动态对象、tagged 动态值、非递归布局的 struct／enum、复杂 Result；[容器](../tests/native_collection_test.rs)、[聚合值寿命](../tests/native_sync_aggregate_owners_test.rs)。 |
| 函数与模块       | 函数值、捕获闭包、模块导入与现有泛型具体化；[闭包释放](../tests/native_closure_drop_test.rs)、[泛型导入](../tests/native_generic_import_test.rs)。                               |
| 所有权与错误     | owned 值、只读借用、显式 clone、`?` 与 `try/catch/finally`；[借用](../tests/borrow_native_test.rs)、[退出清理](../tests/native_cleanup_escape_test.rs)。                         |
| 文件与时间       | `std.fs`／`std.time` 原生实现及有界目录读取；[文件、时间与 JSON 回归](../tests/native_run_test.rs)。                                                                             |
| JSON             | 同步 DOM／typed 接口和既有默认引擎；Task 局部 JSON 可跨 `await`，具体范围见 [Task JSON](native-task-json.md)。                                                                   |
| Task             | 受限的源码 Task、挂起恢复、计时器、取消、期限与资源账本；[并发合同](concurrency.md)、[Task 回归](../tests/native_task_source_test.rs)。这不是完整 async／await 或完整 M:N。      |
| HTTP             | 同步／Task 客户端、同步及受支持的具名 async handler、持久连接、有界流水线与协作式超时；[服务端回归](../tests/native_http_test.rs)、[客户端及响应回归](../tests/native_http_client_test.rs)。 |
| 数据库           | native PostgreSQL、typed PG、Redis、MySQL client；连接池、结果独立寿命及既有清理合同见 [协议文档](protocol-foundation.md)。不自动重试业务 SQL。                                  |
| 网络与 TLS       | owned bytes、move-only `net.client`、TLS client 和匹配目标的 TLS pack；[安装要求](cli-runtime-bundle.md)、[网络回归](../tests/native_net_test.rs)。                              |

只读借用不消费原值；借用结果不能逃出所有者寿命。资源释放必须覆盖返回、错误、取消和部分初始化。生成 C 的内部 helper 不是可供第三方任意并发调用的稳定 ABI。

当前源码支持同步 `time.unix()` 和 `time.unix(time.instant())`，返回 Unix 秒整数；传入 Time 时只读原值。毫秒转秒沿用解释器的向零截断规则，例如 `-1001` 毫秒转换为 `-1` 秒。普通动态对象不能替代 native Time。

同步时间接口还支持 `from_unix`、`from_millis`、`duration`、`add/sub/diff/compare`、`is_leap`、`days_in_month` 和整数／Duration `sleep`。Time 与 Duration 使用不同标签；转换和运算不消费输入，Duration 可以复制，差值可以为负。`duration`、`days_in_month`、`sleep` 沿用 Result 错误；裸值构造或运算失败沿用运行错误，并先退役当前 native frame 的 owner。`from_unix` 只检查乘法溢出，`from_millis` 另检查日期范围，两者不能互换。

当前源码的同步 native C 还支持 `date`、`datetime`、`format`、`parse`、`parts`、`weekday`。解释器与 native 共用锁定的 Chrono 日期引擎；构建时使用与 CLI、目标架构及源码匹配的 `native-calendar/v1/<target>/` 静态包，不调用解释器、Runner 或辅助进程。生成程序链接完成后不依赖该包、SDK、rustc 或 Ku 源码。

日期接口保持既有返回约定：`date()` 返回 Date，带参数的 `date` 及 `datetime/parse` 返回 Result；`format(Time)`、`parts(Time)` 和单参数 `weekday` 返回裸值，带格式／时区的重载返回 Result。裸值接口失败沿用运行错误，Result 接口保留 `time` 错误域。输入只读，字符串、Date 和 parts 对象独立持有结果，原值或临时引擎存储释放后仍可使用。`SSS` 表示三位毫秒数字；分隔点由格式字符串提供，例如 `ss.SSS` 生成 `00.123`。

时区仍限本地、UTC 与固定偏移，不新增命名时区数据库。以上是同步能力，不扩大 native Task 的时间接口或跨 `await` 类型范围。平台与工具链要求见下文。

同步数组增加 `concat`、`first` 和 `last` 的原生实现。`concat` 返回独立数组；`first/last` 的静态返回类型为 `T | null`，空数组返回 null，有值时结果与原数组生命周期独立。使用 `if (value != null)` 明确缩窄后再执行元素运算；它们不是 Result，不能用 `?` 解包。数组原值不被修改或消费，用户 borrowed 参数的适用范围不因此扩大。Task 内按已有平面 int／bool／str 数组范围支持这三项接口，owned 字符串结果可跨 `await`；不扩大为 Task 中任意嵌套容器或自定义结构体。详见 [数组接口](syntax.md#125-array)。

## 当前源码新增能力

以下能力已完成 Linux x86_64、macOS Apple Silicon 和 Windows x86_64 的正常 C11 O2 功能回归及真实 CLI／SDK 验证。它们属于当前源码，不包含在原公开 0.0.20 安装包中。

| 能力                                       | 当前验证入口                                                           |
| ------------------------------------------ | ---------------------------------------------------------------------- |
| HTTP 状态常量、独立常量对象及 `statusText` | [状态常量回归](../tests/native_http_status_test.rs)。                  |
| Unicode `trim`／`lower`／`upper`           | [Unicode、借用与中断回收](../tests/native_unicode_string_test.rs)。    |
| 同步嵌套循环的 `break`／`continue`         | [finally 次序、循环回收与期限](../tests/native_loop_control_test.rs)。 |
| 可选 Zstandard、gzip、DEFLATE 接口         | [输出上限、损坏输入与释放](../tests/native_compression_test.rs)。      |

验证覆盖功能、限额、损坏输入、所有权和失败回收，不代表性能目标或长期负载验收通过。受支持 Task 的循环控制已有独立实现；同步循环的修改不扩大 Task 类型或 finally 子集。

## 平台与部署

0.0.20 发行目标为 Linux x86_64 GNU、macOS Apple Silicon 和 Windows x86_64 MSVC。已有 Linux ARM64 构建／链接检查另行保留，不等于已经提供该架构的发行包。每个平台均需匹配的 C 工具链、系统库及程序使用的可选依赖；跨平台生成成功不等于目标运行通过。

当前源码的安装后验证覆盖 Linux x86_64 GNU、macOS Apple Silicon，以及 Windows x86_64 的 MSVC 和 GNU／MinGW 两种目标。Windows GNU 需要匹配的 MinGW-w64 工具链；Ku CLI 静态链接其 GNU 运行库，程序显式使用的第三方 DLL 仍需按依赖部署。Linux ARM64 仍只有上述单独记录的构建／链接覆盖。

Windows GNU 的安装验证还覆盖普通／typed PostgreSQL 的参数、Unicode、NULL、strict reset 和结果独立寿命。部署时需提供匹配的 libpq 导入库及 DLL 依赖，不能依赖开发机或宽泛的 MSYS2 PATH；GNU／MinGW 尚非当前正式发行目标。

普通原生 C 程序不使用 `libku.rlib`。TLS、日期和 HTTP 静态包须与目标及 CLI 匹配，并仅在程序使用对应能力时参与链接；数据库客户端动态库和其依赖仍须在部署环境提供。详情见 [CLI／SDK](cli-runtime-bundle.md)。

同步 native C 支持普通对象解构、`?.field` 和 `for` 迭代变量捕获。解构移交字段所有权，存在的 null 不触发默认值；可选字段保留 null，接收值只求值一次；循环捕获在每次迭代建立独立绑定。Task 的对象操作、参数、返回、捕获及 finally 仍受现有子集限制，递归布局和未知调用 ABI 会明确报错。

同步 native C 可以读取局部对象字面量的已知标量字段，也可以用同类型的标量表达式更新固定字段，例如 `item.title = replacement`。适用对象须留在当前函数内，并通过固定字段名读写。读出的字符串由调用方独立持有，覆盖字段后先前读出的值仍可使用。见[读写局部对象字段的示例](../tests/fixtures/native_static_object_fields/main.ku)。

原生 C 成为源码默认入口，不表示所有解释器功能或性能目标均已完成。能力、所有权、平台、安装与真实项目仍按各自证据验收，独立性能修改的采用条件不变。`ku run` 与直接执行 `.ku` 的既有行为不因此改变。

## HTTP 客户端与路由

`http.get(url)`、`http.post(url, body)` 和 `http.request(config)` 在同步原生程序中返回 `HttpResponse!`。`HttpResponse` 可用于函数参数和返回值；`status`、`headers`、`body` 与解释器一致。结果独立持有字符串和头表，连接复用后仍有效。`clone()` 返回独立副本；读取 `response.headers` 得到独立表，修改 `response.headers[key]` 则修改该响应。JSON 和打印只暴露这三个公开字段。

同步 HTTP 路由支持显式 `HttpResponse` 返回值，以及可静态确定为响应对象的结构化返回值；对应的 Result 返回同样适用。先写错误分支不再使成功响应的类型推导错误。处理函数仍只执行一次，Result 错误保持原有处理，响应数据按所有权规则移交或复制。

同步路由可直接注册零参数、一个 owned `HttpRequest`，或一个只读 `&req: HttpRequest` 参数的处理器。View 仅在本次同步调用期间有效；注册不会深复制整个请求，也不会转移其所有权。async 处理器仍持有 owned 请求，不能声明跨等待的 borrowed 参数。动态响应值须具有合法的 `status`、字符串头表和字符串 `body`；运行时检查通过后才移交发送路径。无法确定真实函数签名的返回方式仍在构建时明确拒绝。

非 2xx 状态仍是成功结果；网络、UTF-8、响应体上限等错误通过 Result 返回。`http.request` 默认总请求期限 5000 ms、响应体上限 1,000,000 B，最大分别为 300,000 ms 和 16 MiB。已有更早的 HTTP handler 期限会限制本次调用，重定向不续期；调用返回后仍执行原有期限检查和清理。同步 DNS／网络等待不能保证任意时刻被取消。native Task 中的直接 `http.get/post/request` 使用可挂起的网络推进：DNS 使用既有有界后台机制，连接、TLS、发送和接收等待就绪事件，阶段切换不重置原期限。取消后的迟到结果只做清理，不恢复用户代码；已经开始的系统 DNS 查询须返回后才能物理退役。普通同步 wrapper 不会被悄悄放到 Task 线程中阻塞执行。

`http.client(config)` 保留现有配置对象接口，不是单独的连接池句柄；短方法复用现有懒初始化共享 agent。只有使用网络方法的程序才链接目标匹配的 `native-http/v1/<target>` 静态包。开发构建可用绝对路径 `KU_NATIVE_HTTP_CLIENT_PACK` 指定对应包；缺包、目标或源码身份不匹配会明确失败，不回退 Runner。

完整回环示例：[native_http_client.ku](../examples/native_http_client.ku)。运行前，请先在 `127.0.0.1:8080` 启动按示例约定提供 `/hello`、`/post` 和 `/missing` 的本地 HTTP 服务。安装包验收同时链接日期和 HTTP 静态库，并在源码和 SDK 不可用时运行生成程序。各目标仍分别记录实际安装运行结果，交叉编译不等于运行通过。

Task 的 `http.request` 支持字面量和局部配置对象，字符串 header 表也可以先绑定。字段值可以动态计算，`method/body/timeout_ms/max_body_bytes` 的 null 保留默认含义。调用只读配置，配置可重复使用。局部对象可持有受支持的 nullable 标量和嵌套对象。仅返回由标量参数和字面量组成的对象的简单构造函数可在 Task 中调用，实参按顺序各求值一次。任意同步函数、整个对象跨 Task 参数和结果，以及一般捕获仍不在支持范围。非法配置保持运行错误，网络、UTF-8 和正文上限等操作错误保持 Result。请求不会因网络失败而自动重试；重定向和连接复用遵守既有客户端规则。

所有变体都没有载荷的具名枚举可以作为 owned Task 实参，也可以通过 `Phase!` 这样的单层 Result 从子 Task 返回。具名 Task 结构体也可以携带这种枚举字段。相同枚举类型的值可以用 `==` 和 `!=` 比较，比较后两个操作数仍可读取。

native Task 可以用 `match` 匹配无载荷枚举的变体，也可以用末尾无 guard 的 `_` 分支。各分支产生同一种 `int`、`bool`、`null` 或 owned `str`。匹配对象只求值一次。变体的 tag 匹配后才求值布尔 guard。guard 为 false 时继续匹配同一变体的后续分支或末尾通配分支。guard 和选中的分支都可以挂起，也可以通过 `?` 传播错误。若后续分支仍要使用某个 owned 值，可给 guard 中的 owned 调用传入该值的 `.clone()`。

如果具名枚举最多有 64 个变体，只有一个变体携带单个 `int`，其余变体均无载荷，也可以通过单层 Result 返回。无 guard 的 `match` 可以用 `ScanStatus.Found(index)` 读取整数，也可以用 `ScanStatus.Found(_)` 跳过它。读取整数后仍可再次匹配原枚举值。

在同一变体数量限制下，Task 结果也支持只有一个变体携带单个 owned `str`。对 `match (await Scan())?` 这样的临时值执行无 guard 的 `match` 时，字符串移交给选中的分支，后续挂起后仍然有效。

对具名枚举执行无 guard 的 `match` 时，选中分支可以直接返回字符串绑定，也可以调用绑定的 `.len()` 或 `.byte_len()`。这些绑定会消费原枚举。用 `_` 忽略该字段时，仍可再次匹配原枚举值。

Task 可以读取响应的 `body/status`，读取或写入 `response.headers[key]`，并将 owned 响应跨等待保存或返回。动态 header 的字符串类型、名称、值、数量、字节和传输字段仍由既有 HTTP 校验约束；分配计入 Task 预算。非法响应不会使后续合法请求失效。这不扩展为任意 Task 对象、结构体或嵌套容器。

Task 客户端示例：[native_task_http_client.ku](../examples/native_task_http_client.ku)。端到端服务示例：[native_http_relay.ku](../examples/native_http_relay.ku)。先在 `127.0.0.1:8080/hello` 提供本机上游，再构建并运行服务示例；`/info` 使用只读处理器，`/relay` 通过 Task 请求上游，并返回动态 header。
