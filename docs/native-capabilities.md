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
| HTTP             | 同步及受支持的具名 async handler、持久连接、有界流水线与协作式超时；[HTTP 回归](../tests/native_http_test.rs)。                                                                  |
| 数据库           | native PostgreSQL、typed PG、Redis、MySQL client；连接池、结果独立寿命及既有清理合同见 [协议文档](protocol-foundation.md)。不自动重试业务 SQL。                                  |
| 网络与 TLS       | owned bytes、move-only `net.client`、TLS client 和匹配目标的 TLS pack；[安装要求](cli-runtime-bundle.md)、[网络回归](../tests/native_net_test.rs)。                              |

只读借用不消费原值；借用结果不能逃出所有者寿命。资源释放必须覆盖返回、错误、取消和部分初始化。生成 C 的内部 helper 不是可供第三方任意并发调用的稳定 ABI。

当前源码支持同步 `time.unix()` 和 `time.unix(time.instant())`，返回 Unix 秒整数；传入 Time 时只读原值。毫秒转秒沿用解释器的向零截断规则，例如 `-1001` 毫秒转换为 `-1` 秒。普通动态对象不能替代 native Time。

同步时间接口还支持 `from_unix`、`from_millis`、`duration`、`add/sub/diff/compare`、`is_leap`、`days_in_month` 和整数／Duration `sleep`。Time 与 Duration 使用不同标签；转换和运算不消费输入，Duration 可以复制，差值可以为负。`duration`、`days_in_month`、`sleep` 沿用 Result 错误；裸值构造或运算失败沿用运行错误，并先退役当前 native frame 的 owner。`from_unix` 只检查乘法溢出，`from_millis` 另检查日期范围，两者不能互换。

当前源码的同步 native C 还支持 `date`、`datetime`、`format`、`parse`、`parts`、`weekday`。解释器与 native 共用锁定的 Chrono 日期引擎；构建时使用与 CLI、目标架构及源码匹配的 `native-calendar/v1/<target>/` 静态包，不调用解释器、Runner 或辅助进程。生成程序链接完成后不依赖该包、SDK、rustc 或 Ku 源码。

日期接口保持既有返回约定：`date()` 返回 Date，带参数的 `date` 及 `datetime/parse` 返回 Result；`format(Time)`、`parts(Time)` 和单参数 `weekday` 返回裸值，带格式／时区的重载返回 Result。裸值接口失败沿用运行错误，Result 接口保留 `time` 错误域。输入只读，字符串、Date 和 parts 对象独立持有结果，原值或临时引擎存储释放后仍可使用。`SSS` 表示三位毫秒数字；分隔点由格式字符串提供，例如 `ss.SSS` 生成 `00.123`。

时区仍限本地、UTC 与固定偏移，不新增命名时区数据库。以上是同步能力，不扩大 native Task 的时间接口或跨 `await` 类型范围。各平台实际运行结果按本批验证记录分别确认，不能以交叉编译代替执行。

同步数组增加 `concat`、`first` 和 `last` 的原生实现。`concat` 返回独立数组；`first/last` 的静态返回类型为 `T | null`，空数组返回 null，有值时结果与原数组生命周期独立。使用 `if (value != null)` 明确缩窄后再执行元素运算；它们不是 Result，不能用 `?` 解包。数组原值不被修改或消费，用户 borrowed 参数的适用范围不因此扩大。详见 [数组接口](syntax.md#125-array)。

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

普通原生 C 程序不使用 `libku.rlib`。TLS pack 必须与目标及 CLI 匹配；数据库客户端动态库和其依赖仍须在部署环境提供。详情见 [CLI／SDK](cli-runtime-bundle.md)。

递归 value 布局、object 解构、optional chaining、捕获 `for` 迭代绑定，以及尚未映射的标准库能力仍会明确拒绝。Task 参数、返回、捕获及 finally 继续受各自文档中的子集限制。遇到不支持能力或缺少依赖时应报错，不静默换成 Runner。

原生 C 成为源码默认入口，不表示所有解释器功能或性能目标均已完成。能力、所有权、平台、安装与真实项目仍按各自证据验收，独立性能修改的采用条件不变。`ku run` 与直接执行 `.ku` 的既有行为不因此改变。
