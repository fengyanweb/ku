# Ku 0.0.12 Syntax

本文档固定 Ku 0.0.12 当前真实支持的全部语法和边界。CLI 版本应显示：

```powershell
ku version
# ku 0.0.12
```

Ku 当前是解释器优先的语言实现。文档只记录已经能被 lexer / parser / checker / runtime 闭环处理的语法；仍在设计中的能力放在文末“不支持 / 未完成”。

## 1. 文件和入口

Ku 源文件使用 `.ku` 扩展名。

程序入口必须是无参数 `main` 函数：

```ku
fn main() {
    print("Hello Ku")
}
```

当前不支持顶层脚本语句。源文件顶层只能出现：

```txt
import
module
fn
async fn
struct
enum
```

顶层 `return` 会报错。

## 2. 词法

### 2.1 空白和注释

空格、Tab、换行、Windows 回车换行和 UTF-8 BOM 会被忽略。

支持行注释：

```ku
// comment
```

支持块注释：

```ku
/*
comment
*/
```

块注释必须闭合，不支持嵌套块注释。

### 2.2 标识符

标识符只能使用 ASCII 字母、数字和下划线：

```txt
name
User
APP_NAME
value2
_temp
```

首字符必须是字母或 `_`，后续可以是字母、数字或 `_`。

### 2.3 关键字

以下单词是保留关键字：

```txt
fn async await struct enum module import from
let mut
if else while for in
break continue
match
try catch finally fail panic
return print
true false null
```

`let` / `mut` 被 lexer 识别，但 Ku 语法不使用它们；写 `let` 会得到明确错误。

### 2.4 字面量

整数：

```ku
0
123
```

浮点数：

```ku
1.5
0.25
```

布尔值：

```ku
true
false
```

空值：

```ku
null
```

字符串：

```ku
"hello"
'hello'
```

双引号字符串支持转义：

```txt
\n \r \t \" \\
```

单引号字符串支持转义：

```txt
\n \r \t \' \\
```

模板字符串使用反引号：

```ku
`Hello {name}`
`value {1 + 2}`
```

模板字符串中输出字面量花括号：

```ku
`literal \{name\}`
```

## 3. 顶层声明

### 3.1 module

`module` 声明当前是轻量模块名标记：

```ku
module demo
```

### 3.2 导出规则

顶层 `fn` / `struct` / `enum` 的名字首字母大写时，可以被其他文件导入。

```ku
fn Add(a: int, b: int): int {
    return a + b
}

struct User {
    name: str
}

enum State {
    Ready
}
```

首字母小写的顶层名字只在当前文件内部使用，不能从包外导入。

### 3.3 import

Ku 当前支持三种文件导入形式。

命名空间导入：

```ku
import math from "./math"
```

使用：

```ku
math.Add(1, 2)
math.User { name: "Ku" }
math.State.Ready
```

按需导入：

```ku
import { Add, User, State } from "./math.ku"
import { Add as Plus } from "./math"
```

使用：

```ku
Add(1, 2)
Plus(1, 2)
user = User { name: "Ku" }
state = State.Ready
```

全量导入：

```ku
import "./math.ku"
```

全量导入会把目标文件所有导出名直接放进当前文件。

不支持：

```ku
import from "./math.ku"       // 错误
```

导入路径可以使用相对路径或绝对路径，并且可以省略 `.ku` 后缀：

```ku
import math from "./math"
import { Add } from "C:/work/math.ku"
```

### 3.4 标准库模块导入

`fs`、`http` 和 `config` 标准库必须显式导入：

```ku
import "std.fs"
import "std.http"
import "std.config"
import http from "std.http"
```

`import "std.http"` 等价于导入名为 `http` 的标准库模块；`import http from "std.http"` 是显式命名空间形式。旧写法 `std:http` 不支持。

当前 `fs` / `http` 使用强制导入门禁。历史内置模块 `string` / `array` / `json` / `time` / `lexer` / `parser` 仍可直接点调用。

## 4. 类型

### 4.1 基础类型

Ku 0.0.12 的基础类型：

```txt
int
float
bool
str
null
```

不支持：

```txt
string
nil
```

### 4.2 数组类型

数组类型写作 `[T]`：

```ku
nums:[int] = [1, 2, 3]
names:[str] = ["Ku"]
matrix:[[int]] = [[1], [2]]
```

数组是同质数组。

### 4.3 结构体和枚举类型

结构体名和枚举名可以直接作为类型：

```ku
user:User = User { name: "Ku" }
state:State = State.Ready
```

命名空间导入的结构体类型可以通过 namespace 限定：

```ku
import lib from "./lib.ku"

fn show(user: lib.User): str {
    return user.name
}
```

这里的 `lib.User` 是“命名空间限定的结构体类型”，不是一种叫“命名空间结构体”的新结构。

### 4.4 Result 类型

`T!` 表示可恢复错误类型：

```ku
fn load(): str! {
    return ok("ready")
}
```

当前 `T!` 表示 `Result<T, Error>`。解释器中的 Error 是对象：

```ku
{
    domain: "fs"
    code: "read_failed"
    message: "..."
}
```

`catch (err)` 后可以使用 `err.domain`、`err.code`、`err.message`。

### 4.5 联合类型

联合类型使用 `|`：

```ku
fn show(value: str | int): str {
    return str(value)
}
```

当前联合类型主要用于参数、变量和返回值的静态检查。`str | int` 表示值可以是 `str` 或 `int`；传入 `bool` 会在 `ku check` 阶段报类型错误。IR/native 暂时把 union 降成 `unknown`，不承诺 native ABI。

## 5. 变量和赋值

Ku 不使用 `let` / `let mut`。

首次赋值即声明变量：

```ku
name = "Ku"
count = 0
```

带类型声明：

```ku
name:str = "Ku"
count:int = 0
```

部分类型可以声明默认值：

```ku
count:int
ratio:float
ok:bool
name:str
items:[int]
empty:null
```

默认值规则：

```txt
int   -> 0
float -> 0.0
bool  -> false
str   -> ""
[T]   -> []
null  -> null
```

再次赋值：

```ku
count = count + 1
```

位置解构赋值：

```ku
a, b = 1, 2
a, _ = 3, 4
```

`_` 表示丢弃该位置的值。当前解构赋值只支持变量名和 `_`，左右数量必须一致。

赋值目标可以是变量、数组元素、结构体字段或对象字段：

```ku
items[0] = 9
user.name = "Ku"
object.age = 18
```

语句级自增 / 自减会脱糖为赋值：

```ku
i++
i--
items[0]++
user.age--
```

当前 `++` / `--` 只能作为独立语句使用，目标必须可赋值且支持数字运算；不支持在表达式里读取旧值或新值。

全大写或全大写加下划线的名字按常量处理：

```ku
APP_NAME = "Ku"
MAX_COUNT = 10
```

常量后续不能再次赋值。

## 6. 函数

### 6.1 普通函数

函数参数可以写类型，也可以省略类型。返回类型也可省略：

```ku
fn add(a: int, b: int): int {
    return a + b
}

fn add_inferred(a, b) {
    return a + b
}

fn log(message: str) {
    print(message)
}
```

`main` 不能有参数。

带返回类型的函数必须有可见 `return` 或 `fail`，并且返回值类型必须匹配。未写返回类型时，checker 会从 `return` 路径推断；没有返回值时按 `null` 处理。

泛型函数使用 `<T>`：

```ku
fn id<T>(value:T): T {
    return value
}

fn first<T>(left:T, right:T): T {
    return left
}
```

泛型参数会在调用处按实参推断；同一个泛型名在同次调用中必须推断为一致类型。

### 6.2 局部函数

函数可以写在函数内部：

```ku
fn main() {
    fn go(name, age) {
        print(`我是{name},我{age}岁了`)
    }

    go("Ku", 3)
}
```

局部函数可以递归调用自身。

### 6.3 函数值

函数是第一公民。普通函数、局部函数和箭头函数都可以作为值保存、传递和调用。箭头函数与普通函数一样，可以给参数和返回值写类型：

```ku
fn main() {
    add = (a: int, b: int): int => {
        return a + b
    }
    double = (x: int): int => x * 2
    triple = x: int => x * 3
    selected = double

    print(add(1, 2))
    print(selected(3))
    print(triple(3))
}
```

类型也可以省略并由 checker 推断：

```txt
箭头函数可以使用块体或单表达式
单参数箭头函数可以省略参数小括号
带返回类型的箭头函数会检查所有 return 路径
函数值赋给另一个变量后仍然可以调用
```

### 6.4 闭包捕获

局部函数和箭头函数会精确捕获实际使用到的外层局部变量。

```ku
fn main() {
    prefix = "Hello "

    greet = (name) => {
        return prefix + name
    }

    prefix = "Hi "
    print(greet("Ku"))
}
```

当前解释器按共享绑定捕获，读取会看到外层变量最新值，赋值会写回外层可变变量。

### 6.5 async fn 和 await

`async fn` 表示可启动小协程的函数。调用一个 `async fn` 会得到一个 task；在同一个函数里调用多个 `async fn`，语义上就是启动多个独立 task，之后可以分别 `await`：

```ku
async fn load(value: int): int! {
    return ok(value)
}

async fn main(): null! {
    first = load(1)
    second = load(2)
    a = await first?
    b = await second?
    print(a + b)
    return ok(null)
}
```

第一版规则：

- 调用 `async fn` 会立即启动 task，不是延迟到 `await` 才运行。
- `async fn` 必须显式声明 `T!` 返回类型。
- `await` 只能出现在 `async fn` 内。
- `await` 的值必须是 task。
- `await task?` 等价于 `(await task)?`。
- `fn main()` 和 `async fn main()` 不能同时存在。
- async task 可以读取外层捕获，但不能修改外层捕获；checker 和 runtime 都会拒绝写入。
- native C 明确拒绝 async。

Task 生命周期 API：

```ku
task = load(1)
state = task.status()
result = task.await_timeout(1000)
cancelled = task.cancel()
```

- `task.status(): str` 返回 `pending`、`running`、`waiting`、`cancelling`、`completed`、`failed`、`cancelled` 或 `panicked`。
- `task.cancel(): bool` 发起协作式取消；返回 `true` 表示本次成功把未结束任务切换到取消流程，已经结束或已经请求取消时返回 `false`。
- `task.await_timeout(ms:int): T` 最多等待指定毫秒。超时返回结构化 `Err({ domain: "task", code: "timeout", ... })`，只结束本次等待，不自动取消目标 task。
- 取消会唤醒普通 `await` 和超时等待。排队中的任务不会再执行；运行中的 Ku 代码会在下一次解释器安全检查点结束。
- 已经送入 blocking pool 的系统调用不能被强行终止。取消会停止 task 对该调用的等待，但已经开始的文件或网络操作可能自行完成，因此带外部副作用的操作仍应使用自身超时和幂等设计。
- main 返回后，runtime 会取消仍未结束的子 task，并在 1 秒有界窗口内排空；未能停止会返回 `task/shutdown_timeout`，不会无限等待。native C / LLVM 仍明确拒绝 async lowering。

运行时默认边界：

- `max_tasks = 1024`。
- task 队列有界；超过 task 上限返回 `Err({ domain: "task", code: "too_many_tasks", ... })`。
- 队列满返回 `Err({ domain: "task", code: "queue_full", ... })`，不 panic，也不无限重试。
- blocking worker 数为 `min(32, max(4, CPU 核心数))`。
- `max_blocking_queue = 1024`。
- async task 中的 `fs.read/try_read/write/try_write`、`config.env/env_file/yaml`、`http.get/post/request` 会进入 blocking pool。
- self-await、await cycle 和过深等待链会返回结构化 task 错误，避免永久死等。

## 7. 语句

语句之间可以使用换行或分号分隔。大多数语句末尾的分号是可选的。

### 7.1 表达式语句

```ku
add(1, 2)
user.name
```

### 7.2 print

`print` 支持两种写法，`println(value)` 是可复用的内置函数形式：

```ku
print("hello")
print "hello"
println("hello")
```

推荐使用括号形式。

### 7.3 return

```ku
return
return value
```

`return` 只能写在函数体里。

### 7.4 if / else

条件必须带小括号，且条件表达式类型必须是 `bool`。Ku 不使用 truthy 规则；需要判断非空、长度或 null 时请写成显式比较。

```ku
if (age >= 18) {
    print("adult")
} else {
    print("child")
}
```

支持 `else if`：

```ku
if (score >= 90) {
    print("A")
} else if (score >= 60) {
    print("B")
} else {
    print("C")
}
```

不支持省略小括号：

```ku
if age >= 18 { }  // 错误
```

### 7.5 while

条件必须带小括号，且条件表达式类型必须是 `bool`：

```ku
i = 0
while (i < 5) {
    print(i)
    i = i + 1
}
```

### 7.6 for

`for` 当前只遍历数组：

```ku
nums:[int] = [1, 2, 3]

for n in nums {
    print(n)
}
```

### 7.7 break / continue

`break` 结束当前循环，`continue` 跳过当前循环剩余语句并进入下一轮：

```ku
i = 0
while (i < 10) {
    i++
    if (i == 2) {
        continue
    }
    if (i > 5) {
        break
    }
    print(i)
}
```

`break` / `continue` 只能写在 `while` 或 `for` 内部。

### 7.8 try / catch / finally

`try` 至少需要 `catch` 或 `finally`：

```ku
try {
    value = load()?
} catch (err) {
    print("caught: " + err.message)
} finally {
    print("cleanup")
}
```

`catch (err)` 中 `err` 类型固定为 Error 对象，不再是 `str`。

### 7.9 fail

`fail` 主动返回可恢复错误：

```ku
fn load(): str! {
    fail "missing"
}
```

`fail` 的值可以是 `str` 或 Error 对象，并且只能在返回 `T!` 的函数里使用，或出现在 `try` body 内。`fail "missing"` 会被包装成 `{ domain: "ku", code: "fail", message: "missing" }`。

### 7.10 panic

`panic` 表示不可恢复错误，不会被 `try/catch` 捕获：

```ku
panic("bad state")
```

## 8. 表达式

### 8.1 表达式总览

```txt
字面量       1, 1.5, true, false, null, "x", 'x', `x {name}`
变量         name
数组         [1, 2, 3]
对象         { name: "Ku", age: 1 }
结构体       User { name: "Ku" }
字段         user.name, lib.User, string.len
可选字段     user?.name
索引         nums[0]
调用         add(1, 2)
分组         (1 + 2)
一元         -x, !ok
二元         + - * / % == != < <= > >= && ||
自增自减     i++, i--
match        match value { _ => "ok" }
?            load()?
函数值       (a, b) => { return a + b }, x => x * 2
```

### 8.2 优先级

从高到低：

```txt
调用 / 字段 / 可选字段 / 索引 / ?
一元 - !
* / %
+ -
< <= > >=
== !=
&&
||
```

`?` 绑定在前面的表达式上：

```ku
value = load()?
```

### 8.3 算术和比较

数字运算：

```ku
1 + 2
1.5 + 2
5 - 1
2 * 3
8 / 2
5 % 2
```

规则：

```txt
int 和 int 运算结果是 int
int 和 float 混合运算结果是 float
% 只接受 int 和 int
```

字符串拼接：

```ku
"Hello " + "Ku"
```

普通表达式不允许 `str + int`：

```ku
"v" + 1  // 错误
```

比较：

```ku
1 < 2
1 <= 2
1 > 2
1 >= 2
1 == 1
1 != 2
```

大小比较只支持数字。相等比较要求两边类型兼容。

逻辑运算：

```ku
ok && ready
ok || ready
!ok
```

逻辑运算只支持 `bool`。

### 8.4 数组表达式

```ku
nums = [1, 2, 3]
empty:[int] = []
print(nums[0])
nums[0] = 9
doubled = nums.map(x => x * 2)
```

数组元素类型必须一致。索引必须是 `int`。

`array.map` 的链式写法会返回新数组，不会修改原数组。mapper 必须是函数值，参数类型由数组元素自动推断。

对象可以用字符串键动态索引。Ku 默认严格：普通索引缺少键时直接报错；只有显式在索引后写 `?` 才允许缺失并返回 `null`：

```ku
user = { name: "Ku" }
print(user["name"])
print(user["missing"])   // 运行时错误：object has no key 'missing'
print(user["missing"]?)  // null
user["age"] = 1
```

`object[key]?` 只改变对象字符串索引的缺失键行为。数组/字符串索引越界仍然报错；其他表达式后的 `?` 仍是 Result 错误传播。

字符串可以用 `int` 索引，返回单字符 `str`：

```ku
text = "Ku"
print(text[0])
```

### 8.5 可选字段访问

```ku
name = user?.name
missing = object?.missing
none = null?.name
```

`?.` 左侧是 `null` 时返回 `null`；左侧是对象或结构体但字段不存在时也返回 `null`。左侧不是对象、结构体或 `null` 时仍会报类型错误。当前支持可选字段访问和对象字符串键的显式可选索引 `object[key]?`，不支持可选调用。

### 8.6 对象字面量

对象字面量用于临时键值数据：

```ku
person = { name: "张三", age: 18 }
print(person.name)
person.age = 19
```

字段名可以是标识符或字符串：

```ku
person = { "name": "Ku" }
```

对象字面量不是 JSON 文本。字符串值必须加引号：

```ku
{ name: "张三" }
```

对象字段赋值要求字段已经存在。

### 8.7 结构体字面量

```ku
struct User {
    name: str
    age: int
}

fn main() {
    user = User { name: "Ku", age: 1 }
    user.age = 2
}
```

结构体字面量必须提供全部字段，不能提供不存在的字段。

命名空间限定的结构体字面量：

```ku
import lib from "./lib.ku"

user = lib.User { name: "Ku" }
```

`lib.User { ... }` 是导入命名空间 `lib` 中导出的 `User` 结构体字面量，不是另一种结构体类别。

### 8.8 enum 构造

无 payload 变体：

```ku
state = State.Ready
```

payload 变体：

```ku
expr = Expr.Number(1)
```

命名空间 enum：

```ku
import lib from "./lib.ku"

state = lib.State.Ready
value = lib.Result.Ok(1)
```

### 8.9 match 表达式

Ku 只保留 `match`：

```ku
text = match value {
    1 => "one"
    2 => "two"
    _ => "other"
}
```

enum match：

```ku
enum Result {
    Ok(value: int)
    Err(message: str)
}

fn main() {
    result = Result.Ok(42)
    text = match result {
        Result.Ok(value) => str(value)
        Result.Err(message) => message
    }
    print(text)
}
```

guard：

```ku
text = match result {
    Result.Ok(value) if (value > 0) => "positive"
    Result.Ok(value) => "zero or negative"
    Result.Err(message) => message
}
```

嵌套 payload 模式：

```ku
text = match value {
    Expr.Box(Inner.Number(n)) => str(n)
    Expr.Box(_) => "box"
    Expr.Empty => "empty"
}
```

arm 之间可以用逗号、分号或换行分隔。

## 9. Match 模式

当前模式类型：

```txt
_                         wildcard
name                      binding
1, 1.5, "x", true, null   literal
Enum.Variant(...)         enum variant
ns.Enum.Variant(...)      namespace enum variant
```

绑定模式会把匹配到的值放进当前 arm 作用域：

```ku
Result.Ok(value) => str(value)
```

穷尽性规则：

```txt
enum match 没有未带 guard 的 _ 或 binding catch-all 时，必须覆盖所有未带 guard 的 variant
带 guard 的分支不计入穷尽覆盖
局部嵌套模式不等于覆盖整个 variant
```

不可达诊断：

```txt
_ 之后的未带 guard 分支不可达
绑定名 catch-all 之后的未带 guard 分支不可达
重复未带 guard 字面量不可达
重复未带 guard 嵌套模式不可达
完整 variant 捕获后，后续同 variant 分支不可达
```

当前还没有完整 guard 模式矩阵和跨 guard 的穷尽性证明。

## 10. 字符串和模板字符串

普通字符串：

```ku
"hello"
'hello'
```

模板字符串：

```ku
name = "Ku"
print(`Hello {name}`)
print(`1 + 2 = {1 + 2}`)
```

模板插值中会解析 Ku 表达式。

模板插值里额外允许字符串和数字使用 `+` 拼接：

```ku
`size {1 + "px"}`
`value {"v" + 1.5}`
```

模板插值中其他跨类型运算仍然报错：

```ku
`bad {1 - "x"}`  // 错误
```

## 11. 错误处理

Ku 区分可恢复错误和不可恢复错误。

可恢复错误使用：

```txt
T!
?
try / catch / finally
fail
ok(value)
err(message)
```

不可恢复错误使用：

```txt
panic
运行时边界错误，如数组越界、除零、整数溢出、资源上限
```

### 11.1 T!

```ku
fn read_name(): str! {
    return ok("Ku")
}
```

### 11.2 ?

`?` 用于传播 `Err`：

```ku
fn main(): str! {
    name = read_name()?
    return ok(name)
}
```

`?` 只能在返回 `T!` 的函数内，或 `try` body 内使用。

### 11.3 try / catch / finally

```ku
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

`finally` 无论 try 是否失败都会执行。

`panic` 不会被 `try/catch` 捕获。

## 12. 内置函数和标准库

### 12.1 基础内置函数

```txt
len(value:any): int
str(value:any): str
ok(value:T): T!
err(message:str): Unknown!
println(value:any): null
```

示例：

```ku
print(len("Ku"))
print(str(123))
println("Ku")
return ok(1)
return err("bad")
```

### 12.2 fs

使用前必须导入：

```ku
import "std.fs"
```

```txt
fs.read(path:str): str
fs.try_read(path:str): str!
fs.write(path:str, text:str): null
fs.try_write(path:str, text:str): null!
```

`fs.read` / `fs.write` 失败会产生不可恢复运行时错误。`fs.try_read` / `fs.try_write` 在文件不存在、读取或写入失败时返回 Result。

相对路径按当前 `.ku` 文件所在目录解析。

### 12.3 lexer / parser

```txt
lexer.scan(text:str): [str]
parser.parse(input:str | [str]): str
```

当前返回调试摘要，不是完整 Token / AST 数据结构。

### 12.4 string

```txt
string.len(text:str): int
string.contains(text:str, needle:str): bool
string.starts_with(text:str, prefix:str): bool
string.ends_with(text:str, suffix:str): bool
string.trim(text:str): str
string.lower(text:str): str
string.upper(text:str): str
string.replace(text:str, from:str, to:str): str
string.slice(text:str, start:int, end:int): str!
```

`string.slice` 按字符下标切片，越界返回 `Err(Error)`。

字符串函数也可以作为实例方法调用：

```ku
text = " Ku "
print(text.trim())
print(text.slice(1, 3)?)
```

### 12.5 array

```txt
array.len(values:[T]): int
array.is_empty(values:[T]): bool
array.first(values:[T]): T
array.last(values:[T]): T
array.try_get(values:[T], index:int): T!
array.push(values:[T], value:T): [T]
array.concat(left:[T], right:[T]): [T]
values.map(mapper): [U]
```

`array.push` 和 `array.concat` 返回新数组，不会原地修改参数。

`array.first` / `array.last` 对空数组返回 `null`；需要带错误信息的越界访问时使用 `array.try_get`，错误 payload 是 Error 对象。

`values.map(mapper)` 是数组实例方法。`mapper` 是函数值，例如 `nums.map(x => x * 2)`。

除 `map` 外，数组函数也可以作为实例方法调用：

```ku
values = [1, 2]
print(values.len())
print(values.try_get(0)?)
```

### 12.6 json

```txt
json.stringify(value:any): str
json.parse(text:str): Unknown
json.try_parse(text:str): Unknown!
```

`json.parse` 失败是不可恢复运行时错误；`json.try_parse` 失败返回 `Err(Error)`。

### 12.7 time

```txt
time.now(): int
time.unix(): int
time.millis(): int
```

### 12.8 config

配置读取需要显式导入：

```ku
import "std.config"
```

签名：

```txt
config.env(): object
config.env_file(path:str): object
config.yaml(path:str): object!
```

`config.env()` 从当前源码文件所在目录读取 `.env`；文件不存在时返回空对象。`config.env_file(path)` 读取显式 `.env` 文件，读取或解析失败是不可恢复运行时错误。`config.yaml(path)?` 读取第一版平面 YAML，返回 `Result`，失败时返回 `Err({ domain:"config", code:"read_failed", message })`。

第一版配置格式保持小而稳：

- `.env` 支持 `KEY=value`、单双引号、基础转义和 `#` 注释行。
- `yaml` 支持平面 `key: value`，标量支持 `str/int/float/bool/null`。
- 配置文件上限为 1000000 bytes。
- YAML 嵌套、数组和复杂对象暂不支持。

### 12.9 http

HTTP 需要显式导入：

```ku
import "std.http"
import http from "std.http"
```

签名：

```txt
http.get(url:str): HttpResponse!
http.post(url:str, body:str): HttpResponse!
http.request(config:object): HttpResponse!
http.client(config?:object): object
http.text(body:str): HttpResponse
http.json(value:any): HttpResponse
http.service: object
http.service(config?:object): object
http.server(config?:object): object
```

`HttpResponse` 当前用对象表示：

```ku
res = http.get("https://example.com")?
print(res.status)
print(res.body)
```

`http.request` 支持 `{ method, url, headers, body, timeout_ms, max_body_bytes }`。网络、协议、URL 和 body 限制错误返回结构化 `Err({ domain, code, message })`；HTTP 非 2xx 状态仍返回 `Ok(HttpResponse)`，由调用者检查 `res.status`。

HTTP client 使用全局默认 client/agent，底层复用连接；`http.get` / `http.post` / `http.request` 是默认 client 的快捷方式。默认限制：

```txt
timeout_ms: 5000
max_body_bytes: 1000000
```

`http.client(config?)` 返回 client 配置对象，当前支持 `timeout_ms` 和 `max_body_bytes`。它适合后续扩展 cookie、默认 header、代理、认证 token、重试策略和连接池参数。当前 `http.get/post/request` 仍使用默认全局 client。

响应 helper：

```ku
return http.text("ok")
return http.json({ ok: true })
```

HTTP server/router API 已固定服务配置对象：

```ku
service = http.service
server = http.server({ max_body_bytes: 4096 })
service.get("/index", (req, res) => {
    return http.text("ok")
})
service.post("/pets", (req, res) => {
    return http.json({ ok: true })
})
service.get("/user/{id}", (req, res) => {
    return http.text("ok")
})
listener = service.bind(":0")?
print(service.max_active_requests)
print(service.max_pending_requests)
print(service.max_body_bytes)
print(service.routes[0].method)
```

默认 server 配置包含 `read_header_timeout_ms`、`read_body_timeout_ms`、`write_timeout_ms`、`idle_timeout_ms`、`handler_timeout_ms`、`max_body_bytes`、`max_header_bytes`、`max_connections`、`max_active_requests`、`max_pending_requests` 和 `routes`。`service.get/post/put/del(path, handler)` 当前支持注册路由，会把 `{ method, path, param_names, handler }` 写入 `service.routes`。路径参数使用 `/user/{id}`，不使用 Express 的 `:id`。

`service.bind(address)?` 会在 `bind/listen` 前检查并编译 method 分组的路由形状表，`:0` 会让系统分配空闲端口；请求匹配使用这个 `compiled_router`，不会在每次请求时扫描 `service.routes`。`bind/listen` 的配置只来自 `http.service(config?)` / `http.server(config?)` 创建出的 service 对象，不接受第二个 config 参数。`listener.run()?` 会阻塞处理 HTTP 请求，`listener.close()?` 会显式关闭还没 run 的 listener。第一版 handler 参数固定 `(req, res)`，handler 返回 `http.text/json(...)` 这类 `{ status, headers, body }` 响应对象。

第一版 `req` 字段：

```txt
req.method: str
req.path: str
req.params: object   // 字段值按 str 检查
req.query: object    // 字段值按 str 检查
req.headers: object  // 字段值按 str 检查
req.body: str
```

HTTP handler 会在类型检查阶段按 `(req, res)` ABI 复查参数和返回值。为了给后续并发 runtime 留安全边界，handler 第一版不能修改外层捕获变量；需要共享状态时后续应通过专门的 `std.atomic` / `std.sync` 一类 API 设计。

当前 runtime 是有界阻塞 server：

- `max_connections` 表示同时在线连接上限；超过时立即返回 503。
- `max_active_requests` 控制同时工作的连接/handler worker 数。
- `max_pending_requests` 是有界等待队列；队列满时立即返回 503，不无限排队。
- `handler_timeout_ms` 到时返回 504。解释器执行会检查 deadline；响应线程不会在超时后继续无限等待。
- `idle_timeout_ms` 限制连接在发送首字节前的空闲等待。
- header/body/write 分别使用自己的 timeout，网络错误不自动无限重试。

路由未命中返回 404，路径存在但 method 不匹配返回 405，body 超过 `max_body_bytes` 返回 413，header 超过 `max_header_bytes` 返回 431，坏请求返回 400。

## 13. struct

声明：

```ku
struct Token {
    kind: str
    text: str
    line: int
    column: int
}
```

使用：

```ku
token = Token {
    kind: "Ident",
    text: "name",
    line: 1,
    column: 1
}

print(token.kind)
token.kind = "Number"
```

当前不支持：

```txt
方法
继承
泛型
可选字段
默认字段值
```

## 14. enum

声明：

```ku
enum TokenKind {
    Ident
    Number
    Eof
}

enum Expr {
    Number(value: int)
    String(value: str)
    Flag(ok: bool)
}
```

构造：

```ku
kind = TokenKind.Ident
expr = Expr.Number(1)
```

注意：当前 checker 尚未开放 enum 自递归 payload，例如 `Expr.Binary(left: Expr, right: Expr)` 仍会报未知类型。需要树结构时，先用非递归 payload 或等待后续类型系统升级。

## 15. Package 和 ku.mod

`ku.mod` 是包配置文件，不是 Ku 源码。

示例：

```txt
name = "demo_pkg"
version = "0.1.0"
root = "src"
cache = ".ku/cache"

dep.util = "1.0.0"
dep.util.source = "file://C:/work/util"
dep.util.checksum = "ku-fnv64-0123456789abcdef"
```

字段：

```txt
name       package 名
version    package 版本
root       import root，默认 src
cache      cache 目录，默认 .ku/cache
dep.NAME   dependency 版本
dep.NAME.source
dep.NAME.checksum
```

package import：

```ku
import { Value } from "util"
import { Value } from "@util/util"
```

`@util/util` 表示从 dependency cache 的 `util` 包里导入 `util.ku`。

当前 package source 只执行 `file://` 目录下载/缓存。registry 网络下载尚未实现，但第一版离线 schema 已实现严格解析：

```toml
name = "math"
version = "0.1.0"
source = "https://registry.example/ku/math/0.1.0.tar.gz"
checksum = "sha256-<64 hex digits>"
```

registry lock 使用一个或多个 `[[package]]`，要求 `name/version/source/url/checksum/cache_key` 齐全；`source` 必须是 `registry`，版本必须是 `major.minor.patch`，checksum 必须是 `sha256-` 加 64 位十六进制。

离线 resolver 已支持精确版本 `1.2.3` 和 caret 范围 `^1.2.3`。同名依赖的全部约束会合并，选择满足全部约束的最高可用版本；没有共同版本时返回 `package/dependency_conflict`，不做无限回溯或 SAT 搜索。网络层仍未接入，但下载计划已固定为有界策略：最多 8 次尝试、连接/读取超时均有上限、单包最多 100 MB、校验通过的 cache 直接复用，否则下载到进程和序号唯一的临时位置，并在 checksum 通过后原子替换。

## 16. CLI 相关语法

命令：

```powershell
ku <file.ku>
ku run <file.ku>
ku check <file.ku>
ku check --json <file.ku>
ku ir <file.ku>
ku llvm <file.ku>
ku build <file.ku>
ku build --native <file.ku>
ku package gc <file.ku>
ku version
ku -v
ku -h
ku -help
ku --help
ku help
```

`ku check` 会检查：

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
数组、对象、结构体、enum 基础语义错误
import 语法、私有导入、循环导入
Result / ? / fail / try 基础语义错误
stdlib 参数数量和基础类型错误
http std module 是否已显式导入
```

`ku check --json` 使用 JSON Lines。成功时静默；失败时每行一个诊断对象，稳定字段为：

```txt
level code message file line column endLine endColumn notes helps
```

VS Code 扩展优先读取 JSON diagnostics；面对旧版 Ku CLI 时只回退一次文本解析，不循环重试。

`ku llvm file.ku` 在源文件旁输出 `.ll`，不要求本机安装 LLVM。当前文本后端支持 `int/bool/str`、普通函数、局部变量、直接调用、`return`、`if/while`、`print`、非递归 struct 值与字段读写，以及 `Result<int|bool|str|struct>` 的 `ok`、`fail`、`?` 和错误传播。数组、enum、闭包、HTTP 和 async 仍会明确报不支持。后端会拒绝递归值 struct、缺失/重复 CFG block 和无条件自跳，避免生成明显错误或永久循环的 `.ll`。golden test 不依赖外部工具；检测到 `llvm-as` 时会额外验证生成文本。

`ku build` 当前生成解释器打包型可执行文件。

`ku build --native` 当前输出 prototype C 源码，支持：

```txt
int / bool / str
非递归 struct layout / literal / field read / field write
局部变量
直接函数调用
print
return
if / while
基础 Result ABI
ok / err / ?
错误传播
有界 array runtime / length / read-write bounds check
enum tag + payload layout
unit / payload / nested enum match lowering
系统 int main(void) wrapper
```

native C 当前仍是 prototype：array 使用 `{ len, data }` runtime-owned 结构，字面量会复制数据，所有读写索引都检查负数和 `index >= len`；越界打印清晰错误并终止进程。enum 使用 `tag + union payload`，支持 unit variant、payload variant、guard、绑定和嵌套 enum match。struct/enum 值布局仍拒绝递归；array 暂无释放/所有权 ABI，错误槽也还不是完整 Error 对象 ABI。闭包、try/catch/finally 和 async native lowering 仍明确不支持。

## 17. 资源保护

当前资源上限：

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
http response 最大 body: 1000000 bytes
http 默认超时: 5 秒
```

## 18. 当前不支持 / 未完成

```txt
LLVM 数组、enum、闭包、HTTP、async lowering
LLVM 递归 struct 和更复杂 Result payload
完整 native C 后端
registry 网络下载
registry 索引协议、签名信任和实际网络缓存更新
native array/struct/enum 的释放、复制和移动所有权 ABI
match guard 模式矩阵和跨 guard 的完整穷尽性检查
顶层脚本语句
方法 / trait / interface
泛型类型 / 泛型 struct / 泛型方法 / trait 约束泛型
模块内嵌作用域
异常式 throw
JavaScript 式 Promise
数组切片语法
表达式级 ++ / --
字典 / Map 专用类型
```

## 19. 快速语法表

```txt
program       ::= item*
item          ::= import | module | fn | async_fn | struct | enum
import        ::= 'import' IDENT 'from' STRING
                | 'import' '{' import_name (',' import_name)* '}' 'from' STRING
                | 'import' STRING
import_name   ::= IDENT ('as' IDENT)?
module        ::= 'module' IDENT
fn            ::= 'fn' IDENT '(' params? ')' (':' type)? block
async_fn      ::= 'async' 'fn' IDENT '(' params? ')' (':' type)? block
params        ::= IDENT (':' type)? (',' IDENT (':' type)?)*
struct        ::= 'struct' IDENT '{' fields* '}'
enum          ::= 'enum' IDENT '{' variants* '}'
type          ::= union
union         ::= result ('|' result)*
result        ::= atom '!'?
atom          ::= 'int' | 'float' | 'bool' | 'str' | 'null' | '[' type ']' | IDENT ('.' IDENT)*
block         ::= '{' stmt* '}'
stmt          ::= var | assign | destructure | inc | if | while | for | break | continue | try | fail | panic | return | print | expr
var           ::= IDENT ':' type ('=' expr)?
assign        ::= assign_target '=' expr
destructure   ::= (IDENT | '_') (',' (IDENT | '_'))+ '=' expr (',' expr)+
inc           ::= assign_target ('++' | '--')
assign_target ::= IDENT | expr '[' expr ']' | expr '.' IDENT
if            ::= 'if' '(' expr ')' block ('else' (if | block))?
while         ::= 'while' '(' expr ')' block
for           ::= 'for' IDENT 'in' expr block
try           ::= 'try' block ('catch' '(' IDENT ')' block)? ('finally' block)?
return        ::= 'return' expr?
print         ::= 'print' expr | 'print' '(' expr ')'
expr          ::= literal | IDENT | call | field | index | optional_index | await | array | object | struct_lit | match | arrow | unary | binary | '(' expr ')'
match         ::= 'match' expr '{' arm* '}'
arm           ::= pattern ('if' expr)? '=>' expr
pattern       ::= '_' | IDENT | literal | IDENT '.' IDENT ('(' pattern* ')')? | IDENT '.' IDENT '.' IDENT ('(' pattern* ')')?
optional_index ::= expr '[' expr ']' '?'
await         ::= 'await' expr
arrow         ::= IDENT (':' type)? '=>' (expr | block)
                | '(' params? ')' (':' type)? '=>' (expr | block)
```
