# Ku

Ku 是一个正在开发中的小型编程语言和解释器。当前版本是 `0.0.10`，重点是运行时精确闭包捕获、Result 显式 CFG、package lock 依赖记录和 native C 后端 if/while/int 子集。

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
ku version            Print version
ku -h | -help         Print help
```

`ku check` 会检查词法、语法和基础语义错误，并输出文件名、行号、列号和源码片段。

## 0.0.10 支持的核心语法

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
        message = "caught: " + err
    } finally {
        print("cleanup")
    }

    print(message)
}
```

当前基础类型固定为：

```txt
int
float
bool
str
null
```

Ku 不使用 `let` / `let mut`。首次赋值即声明变量，带类型写作 `name:type = value`。

## 模块导入

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

有 `ku.mod` 时可以固定本地 package import root：

```txt
name = "demo_pkg"
version = "0.1.0"
root = "src"
cache = ".ku/cache"
```

```ku
import { Value } from "util"
```

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

- [0.0.10 语法草案](docs/syntax.md)
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

`ku build --native` 当前输出 prototype C 源码，只覆盖 int/bool/str、局部变量、直接函数调用、print、return、if 和 while。复杂语法会清楚报不支持，还不是完整 native C / LLVM 后端。

暂未完成：

```txt
async / await
完整 native C / LLVM 后端
复杂嵌套模式、match guard 模式矩阵和完整穷尽性检查
远程 package、版本解析、下载校验和缓存淘汰
```

已评估但暂不完整进入 0.0.10 的能力：

```txt
包管理：已有 ku.mod/import root/cache/version/ku.lock 依赖列表和 import cache key；远程包、版本解析、下载校验和缓存淘汰还没有做。
async / await：需要事件循环或任务模型，不能只加关键字。
native C / LLVM 后端：已有 typed temp IR、layout table、stdlib ABI metadata、Result 显式 CFG 和 prototype C 源码输出；完整 Result、闭包、struct/enum ABI 和 LLVM 还没有做。
引用捕获闭包：运行时已改成 capture map，递归 self binding 不再保存整个 Env；后续如果做跨线程/异步闭包，再评估 Weak/arena 模型。
match 穷尽性：enum 顶层 variant 已做基础穷尽性检查；复杂嵌套模式和 guard 模式矩阵还没有做。
```

## 开发验证

```powershell
cargo test --no-fail-fast
cargo clippy --all-targets -- -D warnings
cargo build --release
```
