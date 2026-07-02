# Ku 0.0.15 Syntax

本文档固定 Ku 0.0.15 当前真实支持的全部语法和边界。CLI 版本应显示：

```powershell
ku version
# ku 0.0.15
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
import { fs, http, time } from "std"
```

`import "std.http"` 等价于导入名为 `http` 的标准库模块；`import http from "std.http"` 是显式命名空间形式。

`import { fs, http } from "std"` 表示一次导入多个标准库模块，也可以多行书写：

```ku
import {
    fs,
    http,
    time
} from "std"
```

`from "std"` 只接受标准库模块名，不支持 alias；`import { fs as file } from "std"` 当前会报错。旧写法 `std:http` 不支持。

当前 `fs` / `http` 使用强制导入门禁。历史内置模块 `string` / `array` / `json` / `time` / `lexer` / `parser` 仍可直接点调用。

## 4. 类型

### 4.1 基础类型

Ku 0.0.15 的基础类型：

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

`_` 表示丢弃该位置的值。位置解构赋值只支持变量名和 `_`，左右数量必须一致。

对象解构赋值按 JS 风格进入当前语法：

```ku
user = { name: "Ku", city: "Hangzhou" }
{ name, city: place, missing = "fallback", ...rest } = user
{ code } = http
```

规则：

- `{ name } = obj` 绑定同名字段。
- `{ city: place } = obj` 读取 `city` 字段并绑定到 `place`。
- `{ missing = value } = obj` 仅在字段缺失时使用 default。
- `{ ...rest } = obj` 绑定剩余字段对象，rest 必须放最后。
- `{ field: _ } = obj` 读取并丢弃字段。
- 对象解构会消费右侧 source object；解构后不能继续使用原对象。需要保留原对象时写 `{ name } = user.clone()`。
- 静态 object 缺字段会在检查阶段报错；动态 object 缺字段在运行时报错。需要宽松读取时继续使用显式 `obj["field"]?`。
- `http` 作为 std 模块对象只暴露可作为值读取的对象成员，例如 `{ code } = http`；`http.service` / `http.server` 是函数，不能被当作属性式默认对象解构或读取。

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
++i
--i
items[0]++
user.age--
```

`++` / `--` 只能作为独立语句使用，目标必须可赋值且支持数字运算；不支持在表达式里读取旧值或新值。

复合赋值支持：

```ku
i += i + 1
i -= 1
i *= 2
i /= 3
i %= 2
items[0] += 10
user.age -= 1
```

复合赋值按“读取当前左值一次、计算、写回”的语义执行。当前赋值目标仍沿用已有边界：变量、直接数组/对象索引、直接字段可写；深层链式写入仍按现有 assignment target 规则检查。

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

函数是第一公民。普通函数、局部函数、匿名 `fn` 和箭头函数都可以作为值保存、传递和调用。箭头函数与普通函数一样，可以给参数和返回值写类型：

```ku
fn main() {
    add = (a: int, b: int): int => {
        return a + b
    }
    double = (x: int): int => x * 2
    triple = x: int => x * 3
    handler = fn(req, res) {
        return "ok"
    }
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
匿名 fn 使用块体，适合 HTTP handler 这类需要普通函数形状的内联写法
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

### 6.5 async fn、task 句柄和 await

`async fn` 表示可异步执行的函数。调用一个 `async fn` 会立即启动一个轻量 task，并返回一个一次性 task 句柄；task 不是线程，也不是关键字，用户不能手动创建或调度 task。

普通开发者只需要使用这一种模型：

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

核心规则：

- 调用 `async fn` 会立即启动 task，不是延迟到 `await` 才运行。
- `async fn` 必须显式声明 `T!` 返回类型；调用结果的内部类型可显示为 `Task<T!>`，但日常代码推荐靠类型推导。
- `task` 不是关键字，普通变量仍然可以叫 `task`。
- task 是异步任务句柄，不是 OS 线程，不代表用户能直接管理调度器。
- Ku 不提供 `task.spawn`、`Task.new`、`runtime.schedule` 或 `thread.spawn` 这类用户级任务创建/调度 API。
- 普通 task 是 owned、move-only，不能隐式复制，不能 `clone()`。
- `await task` 会消费 task 句柄；普通 task 只能 await 一次。
- `await task?` 等价于 `(await task)?`，先等待任务完成，再把 `Result` 错误向外传播。
- `await` 只能出现在 `async fn` 内。
- `await` 的值必须是 task。
- `fn main()` 和 `async fn main()` 不能同时存在。
- async task 可以读取外层捕获，但不能修改外层捕获；checker 和 runtime 都会拒绝写入。
- HTTP server 内部可以使用 task 处理并发请求，但 handler 用户不需要手动创建或管理 task。
- native C 明确拒绝 async。

错误示例：

```ku
task = load(1)
first = await task?
second = await task? // error: task has already been awaited
```

正确写法：

```ku
task = load(1)
value = await task?
println(value)
println(value)
```

作用域结束时，用户没有 detach/cancel/spawn 入口。当前解释器在 `main` 返回后会请求取消仍未结束的子 task，并在 1 秒有界窗口内排空；未能停止会返回 `task/shutdown_timeout`，不会无限等待。native C / LLVM 仍明确拒绝 async lowering。

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

### 7.2 print / println

`print` 输出内容但不自动换行；`println(value)` 输出内容并追加换行。

`print` 支持两种写法，推荐使用括号形式：

```ku
print("hello")
print "hello"
print(" ")
println("world")
```

上面输出为同一行 `hello world\n`。需要逐行日志、示例输出、压测结果时优先使用 `println`。

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

当 `then` / `else` 分支只有一个语句时，可以省略 `{}`：

```ku
if (age >= 18) print("adult")
else print("child")

while (true)
    if (ready) break
    else continue
```

省略 `{}` 只包住紧跟着的一条语句。多条语句必须继续使用块。

### 7.5 while

条件必须带小括号，且条件表达式类型必须是 `bool`：

```ku
i = 0
while (i < 5) {
    print(i)
    i = i + 1
}
```

循环体只有一个语句时可以省略 `{}`：

```ku
while (i < 5) i++
```

### 7.6 for

`for` 可以遍历数组，也可以遍历非负整数范围。

```ku
nums:[int] = [1, 2, 3]

for n in nums {
    print(n)
}

for i in 10 {
    print(i) // 0 到 9
}
```

`for i in 10` 表示迭代 `0 <= i < 10`。负数会报错，不会静默跳过。

循环体只有一个语句时可以省略 `{}`：

```ku
total = 0
for i in 4 total += i
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

### 7.8 循环上限与抢占边界

解释器不再设置固定“最多执行多少步”的硬性循环上限。同步死循环会一直占用当前解释器线程，直到进程、终端、测试 harness、HTTP handler timeout 或操作系统限制终止它。

仍然存在这些边界：

- `int` 运算使用有界整数，溢出会报 `integer overflow`。
- 函数调用深度有保护，直接或间接递归过深会报错，避免只依赖宿主栈崩溃。
- async task 的循环会在语句 tick 时检查协作式取消；main 返回后的 shutdown 会取消未完成 task，并在有界窗口内排空。
- HTTP handler 有 `handler_timeout_ms`，超时返回 504，不会让请求无限等待。
- 不断分配内存的循环仍可能触发宿主环境 OOM。

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
println(str(123))
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
time.now(): Time
time.now(value: Time): int
time.unix(): int
time.unix(value: Time): int
time.millis(): int
time.millis(value: Time | Duration): int
time.from_unix(seconds:int): Time
time.from_millis(ms:int): Time
time.date(): Date
time.date(value: Time): Date
time.date(value: Time, zone:str): Date!
time.date(year:int, month:int, day:int): Date!
time.datetime(year:int, month:int, day:int, hour:int, minute:int, second:int): Time!
time.datetime(year:int, month:int, day:int, hour:int, minute:int, second:int, zone:str): Time!
time.format(value: Time): str
time.format(value: Time, layout:str): str!
time.format(value: Time, layout:str, zone:str): str!
time.parse(text:str): Time!
time.parse(text:str, layout:str): Time!
time.parse(text:str, layout:str, zone:str): Time!
time.duration(ms:int): Duration!
time.duration(value:int, unit:str): Duration!
time.add(value: Time, duration: Duration): Time
time.sub(value: Time, duration: Duration): Time
time.diff(left: Time, right: Time): Duration
time.compare(left: Time, right: Time): int
time.parts(value: Time): object
time.parts(value: Time, zone:str): object!
time.weekday(value: Time | Date): int
time.weekday(value: Time, zone:str): int!
time.is_leap(year:int): bool
time.days_in_month(year:int, month:int): int!
time.sleep(ms:int): null!
time.sleep(duration: Duration): null!
```

第一版不新增独立 VM 值类型，`Time` / `Date` / `Duration` 用普通 object 承载：

```ku
now = time.now()
print(now.kind)   // "time.time"
print(now.millis) // Unix 毫秒时间戳
```

`Date` 形如 `{ kind:"time.date", year, month, day }`，`Duration` 形如 `{ kind:"time.duration", millis }`。

默认格式为：

```txt
yyyy-MM-dd HH:mm:ss
```

支持的格式符：

```txt
yyyy 年
MM   月 01-12
dd   日 01-31
HH   小时 00-23
mm   分钟 00-59
ss   秒 00-59
SSS  毫秒 000-999
```

`zone` 第一版支持 `"local"`、`"utc"`、`"+08:00"`、`"-05:30"` 这类固定偏移。

示例：

```ku
import { time } from "std"

fn main(): null! {
    now = time.now()
    text = time.format(now, "yyyy-MM-dd HH:mm:ss", "+08:00")?
    println(text)

    duration = time.duration(5, "s")?
    later = time.add(now, duration)
    println(time.millis(time.diff(later, now)))

    d = time.date(2026, 6, 23)?
    println(time.weekday(d)) // 1=周一，7=周日

    time.sleep(1000)?
    return ok(null)
}
```

非法日期、非法时间、非法格式、非法时区和非法 duration 返回结构化 `Err({ domain:"time", code, message })`。`time.sleep` 在同步 main 中阻塞当前执行；在 async task 中会走 blocking worker，避免长时间占用 task worker。

### 12.8 std.task 观测与压力测试

使用前显式导入：

```ku
import "std.task"
```

```txt
task.stats(): object
task.stress(demand:int, producers:int, hold_ms:int): object
```

`std.task` 是 runtime 诊断和压力测试命名空间，不是普通 task 句柄 API。它不提供 `task.spawn`、`Task.new`、`runtime.schedule` 或 `thread.spawn`，也不能手动创建用户任务。普通业务并发仍然只通过 `async fn` 调用返回的 `Task<T>` 句柄和 `await task` / `await task?` 完成。

`task.stats()` 返回当前 runtime 的 active/registered/queued task、等待边、blocking job、worker 数以及累计 accepted/rejected/finished。

`task.stress` 用多个生产者并发提交指定数量的 task demand。runtime 仍执行默认 `max_tasks = 1024`，超出的需求立即按 `too_many_tasks` 计入拒绝，不会扩大上限或无限排队。参数边界：

- demand：1 到 10000000。
- producers：1 到 64。
- hold_ms：0 到 60000。
- 调用时 runtime 必须空闲；如果已有 active/queued task 或 blocking job，会返回 `task/stress_runtime_busy`，避免指标和业务 task 混在一起。
- workload drain 最多等待 30 秒，超时返回 `task/stress_timeout`。

仓库根目录的 `test.ku` 打印前后时间、耗时和 runtime 指标；`run-test.ps1` 额外从进程外采集 CPU 时间、峰值 working set、峰值 private memory 和线程数：

```powershell
.\run-test.ps1
```

### 12.9 config

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

### 12.10 http

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
http.text(status:int, body:str): HttpResponse
http.json(value:any): HttpResponse
http.json(status:int, value:any): HttpResponse
http.empty(status?:int): HttpResponse
http.redirect(location:str): HttpResponse
http.redirect(status:int, location:str): HttpResponse
http.statusText(code:int): str
http.service(config?:object): object
http.server(config?:object): object
```

`http.service` / `http.server` 是函数，必须写成 `http.service()` / `http.server(config)`。Ku 不再兼容旧的属性式默认对象；写 `app = http.service` 会报错并提示改成函数调用。

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
return http.text(http.status.created, "created")
return http.json({ code: 0, msg: "ok", data: null })
return http.json(http.status.created, { code: 0, msg: "created", data: null })
return http.empty()
return http.empty(http.status.noContent)
return http.redirect("/login")
return http.redirect(http.status.temporaryRedirect, "/login")
```

`http.text(body)` 和 `http.json(body)` 默认 HTTP status 是 `http.status.ok` / `200`。创建资源建议显式 `http.status.created` / `201`。`http.empty()` 默认 `204`。错误响应建议显式传入 `4xx/5xx`。`http.redirect(location)` 默认 `302`，也可以显式传入 `301/303/307/308`。

HTTP status 是协议状态码，放在 `http.text/json/empty/redirect` 的第一个参数；业务响应里的 `body.code` 由开发者自己维护，Ku 不替业务维护业务码。推荐业务成功固定 `code: 0`，`msg` 是提示文本，`data` 成功时放数据、失败时通常放 `null`。

状态码常量：

```ku
http.status.ok                 // 200 OK
http.status.created            // 201 Created
http.status.accepted           // 202 Accepted
http.status.noContent          // 204 No Content
http.status.movedPermanently   // 301 Moved Permanently
http.status.found              // 302 Found
http.status.seeOther           // 303 See Other
http.status.notModified        // 304 Not Modified
http.status.temporaryRedirect  // 307 Temporary Redirect
http.status.permanentRedirect  // 308 Permanent Redirect
http.status.badRequest         // 400 Bad Request
http.status.unauthorized       // 401 Unauthorized
http.status.forbidden          // 403 Forbidden
http.status.notFound           // 404 Not Found
http.status.methodNotAllowed   // 405 Method Not Allowed
http.status.notAcceptable      // 406 Not Acceptable
http.status.requestTimeout     // 408 Request Timeout
http.status.conflict           // 409 Conflict
http.status.gone               // 410 Gone
http.status.contentTooLarge    // 413 Content Too Large
http.status.uriTooLong         // 414 URI Too Long
http.status.unsupportedMedia   // 415 Unsupported Media Type
http.status.rangeNotSatisfiable // 416 Range Not Satisfiable
http.status.unprocessable      // 422 Unprocessable Content
http.status.tooManyRequests    // 429 Too Many Requests
http.status.headerTooLarge     // 431 Request Header Fields Too Large
http.status.internalError      // 500 Internal Server Error
http.status.notImplemented     // 501 Not Implemented
http.status.badGateway         // 502 Bad Gateway
http.status.serviceUnavailable // 503 Service Unavailable
http.status.gatewayTimeout     // 504 Gateway Timeout

text = http.statusText(http.status.notFound) // "Not Found"
```

`http.code` 是大写短别名对象，当前只用于少量常见 HTTP status，例如 `http.code.SUCCESS == 200`。新代码优先使用更清楚的 `http.status.*`。

HTTP server/router API 已固定服务配置对象：

```ku
fn index(req, res) {
    return http.text("ok")
}

service = http.service()
server = http.server({ max_body_bytes: 4096 })
service.get("/index", index)
service.get("/fn", fn(req, res) {
    return http.text("Ku HTTP 123")
})
service.post("/pets", (req, res) => {
    return http.json(http.status.created, { code: 0, msg: "created", data: null })
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

`http.service()` / `http.server(config?)` 返回 service 配置对象。`service.bind(address)?` 会在 `bind/listen` 前检查并编译 method 分组的路由形状表，`:0` 会让系统分配空闲端口；请求匹配使用这个 `compiled_router`，不会在每次请求时扫描 `service.routes`。`bind/listen` 的配置只来自 `http.service(config?)` / `http.server(config?)` 创建出的 service 对象，不接受第二个 config 参数。`listener.run()?` 会阻塞处理 HTTP 请求，`listener.close()?` 会显式关闭还没 run 的 listener。第一版 handler 参数固定 `(req, res)`，handler 返回 `http.text/json/empty/redirect(...)` 这类 `{ status, headers, body }` 响应对象。

第一版 `req` 字段：

```txt
req.method: str
req.path: str
req.params: object   // 字段值按 str 检查
req.query: object    // 字段值按 str 检查
req.headers: object  // 字段值按 str 检查
req.body: str
```

`req` 是请求对象，提供 method/path、路由参数、query、headers 和 body。`res` 是第一版 handler ABI 的响应占位对象，当前常规写法不需要手动修改 `res`，直接 `return http.text/json/empty/redirect(...)` 即可；保留 `res` 是为了让 handler 形状固定，后续可扩展 response builder。

HTTP handler 会在类型检查阶段按 `(req, res)` ABI 复查参数和返回值。为了给后续并发 runtime 留安全边界，handler 第一版不能修改外层捕获变量；需要共享状态时后续应通过专门的 `std.atomic` / `std.sync` 一类 API 设计。

当前 runtime 是有界阻塞 server：

- `max_connections` 表示同时在线连接上限；超过时立即返回 503。
- `max_active_requests` 控制同时工作的连接/handler worker 数。
- `max_pending_requests` 是有界等待队列；队列满时立即返回 503，不无限排队。
- `handler_timeout_ms` 到时返回 504。解释器执行会检查 deadline；响应线程不会在超时后继续无限等待。
- `idle_timeout_ms` 限制连接在发送首字节前的空闲等待。
- header/body/write 分别使用自己的 timeout，网络错误不自动无限重试。

Ku runtime 自动维护自己能判断的协议错误：路由未命中返回 404，路径存在但 method 不匹配返回 405，body 超过 `max_body_bytes` 返回 413，header 超过 `max_header_bytes` 返回 431，坏请求返回 400，handler timeout 返回 504，服务过载返回 503，handler panic/内部失败返回 500。

可直接运行的 HTTP 示例：

```powershell
cargo run -- run examples\http_server.ku
```

另开一个终端压测：

```powershell
powershell -ExecutionPolicy Bypass -File examples\http_bench.ps1 -Url http://127.0.0.1:8080/json -Requests 10000 -Concurrency 100
```

`examples/http_bench.ps1` 使用内嵌 C# `HttpClient` 并发发送请求，输出开始/结束时间、总耗时、RPS、错误数、状态码分布和粗略延迟分位；脚本有外部 deadline，不会无限等待。

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
main       package 入口，默认 main.ku
out        build 输出目录，默认 .ku/build
cache      cache 目录，默认 .ku/cache
template   create/init 使用的内置模板名，可选
type       package 类型，例如 lib，可选
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

当前 CLI package import 仍只启用 `file://` 目录 source。registry 的执行层已经实现 HTTPS-only 获取、静态 index 解析、SHA-256 流式校验、有界重试/超时/下载大小、唯一临时目录和内容寻址 cache；在签名信任根与归档格式完成决策前，CLI 不会绕过验证直接启用远程包：

```toml
name = "math"
version = "0.1.0"
source = "https://registry.example/ku/math/0.1.0.tar.gz"
checksum = "sha256-<64 hex digits>"
```

registry lock 使用一个或多个 `[[package]]`，要求 `name/version/source/url/checksum/cache_key` 齐全；`source` 必须是 `registry`，版本必须是 `major.minor.patch`，checksum 必须是 `sha256-` 加 64 位十六进制。`cache_key` 必须由 name、精确版本和完整 SHA-256 确定，不能自行填写任意值。

resolver 支持精确版本 `1.2.3` 和 caret 范围 `^1.2.3`。同名依赖的全部约束会合并，选择满足全部约束的最高可用版本；没有共同版本时返回 `package/dependency_conflict`，不做无限回溯或 SAT 搜索。远程请求最多 8 次，连接/读取超时均有上限，单包最多 100 MB；只对明确瞬时错误有限退避。下载 staging 与 GC 隔离，SHA-256 通过后安装到不可覆盖的内容寻址 cache；并发安装锁和旧锁恢复均有界。当前已实现 `Ed25519RegistryIndexVerifier`，可验证 registry index 的 detached signature；未配置 verifier、缺少 dependency source 或签名不匹配都会 fail-closed，不会读取旧 cache 绕过信任边界。内置官方根公钥、自定义 registry 公钥配置、key rotation/revocation 和受限 `.tar.zst` 解包仍在后续队列。

## 16. CLI 相关语法

命令：

```powershell
ku <file.ku>
ku create <name>
ku create <name> --template <template>
ku create --list
ku init
ku init --template <template>
ku template list
ku run
ku run <file.ku>
ku check
ku check <file.ku>
ku check --deny-unused [file.ku]
ku check --json
ku check --json [--deny-unused] <file.ku>
ku ir <file.ku>
ku llvm <file.ku>
ku build [file.ku]
ku build .
ku build -o <path> [file.ku]
ku build --release [file.ku]
ku build --debug [file.ku]
ku build --profile <debug|release|small|fast> [file.ku]
ku build --target <target> [file.ku]
ku build --emit-c [file.ku]
ku build --emit-ir [file.ku]
ku build --emit-llvm [file.ku]
ku build --backend c [file.ku]
ku build --native <file.ku>
ku package gc <file.ku>
ku version
ku -v
ku -h
ku -help
ku --help
ku help
```

`ku create` 创建新目录，`ku init` 初始化当前目录，`ku run` 只负责运行当前 package 或指定 `.ku` 文件，不再承担创建项目语义。内置模板：

```txt
basic    minimal Ku project
cli      command line tool
http     HTTP server
json     JSON processing example
fs       file processing example
lib      library project
```

模板项目生成 `ku.mod` 和 `src/main.ku`；`http` 模板会生成可直接 `ku check` / `ku run` / `ku build` 的 HTTP server 示例。

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

`ku check --deny-unused` 是严格 unused 检查第一阶段：本文件局部变量/常量如果声明后没有被读取，会报 `E0905`；用 `_` 或 `_name` 表示明确丢弃。函数参数暂不纳入错误，避免破坏 HTTP handler `(req, res)` 形状。未使用 import 还未默认开启，因为 import expansion 需要先保留 import-origin，避免跨文件导出和 std namespace 误判。

`ku llvm file.ku` 在源文件旁输出 `.ll`，不要求本机安装 LLVM。当前文本后端支持 `int/bool/str`、普通函数、局部变量、直接调用、`return`、`if/while`、`print`、非递归 struct 值与字段读写，以及 `Result<int|bool|str|struct>` 的 `ok`、`fail`、`?` 和错误传播。数组、enum、闭包、HTTP 和 async 仍会明确报不支持。后端会拒绝递归值 struct、缺失/重复 CFG block 和无条件自跳，避免生成明显错误或永久循环的 `.ll`。golden test 不依赖外部工具；检测到 `llvm-as` 时会额外验证生成文本。

`ku build` 当前生成解释器打包型可执行文件。

构建入口规则：

```txt
ku build src/main.ku      使用显式传入文件
ku build .                从指定目录向上找 ku.mod
ku build                  从当前目录向上找 ku.mod
```

有 `ku.mod` 时，入口为 `root + main`；`root` 默认 `src`，`main` 默认 `main.ku`。输出目录为 `out`，默认 `.ku/build`。单文件没有 `ku.mod` 时，输出根目录为源文件所在目录下的 `.ku/build`。

默认 profile 是 `debug`：

```txt
ku build                  -> .ku/build/debug/<package_name>
ku build --release        -> .ku/build/release/<package_name>
ku build --target x86_64-windows --release
                          -> .ku/build/x86_64-windows/release/<package_name>.exe
```

Windows host 或 Windows target 会自动追加 `.exe`。`-o` / `--output` 可以覆盖输出路径：

```powershell
ku build -o app.exe src/main.ku
ku build --release -o dist/app.exe
```

调试产物：

```txt
--emit-ir      写入 .ku/build/<profile>/ir/main.ir
--emit-c       写入 .ku/build/<profile>/c/main.c
--emit-llvm    写入 .ku/build/<profile>/llvm/main.ll
```

`--backend c` 会使用 prototype C 后端生成 C 后再调用 C 编译器。查找顺序为 `KU_CC`、`zig cc`、`clang`、`cc`、`gcc`、`cl`；找不到或编译失败会给出修改方向。默认 backend 仍是解释器 wrapper，因为完整 native closure / KuString / dynamic object / async ABI 尚未完成。`ku run build` 仅作为兼容别名保留，会提示改用 `ku build`。

0.0.15 build 的重要边界：默认生成的是“解释器打包型二进制”，入口源码会嵌入 wrapper；带 import 的程序仍会按原源码路径读取依赖，因此还不是最终“不依赖 Ku 源码文件”的 native binary。最终 native build 仍需要 import graph 打包、runtime ABI lowering、closure/native string/object/async lowering 和增量缓存继续补齐。

`ku build --native` 当前输出 prototype C 源码，支持：

```txt
int / bool / str
非递归 struct layout / literal / field read / field write
局部变量
直接函数调用
print
return
if / while
统一 KuError / Result ABI
ok / err / ?
错误传播
try / catch / finally
return-through-finally
Result<int/bool/str/null/array/struct/enum>
有界 array runtime / length / read-write bounds check
默认 move / 显式 clone() / 自动 drop
嵌套 owned array 递归 clone/drop
enum tag + payload layout
unit / payload / nested enum match lowering
系统 int main(void) wrapper
```

native C 当前仍是 prototype，但同步所有权和错误流已闭环：Copy 类型是 `int/bool/float/null`；`str/array/object/struct/enum/Result/task` 在语言检查层按 Owned 处理，赋值和传参默认 move。`str/array/object/struct/enum/Result` 的复制必须显式 `.clone()`，`Task<T>` 是 move-only，不能 clone，`await` 会消费 task。checker 会拒绝 use-after-move、重复消费 Result、重复 await task、match 分支漏合并和循环回边重复 move。C 后端为 array、named value 和 Result 生成 move/clone/drop，赋值先物化 RHS 再 drop 旧值，解构交换先物化全部 RHS；嵌套 owned array 递归 clone/drop。

`.clone()` 规则：

- Copy 类型 `int/bool/float/null` 可以写 `.clone()`，语义等同复制，优化阶段应直接消掉。
- `str.clone()` 语义是得到独立字符串值；解释器当前用 host String 深拷贝，native 后续固定为 `KuString { ptr, len, capacity, storage }`，字面量 `static` clone 零分配，运行时 `owned` clone 复制 UTF-8 bytes。
- `[T].clone()` 生成新数组并递归 clone 元素；如果中途失败，已经 clone 的元素必须按 drop 路径清理。
- `object.clone()` 生成新动态对象并递归 clone key/value；第一阶段禁止 native 自引用/cycle，后续如需要再设计 cycle 策略。
- `struct.clone()` 按字段递归 clone；字段不可 clone 时错误定位到字段。
- `enum.clone()` 只 clone 当前 tag 对应 payload，不访问其它 variant payload。
- `Result<T>.clone()` 只 clone 当前 ok payload；Error payload 按 Error 对象 clone。
- `function` 值在语言方向上是 Owned，未来 native closure clone 只复制函数入口并增加小范围 RC env，不深拷贝捕获环境；捕获 binding 默认共享。
- `Task<T>` 不允许 clone；`await task` 消费 task，普通 task 只能 await 一次。
- 优化方向包括 Copy clone 消除、源值随后不再使用时的 clone-to-move、临时 clone 消除、return clone-to-move、static string clone 零分配、struct clone inline、array/object 预分配，以及不需要的 drop/clone 消除。

native Error ABI 是 `KuError { domain, code, message }`。`?` 只传播 Error，不要求来源和目标 Result payload 相同；`try/catch/finally` 的普通完成、错误和 return 都经过对应 finally block。array 所有索引检查负数和 `index >= len`；enum 使用 `tag + union payload`。递归值 struct/enum、native closure、动态 object ABI 和 async native lowering仍明确拒绝。native `str` 暂时仍使用只读 C 字符串原型，正式 owned `KuString` ABI 已进入执行队列；在 KuString lowering 完成前，native C 会明确拒绝字符串拼接，不生成 `const char* + const char*` 这类错误 C。

## 17. 资源保护

当前资源上限：

```txt
最大 token 数: 100000
最大解析深度: 32
最大检查深度: 32
最大执行步数: 无固定硬上限；仍受取消、timeout、调用深度和宿主环境限制
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
async active task 上限: 1024
async task queue 上限: 1024
async blocking queue 上限: 1024
async await 深度上限: 64
```

async runtime snapshot 会报告 active/registered/queued task、等待边、blocking queue/running job、worker 数和累计 accepted/rejected/finished。shutdown 同时等待 task 与 blocking job；已经开始且不配合取消的系统调用不能被强杀，超过有界窗口返回 `task/shutdown_timeout`。

2026-06-22 的 release 压力测试使用 15 个生产者并发提交 1,000,000 个 task demand，并让已接纳任务同时保持 active。结果：

```txt
peak_active: 1024
accepted: 1024
rejected_limit: 998976
rejected_queue/internal: 0
submit: 126 ms
runtime total: 379 ms
外部观测 wall time: 648 ms
CPU time: 2312 ms
峰值 working set: 6.07 MiB
峰值 private memory: 1.91 MiB
观测峰值线程数: 13
```

这个测试验证的是“百万并发需求下仍保持 1024 有界接纳”，不是允许一百万个活跃协程常驻内存。超限请求立即得到结构化拒绝，不排队死等、不无限重试。

## 18. 当前不支持 / 未完成

```txt
LLVM 数组、enum、闭包、HTTP、async lowering
LLVM 递归 struct 和更复杂 Result payload
完整 native C 后端
registry 根公钥配置、key rotation/revocation、归档格式、受限解包和 CLI 远程 import 串联
native closure ABI、captured env 和函数类型语法
native owned string 与动态 object ABI
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
stmt          ::= var | assign | compound_assign | destructure | object_destructure | inc | if | while | for | break | continue | try | fail | panic | return | print | expr
var           ::= IDENT ':' type ('=' expr)?
assign        ::= assign_target '=' expr
compound_assign ::= assign_target ('+=' | '-=' | '*=' | '/=' | '%=') expr
destructure   ::= (IDENT | '_') (',' (IDENT | '_'))+ '=' expr (',' expr)+
object_destructure ::= '{' object_binding (',' object_binding)* (',' rest_binding)? '}' '=' expr
object_binding ::= IDENT (':' IDENT_OR_DISCARD)? ('=' expr)?
rest_binding  ::= '...' IDENT_OR_DISCARD
IDENT_OR_DISCARD ::= IDENT | '_'
inc           ::= assign_target ('++' | '--') | ('++' | '--') assign_target
assign_target ::= IDENT | expr '[' expr ']' | expr '.' IDENT
body          ::= block | stmt
if            ::= 'if' '(' expr ')' body ('else' (if | body))?
while         ::= 'while' '(' expr ')' body
for           ::= 'for' IDENT 'in' expr body
try           ::= 'try' block ('catch' '(' IDENT ')' block)? ('finally' block)?
return        ::= 'return' expr?
print         ::= 'print' expr | 'print' '(' expr ')'
expr          ::= literal | IDENT | call | field | index | optional_index | await | array | object | struct_lit | match | fn_expr | arrow | unary | binary | '(' expr ')'
match         ::= 'match' expr '{' arm* '}'
arm           ::= pattern ('if' expr)? '=>' expr
pattern       ::= '_' | IDENT | literal | IDENT '.' IDENT ('(' pattern* ')')? | IDENT '.' IDENT '.' IDENT ('(' pattern* ')')?
optional_index ::= expr '[' expr ']' '?'
await         ::= 'await' expr
arrow         ::= IDENT (':' type)? '=>' (expr | block)
                | '(' params? ')' (':' type)? '=>' (expr | block)
fn_expr       ::= 'fn' '(' params? ')' (':' type)? block
```
