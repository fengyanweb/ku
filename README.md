# Ku

Ku 是一个正在开发中的小型编程语言和解释器。当前版本是 `0.0.12`，重点是嵌套 `match` 模式、独立导入的 `std.http` 标准库雏形，以及更接近真实可执行入口的 native C 输出。

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
ku ir <file.ku>       Print checked Ku IR draft
ku build <file.ku>    Build a runnable executable wrapper
ku build --native <file.ku>
                      Emit prototype native C source
ku package gc <file.ku>
                      Remove unused package cache entries for a package
ku version            Print version
ku -h | -help         Print help
```

`ku check` 会检查词法、语法和基础语义错误，并输出文件名、行号、列号和源码片段。

## 0.0.12 支持的核心语法

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

    print(text)
    print(greet(user.name))
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
        print("cleanup")
    }

    print(message)
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
    print(lib.Format(user))
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
```

## 文档

- [0.0.12 语法文档](docs/syntax.md)
- [0.0.12 版本记录](docs/v0.0.12.md)
- [0.0.11 版本记录](docs/v0.0.11.md)
- [0.0.10 版本记录](docs/v0.0.10.md)
- [0.0.9 版本记录](docs/v0.0.9.md)
- [0.0.8 版本记录](docs/v0.0.8.md)
- [0.0.7 版本记录](docs/v0.0.7.md)
- [Package 草案](docs/package.md)
- [IR 草案](docs/ir.md)
- [版本和解释器历史](docs/history.md)
- [0.0.6 版本记录](docs/v0.0.6.md)
- [0.0.5 版本记录](docs/v0.0.5.md)
- [0.0.4 版本记录](docs/v0.0.4.md)
- [0.0.3 版本记录](docs/v0.0.3.md)

## 当前边界

`ku build` 当前生成解释器打包型可执行文件。

`ku build --native` 当前输出 prototype C 源码，覆盖 `int` / `bool` / `str`、局部变量、直接函数调用、`print`、`return`、`if`、`while`，以及基础 Result 的 `ok` / `err` / `?` / 错误传播。native C 的错误槽仍是原型字符串 ABI；数组、struct、enum、闭包、match、try/catch 的 native lowering 仍会明确报不支持。

已完成到 0.0.12 的关键前置：

```txt
支持 str | int 这类联合类型，用于参数、变量和返回值检查。
支持 break / continue，并修复 for + continue 作用域弹出问题。
支持位置解构赋值 a, b = 1, 2 和丢弃占位符 _。
支持可选字段访问 user?.name。
支持单参数箭头函数 x => x * 2。
支持数组链式 map：nums.map(x => x * 2)。
支持泛型函数：fn id<T>(value:T): T。
支持 string / array 标准库实例方法：text.trim()、items.try_get(0)?。
支持对象字符串键索引和字符串 int 索引：object["name"]、text[0]。
可恢复错误统一为 Error 对象：{ domain, code, message }，catch (err) 后使用 err.message / err.domain / err.code。
运行时闭包使用精确 capture map，不再把整个 Env 存进函数值。
IR 已有 ResultBranch / BindOk / JumpErr / PropagateErr。
native C 后端已有基础 Result ABI 子集，但还不是完整 Error 对象 ABI。
package 已有 ku.mod、file:// dependency、checksum、ku.lock 和 cache GC。
match 已修正 guarded wildcard 误判，并诊断重复未带 guard 的字面量分支。
match 支持嵌套 enum payload 模式、绑定、字面量和 `_` 的递归检查。
std.http 必须显式 import，当前提供 http.get/post/request，返回 `{ status, headers, body }` Response 对象。fs 需要 `import "std.fs"` 后使用，并提供 read/write 与 try_read/try_write。
native C 输出会把 Ku main 改成 ku_main，并生成系统 int main(void) wrapper。
```

仍未完成：

```txt
async / await
LLVM 后端
HTTP/registry package、真正语义版本求解、网络下载和强校验
完整 match guard 模式矩阵和跨 guard 的穷尽性证明
完整 native C 后端
```

## 开发验证

```powershell
cargo test --no-fail-fast
cargo clippy --all-targets -- -D warnings
cargo build --release
```
