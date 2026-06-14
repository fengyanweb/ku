# Ku 0.0.10 Syntax Draft

本文固定 Ku 0.0.10 的语法边界。当前 CLI 版本显示 `0.0.10`。

## 文件和入口

Ku 源文件使用 `.ku` 扩展名。程序必须提供无参数入口：

```ku
fn main() {
    print("Hello Ku")
}
```

当前不支持顶层脚本语句。

## 顶层声明

当前支持：

```ku
module demo

struct Token {
    kind: str
    text: str
    line: int
    column: int
}

enum TokenKind {
    Ident
    Number
    Eof
}

fn main() {
    print("ok")
}
```

首字母大写的顶层 `fn` / `struct` / `enum` 会作为跨文件导出名；首字母小写的顶层名字只在当前文件内部使用。

支持三种导入形式：

```ku
import math from "./math"
import { Add, Twice } from "./math.ku"
import "./math.ku"
```

`import math from "./math"` 会把 `./math.ku` 中导出的函数、结构体和 enum 放进命名空间，通过 `math.Add(1, 2)`、`math.User { ... }`、`math.State.Ready` 使用。

`import { Add, Twice } from "./math.ku"` 会按需导入导出名，直接使用 `Add(1, 2)`。

`import "./math.ku"` 会全量导入该文件所有首字母大写的导出名，直接使用。`import from "./math.ku"` 不属于 Ku 语法。

## 类型

基础类型：

```txt
int
float
bool
str
null
```

数组类型：

```ku
nums:[int] = [1, 2, 3]
names:[str] = ["Ku"]
```

结构体类型直接使用结构体名：

```ku
token:Token = Token { kind: "Ident", text: "name", line: 1, column: 1 }
```

可恢复错误类型：

```ku
fn load(): str! {
    return ok("ready")
}
```

`T!` 表示 `Result<T, str>`。当前固定错误 payload 为 `str`。

`string` 和 `nil` 不是 0.0.10 类型名。

## 变量

Ku 不使用 `let` / `let mut`。

```ku
name = "Ku"           // 首次赋值即声明，默认可变
score = 100
title:str = "hello"   // 带类型声明
count:int             // 默认值声明
```

全大写或全大写加下划线的名字按常量处理：

```ku
APP_NAME = "Ku"
```

常量后续不能再次赋值。

## 函数

普通函数参数必须写类型，返回类型可选：

```ku
fn add(a: int, b: int): int {
    return a + b
}

fn log(message: str) {
    print(message)
}
```

函数可以写在顶层，也可以写在函数内部作为局部函数：

```ku
fn greet(name: str) {
    print(`Hello {name}`)
}

fn main() {
    fn go(name: str, age: int) {
        print(`我是{name},我{age}岁了`)
    }

    greet("Ku")
    go("Ku", 3)
}
```

限制：

- `main` 不能有参数。
- 同一个函数内不能有重复参数名。
- 带返回类型的函数必须有可见 `return`。
- 返回值类型必须和声明匹配。
- 局部函数按引用捕获外层局部变量，读取时能看到最新值，赋值会写回外层可变变量。

## 函数值

函数值使用箭头语法：

```ku
fn main() {
    add = (a, b) => {
        return a + b
    }

    print(add(10, 20))
}
```

限制：

- 箭头函数参数暂时不写类型。
- 箭头函数按引用捕获外层局部变量，读取时能看到最新值，赋值会写回外层可变变量。
- 函数值调用会按实参类型检查函数体并推导返回类型。
- 0.0.10 的 IR 和运行时都使用精确 capture map。函数值只保存实际自由变量的共享绑定；递归局部函数在调用时临时注入 self binding，不再把整个外层 `Env` 保存进函数值。

## 条件和循环

```ku
if (age >= 18) {
    print("adult")
} else {
    print("child")
}
```

```ku
i = 0

while (i < 5) {
    print(i)
    i = i + 1
}
```

```ku
nums:[int] = [1, 2, 3]
for n in nums {
    print(n)
}
```

`if` 和 `while` 条件必须带小括号，并且条件类型必须是 `bool`。`for` 当前只遍历数组。

## 结构体

结构体是数据容器，当前不支持方法、继承或泛型：

```ku
struct Token {
    kind: str
    text: str
    line: int
    column: int
}

fn main() {
    token = Token { kind: "Ident", text: "name", line: 1, column: 1 }
    print(token.kind)
}
```

结构体字面量必须提供全部字段，不能写不存在的字段。

结构体字段可以赋值，字段必须存在且类型必须匹配：

```ku
token.kind = "Number"
```

## 枚举

当前支持 enum 声明、无 payload 变体值和 payload 构造：

```ku
enum TokenKind {
    Ident
    Number
    Eof
}

enum Expr {
    Number(value: int)
    Text(value: str)
}

fn main() {
    kind = TokenKind.Ident
    expr = Expr.Number(1)
    print(kind)
    print(expr)
}
```

payload 构造使用 `EnumName.Variant(args...)`。

## Match / Switch

`match` 和 `switch` 是表达式，当前支持字面量、`_` 和 enum variant 模式：

```ku
text = match Expr.Number(7) {
    Expr.Number(value) => str(value)
    Expr.Text(value) => value
    _ => "none"
}

label = switch 2 {
    1 => "one"
    _ => "other"
}
```

对 enum 做 `match` 时，如果没有 `_`，必须覆盖所有未带 guard 的 variant。带 guard 的分支不计入穷尽覆盖；复杂嵌套模式和完整 guard 模式矩阵后续再补。非 enum 值当前没有完整穷尽性检查，没有匹配分支会在运行时报错。

## 数组

数组是同质数组：

```ku
nums:[int] = [1, 2, 3]
print(nums[0])
print(len(nums))
```

当前支持读取和写入索引：

```ku
nums[0] = 9
```

## 对象字面量

对象字面量用于临时组织键值数据，语法接近 JSON，但它是 Ku 表达式，不是 JSON 文本：

```ku
person = { name: "张三", age: 18 }
print(person.name)
print(person.age)
```

字段名可以是标识符或字符串。字符串值必须加引号，例如 `{ name: "张三" }`；`{ name: 张三 }` 会被当成变量读取。

对象字段可以赋值，字段必须已经存在：

```ku
person.age = 19
```

## 表达式

支持的表达式：

```txt
字面量:       123, 1.5, true, false, null, "text", 'text', `hello {name}`
数组:         [1, 2, 3]
对象:         { name: "Ku", age: 1 }
结构体:       Token { kind: "Ident", text: "name", line: 1, column: 1 }
变量:         name
字段:         token.kind, fs.read
索引:         nums[0]
match:        match value { _ => "ok" }
分组:         (expr)
一元:         -x, !ok
二元:         +, -, *, /, %, ==, !=, <, <=, >, >=, &&, ||
调用:         add(1, 2), len("Ku"), fs.read("main.ku")
函数值:       (a, b) => { return a + b }
```

优先级从高到低：

```txt
函数调用 / 字段访问 / 索引
一元 - !
* / %
+ -
< <= > >=
== !=
&&
||
```

## 字符串

双引号和单引号字符串都支持：

```ku
"hello"
'hello'
```

反引号模板字符串支持 `{表达式}` 插值：

```ku
print(`Hello {name} {1 + 2}`)
```

如果要输出字面量花括号，使用转义：

```ku
print(`literal \{name\}`)
```

普通表达式中：

- `int/float` 可以做数字运算。
- `str + str` 是字符串拼接。
- `str + int` 不允许。

模板插值内部额外允许：

```ku
`value {1 + "px"}`
`value {"px" + 1.5}`
```

其他跨类型运算仍然报错，例如：

```ku
`bad {1 - "x"}`
```

## 错误处理

Ku 0.0.10 继续沿用可恢复错误模型：

```txt
T!             Result<T, str>
?              Err 时向上传
try/catch      局部处理可恢复错误
finally        无论 try 是否失败都会执行
fail           主动返回可恢复错误，错误值必须是 str
panic          不可恢复运行时错误
```

示例：

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

`?` 只能用在返回 `T!` 的函数内，或用在 `try { ... }` 的 body 内。它会短路当前表达式，避免继续执行右侧调用。`catch (err)` 中的 `err` 固定为 `str`。0.0.10 的 IR 已把 `?` 降成显式 `ok/err` 控制流，native 后端可以在此基础上继续补 Result ABI。

`panic` 不会被 `try/catch` 捕获；它表示数组越界、内部 bug、主动崩溃等不可恢复问题。

## 内置能力

基础函数：

```ku
len("Ku")    // 2
len([1, 2])  // 2
str(123)    // "123"
ok(123)     // Ok(123)
err("bad")  // Err("bad")
```

文件和编译器管线雏形：

```ku
text = fs.read("main.ku")
safe = fs.try_read("main.ku")
tokens = lexer.scan(text)
ast = parser.parse(tokens)
```

`fs.read` 读取 UTF-8 文本，当前有 1MB 文件大小保护，失败直接报运行时错误，不重试。相对路径按当前 `.ku` 源文件所在目录解析，不按启动终端所在目录解析。

`fs.try_read` 返回 `str!`，文件不存在或读取失败会返回 `Err(str)`；文件过大仍按资源保护处理为不可恢复运行时错误。

`lexer.scan` 当前返回 `[str]` 形式的 token 文本摘要。`parser.parse` 当前返回 AST 摘要字符串，后续会在 struct/enum 稳定后暴露真正的 Token/AST/Span/Error/Symbol 数据模型。二者都有输入大小、token 数和输出大小限制，避免超大字符串绕过主 parser 的资源保护。

标准库第一批模块：

```ku
string.len("Ku")
string.contains("Ku Lang", "Lang")
string.starts_with("Ku", "K")
string.ends_with("Ku", "u")
string.trim("  Ku  ")
string.lower("KU")
string.upper("ku")
string.replace("Ku Lang", "Lang", "0.0.10")
string.slice("Ku Lang", 0, 2)

array.len([1, 2])
array.is_empty([])
array.push([1, 2], 3)
array.concat([1], [2])
array.first([1, 2])
array.last([1, 2])
array.try_get([1, 2], 0)

json.stringify({ name: "Ku", version: 6 })
json.parse("{\"name\":\"Ku\"}")
json.try_parse("{bad}")

time.now()
time.unix()
time.millis()
```

`array.push` 和 `array.concat` 返回新数组，不会原地修改参数。`array.try_get` 返回元素类型的 `T!`，越界或负数返回 `Err(str)`。`string.slice` 按字符下标切片，返回 `str!`，越界返回 `Err(str)`。

`json.parse` 返回运行时 JSON 值，静态类型暂记为 `Unknown`；字段级类型推断后续随类型系统完善。`json.try_parse` 返回 `Unknown!`，可以配合 `?` 和 `try/catch`。

## Package

0.0.7 开始固定本地 package 草案，0.0.10 增加 `ku.lock` 依赖列表和 import cache key。包根目录可以放 `ku.mod`：

```txt
name = "demo_pkg"
version = "0.1.0"
root = "src"
cache = ".ku/cache"
```

有 `ku.mod` 时：

```ku
import { Value } from "util"
```

会从 `root` 下解析为 `util.ku`。相对导入 `./util.ku` 仍按当前文件目录解析，但结果不能跳出 package import root。`ku.lock` 会记录本地导入依赖和稳定内容 hash 作为 cache key。远程包、版本解析、下载校验和缓存淘汰暂未实现。

## 命令

```powershell
ku <file.ku>
ku run <file.ku>
ku check <file.ku>
ku ir <file.ku>
ku build <file.ku>
ku version
ku -v
ku -h
ku -help
ku --help
ku help
```

`ku check` 只检查，不运行。成功时输出被检查的文件；失败时输出文件名、行号、列号和源码位置。

当前 `ku check` 会覆盖：

```txt
未知字符
字符串未闭合
括号 / 方括号 / 大括号不匹配
顶层 return
函数参数数量错误
变量重复声明
变量未定义
基础类型错误
if / while 条件类型错误
数组、对象、结构体、enum 的基础语义错误
import 语法、私有导入、循环导入
Result / ? / fail / try 的基础语义错误
stdlib string / array / json / time 的参数数量和基础类型错误
```

`ku build` 当前生成“解释器打包型可执行文件”：源码被嵌入生成的 exe 中，运行 exe 会通过 Ku 解释器执行。它是真正可运行的文件，但还不是 native C/LLVM 后端。

`ku build --native <file.ku>` 当前输出 prototype C 源码，只支持 int/bool/str、局部变量、直接函数调用、print、return、if 和 while。遇到数组、struct、enum、Result、闭包、match、try 等复杂语法会明确报不支持。

当前 `ku build` 是开发环境功能，需要本机可调用 `rustc`，并且 `ku` 可执行文件旁边能找到 `libku.rlib` 或 `deps/libku*.rlib`。它只打包 Ku 源码本身，不打包 `fs.read` 读取的外部资源文件；相对资源路径仍按被 build 的源文件路径解析。

## 当前不支持

```txt
远程 package、版本解析、下载校验和缓存淘汰
异步
完整 native C / LLVM 后端
顶层脚本语句
复杂嵌套模式、match guard 模式矩阵和完整穷尽性检查
```

## 资源保护

当前解释器有基础资源上限，用来避免明显死循环、无限递归或过大输入长期占用资源：

```txt
最大 token 数: 100000
最大解析深度: 32
最大检查深度: 32
最大执行步数: 1000000
最大函数调用深度: 16
源码文件最大读取: 1000000 bytes
fs.read 最大读取: 1000000 bytes
compiler builtin 最大输入: 1000000 bytes
compiler builtin 最大 token 数: 100000
parser.parse 最大输出: 1000000 bytes
json 最大输入: 1000000 bytes
json 最大嵌套深度: 32
json.stringify 最大输出: 1000000 bytes
```
