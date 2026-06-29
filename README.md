# Ku

Ku 是一个正在开发中的小型编程语言和解释器。当前版本是 `0.0.13`，正在补齐二进制构建系统、native C、LLVM、registry resolver 和 async task 生命周期能力。

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
ku run examples/hello.ku
ku check examples/index.ku
```

## 命令

```txt
ku <file.ku>          Run a Ku source file
ku run <file.ku>      Run a Ku source file
ku check <file.ku>    Check a Ku source file without running
ku check --json <file.ku>
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
ku build --backend c [file.ku]
                      Build the native C prototype with a C compiler
ku build --native <file.ku>
                      Compatibility form: emit prototype native C source beside file
ku package gc <file.ku>
                      Remove unused package cache entries for a package
ku version            Print version
ku -h | -help         Print help
```

`ku check` 会检查词法、语法和基础语义错误，并输出文件名、行号、列号和源码片段。

## 0.0.13 支持的核心语法

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

有 `ku.mod` 时可以固定本地 package import root，也可以声明 `file://` 依赖：

```txt
name = "demo_pkg"
version = "0.1.0"
root = "src"
cache = ".ku/cache"

dep.util = "1.0.0"
dep.util.source = "file://C:/work/util"
dep.util.checksum = "ku-fnv64-..."
```

```ku
import { Value } from "util"
import { Value } from "@util/util"
```

`ku check` / `ku run` 会把 file dependency 放进 `<package>/.ku/cache/packages/<name>/<version>/`，并把 package dependency 写进 `ku.lock`。`ku package gc <file.ku>` 会清理 manifest 当前依赖之外的缓存版本。
`dep.<name>.checksum` 必须是 `ku-fnv64-` 加 16 位十六进制；未写 checksum 的 file dependency 会在 source 内容变化后刷新 cache。

## 示例

仓库内置示例在 `examples/`：

```txt
hello.ku
fib.ku
loop.ku
function.ku
arrays.ku
structs.ku
enum.ku
object.ku
imports.ku
compiler_pipeline.ku
result.ku
try_read.ku
stdlib.ku
package/
http_server.ku
http_bench.ps1
```

## 文档

- [0.0.13 语法文档](docs/syntax.md)
- [0.0.13 版本记录](docs/v0.0.13.md)
- [0.0.12 版本记录](docs/v0.0.12.md)
- [0.0.11 版本记录](docs/v0.0.11.md)
- [0.0.10 版本记录](docs/v0.0.10.md)
- [0.0.9 版本记录](docs/v0.0.9.md)
- [0.0.8 版本记录](docs/v0.0.8.md)
- [0.0.7 版本记录](docs/v0.0.7.md)
- [Package 草案](docs/package.md)
- [IR 草案](docs/ir.md)
- [版本和解释器历史](docs/history.md)
- [待决策问题与路线草案](docs/roadmap-decisions.md)
- [0.0.6 版本记录](docs/v0.0.6.md)
- [0.0.5 版本记录](docs/v0.0.5.md)
- [0.0.4 版本记录](docs/v0.0.4.md)
- [0.0.3 版本记录](docs/v0.0.3.md)

## 当前边界

`ku build` 当前生成解释器打包型可执行文件。单文件默认输出到源文件旁的 `.ku/build/<profile>/<name>`；有 `ku.mod` 时可以直接 `ku build` 或 `ku build .`，入口来自 `root + main`，默认 `src/main.ku`，输出目录来自 `out`，默认 `.ku/build`。支持 `-o` 指定输出、`--debug` / `--release` / `--profile debug|release|small|fast`、`--target` 目标目录分层、`--emit-ir`、`--emit-c`、`--emit-llvm` 和 `--backend c` 原型。`ku run build` 保留兼容，但会提示改用 `ku build`。

注意：0.0.13 的默认 build 是“解释器打包型二进制”，会把入口源码嵌入 Rust wrapper；它用于稳定生成可运行 exe，不等价于最终 native ABI。带 import 的程序仍应保持源码依赖路径可访问。完整 native binary 目标仍在执行队列：native closure、正式 `KuString`、dynamic object、async state machine runtime、增量缓存和真正不依赖源码的 import graph 打包。

`ku build --native` 当前输出 prototype C 源码，覆盖 `int` / `bool` / `str`、非递归 struct、带长度和越界检查的 array、enum tag/payload、嵌套 match、基础控制流、统一 `KuError` / Result、`try/catch/finally` 和 return-through-finally。array/named/Result 已按默认 move、显式 `clone()`、自动 drop 生成所有权代码；闭包、动态 object、正式 owned string 和 async native lowering 仍会明确报不支持。

已完成到 0.0.13 的关键前置：

```txt
支持 str | int 这类联合类型，用于参数、变量和返回值检查。
支持 break / continue，并修复 for + continue 作用域弹出问题。
支持 `for i in 10` 非负整数迭代，语义为 `0 <= i < 10`。
支持单语句控制体：`if (ok) break`、`while (i < 10) i++`、`for i in 4 total += i`。
支持 `++i`、`--i`、`i++`、`i--` 和 `+=` / `-=` / `*=` / `/=` / `%=` 复合赋值。
支持位置解构赋值 a, b = 1, 2 和丢弃占位符 _。
支持可选字段访问 user?.name。
支持带参数/返回类型的箭头函数，例如 `(a:int, b:int): int => a + b` 和 `x:int => x * 2`；函数保持第一公民。
支持数组链式 map：nums.map(x => x * 2)。
支持泛型函数：fn id<T>(value:T): T。
支持 string / array 标准库实例方法：text.trim()、items.try_get(0)?。
支持严格对象字符串键索引和字符串 int 索引：`object["name"]`、`text[0]`；对象缺键默认报错，显式 `object["missing"]?` 才返回 `null`。
可恢复错误统一为 Error 对象：{ domain, code, message }，catch (err) 后使用 err.message / err.domain / err.code。
运行时闭包使用精确 capture map，不再把整个 Env 存进函数值。
IR 已有 ResultBranch / BindOk / JumpErr / PropagateErr。
native C 后端已有统一 Error 对象 ABI、复杂 Result payload 和 try/catch/finally。
package 已有 ku.mod、file:// dependency、checksum、ku.lock 和 cache GC。
registry 执行层已有 HTTPS-only 下载、SHA-256、内容寻址 cache 和有界安装锁；签名/归档决策前 CLI 保持 fail-closed。
async runtime 已有 blocking shutdown drain、累计指标和百万并发需求压力测试。
仓库根目录提供 `test.ku` 和 `run-test.ps1`：前者通过 `std.task` 打印百万并发需求测试的前后时间与 runtime 指标，后者额外采集进程 CPU、峰值内存和线程数。
`std.time` 已按第一版文档实现 Time/Date/Duration object、format/parse/date/datetime/duration/add/sub/diff/compare/parts/weekday/is_leap/days_in_month/sleep 和固定偏移 zone。
match 已修正 guarded wildcard 误判，并诊断重复未带 guard 的字面量分支。
match 支持嵌套 enum payload 模式、绑定、字面量和 `_` 的递归检查。
标准库可以用 `import { fs, http, time } from "std"` 一次导入多个模块。std.http 必须显式 import，当前提供 http.get/post/request，返回 `{ status, headers, body }` Response 对象；默认 client 复用连接，并提供 http.client/http.text/http.json/http.service/http.server 配置与响应 helper。service.get/post/put/del(path, handler) 已支持注册路由并写入 service.routes，路径参数使用 `{id}`；handler 固定 `(req, res)`，返回 `{ status, headers, body }`，并禁止修改外层捕获变量；bind/listen 只接收 address，配置来自 service/server 对象，会先真实绑定端口并编译运行时路由表，listen/run 会阻塞处理基础 HTTP 请求，listener.close 可显式关闭未运行的 listener。fs 需要 `import "std.fs"` 或 `import { fs } from "std"` 后使用，并提供 read/write 与 try_read/try_write。std.config 需要显式导入后使用，并提供 env/env_file/yaml 第一版配置读取。VS Code formatter 已支持 4 空格缩进、空行压缩、运算符/逗号空格、`++/--`、复合赋值和 `} else/catch/finally` 合并。

`print(value)` 不自动换行；需要逐行输出时使用 `println(value)`。

HTTP 服务端示例和压测脚本：

```powershell
cargo run -- run examples\http_server.ku
powershell -ExecutionPolicy Bypass -File examples\http_bench.ps1 -Url http://127.0.0.1:8080/json -Requests 10000 -Concurrency 100
```
native C 输出会把 Ku main 改成 ku_main，并生成系统 int main(void) wrapper。
async fn 调用会立即启动 task，必须显式返回 T!；await task? 等价于 (await task)?。
async runtime 默认最多 1024 个 task；blocking worker 为 min(32, max(4, CPU 核心数))，blocking queue 最多 1024，超限返回结构化 task Err。
task.status/cancel/await_timeout 已实现；取消是协作式的，等待超时不会隐式取消目标任务。
registry resolver 支持精确版本和 caret 范围、最高兼容版本选择和冲突诊断；HTTPS-only 获取、SHA-256、内容寻址 cache 和安装锁已实现，签名信任根、归档格式、受限解包和 CLI 远程 import 串联前仍保持 fail-closed。
LLVM 文本后端已支持非递归 struct 和基础/struct Result。
标准库 root import 允许小写导出，例如 `import { task, time } from "std"`；用户自定义文件的顶层 `fn/struct/enum` 仍必须首字母大写才对外导出。import/export 诊断会给出位置、问题描述和修改方向。
`std.time` 会拒绝超出 chrono 支持范围的毫秒值，不再静默回退到当前时间。
`ku.mod` 增加 `main` 和 `out` 字段，供 `ku build` 解析项目默认入口和输出目录。
```

仍未完成：

```txt
LLVM array/enum、闭包和高级控制流 lowering
registry 签名信任根、归档格式、受限解包和 CLI 远程 import 串联
完整 match guard 模式矩阵和跨 guard 的穷尽性证明
native C 闭包、动态 object、正式 owned string ABI
native async ABI
```

## VS Code 插件

插件目录：

```txt
editors/vscode-ku
```

已提供：

```txt
Ku 0.0.13 语法高亮和 snippet
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

图形界面安装方式：VS Code 扩展页 `...` -> `Install from VSIX...`，选择：

```txt
editors/vscode-ku/ku-language-0.0.13.vsix
```

命令安装方式：

```powershell
code --install-extension editors\vscode-ku\ku-language-0.0.13.vsix --force
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
