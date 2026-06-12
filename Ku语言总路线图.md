# ku 语言总路线图

> 目标：从 0 开发一门叫 **ku** 的语言。  
> 核心要求：**稳定、语法简单、占用资源小、速度非常快**。  
> 本文档用于固定大方向，具体细节后续可以逐项找 AI 拆方案。
> 当前语法修订：0.0.3 语法边界已废弃 `let` / `let mut`。变量使用 `name = value`、`name:type = value`、`name:type`；常量使用全大写命名规则。本文中较早出现的 `let` 示例属于历史草案，不代表当前语法。

---

## 1. ku 的总定位

ku 不应该一开始就做成“什么都能干”的大语言。

ku 的第一定位应该是：

```txt
简单、稳定、低资源、高性能的编译型语言
```

更具体一点：

```txt
ku = Go 的简单工程体验 + Zig/C 的低资源控制 + Rust 的稳定意识
```

但 ku 第一版不要直接模仿 Rust 的所有权系统，也不要做成 Python 那样的完全动态脚本语言。

ku 长期适合做：

```txt
命令行工具
高性能服务端程序
系统工具
自动化任务
嵌入式脚本
WASM 小程序
语言工具链
```

ku 第一阶段的目标不是打败 Rust、Go、C++，而是：

```txt
做出一门语法简单、能运行、能编译、低资源、可长期演进的小语言。
```

---

## 2. ku 的四个核心原则

ku 的所有设计都必须围绕四个关键词：

```txt
稳定
简单
低占用
高速度
```

### 2.1 稳定

稳定不是功能多，而是：

```txt
语法规则少
错误提示清楚
类型尽量提前检查
标准库行为可预测
编译器输出稳定
版本升级尽量兼容
```

不要为了炫技加入大量复杂语法。

### 2.2 语法简单

ku 语法应该做到：

```txt
一眼能看懂
关键字少
符号魔法少
格式统一
工具自动格式化
```

推荐风格：

```ku
fn add(a: int, b: int): int {
    return a + b
}

fn main() {
    let result = add(10, 20)
    print(result)
}
```

### 2.3 占用资源小

ku 应该避免大型运行时。

目标：

```txt
编译产物尽量小
运行时尽量小
不依赖大型虚拟机
不默认引入重型 GC
标准库小而稳
默认编译成原生程序
```

### 2.4 速度非常快

ku 的速度包括两方面：

```txt
程序运行速度快
编译速度也尽量快
```

长期依靠：

```txt
静态类型
提前编译
小运行时
少 GC 或无 GC
良好内存布局
高效标准库
```

---

## 3. ku 应该是什么类型的语言

ku 最适合走：

```txt
静态类型 + 编译型 + 小运行时 + 可选高级能力
```

第一版可以用解释器验证设计，但最终目标应该是编译型语言。

推荐路线：

```txt
Rust 写第一代解释器
→ 稳定语法和语义
→ 加类型检查
→ 编译到 C
→ 后期支持 WASM / LLVM
→ 用 ku 写 ku 自己的工具链
```

不建议第一版直接接 LLVM，因为复杂度太高。中期更现实的是：

```txt
ku 源码
  ↓
Lexer
  ↓
Parser
  ↓
AST
  ↓
Type Checker
  ↓
C Backend
  ↓
clang / gcc
  ↓
可执行文件
```

---

## 4. ku 的长期架构

最终架构可以这样设计：

```txt
                ku 源代码
                    ↓
                 Lexer
                    ↓
                 Parser
                    ↓
                  AST
                    ↓
              Type Checker
                    ↓
                   IR
          ┌─────────┼─────────┐
          ↓         ↓         ↓
      Interpreter  C 后端   WASM 后端
          ↓         ↓         ↓
        快速运行   原生程序  浏览器/沙盒
```

后期再扩展：

```txt
LLVM 后端
字节码 VM
LSP
Formatter
Package Manager
Debugger
Profiler
Benchmark Tool
```

第一年不要全部做，先跑通主线。

---

## 5. 第一代为什么用 Rust 写

ku 第一代解释器 / 编译器建议用 Rust 写。

原因：

```txt
Rust 性能好
Rust 内存安全
Rust 适合写编译器
Rust 的 enum / match 很适合 AST
Rust 可以编译成单文件工具
后期做 CLI、LSP、格式化工具也方便
```

第一代不要用 ku 写，因为 ku 还不存在。

正确路线是：

```txt
Rust 写 ku 第一代工具链
→ ku 语言逐渐成熟
→ ku 写标准库
→ ku 写一部分工具
→ ku 重写部分编译器
→ 最终 ku 编译 ku
```

这叫自举，但自举是后期目标。

---

## 6. ku 第一版支持什么

ku v0.1 必须小。

只支持：

```txt
数字
字符串
布尔值
变量
函数
if
while
return
print
基础错误提示
文件运行
```

示例：

```ku
fn main() {
    let name = "ku"
    print("Hello " + name)
}
```

函数：

```ku
fn add(a: int, b: int): int {
    return a + b
}

fn main() {
    print(add(10, 20))
}

fun = ()=>{
}

```

循环：

```ku
fn main() {
    let mut i = 0

    while i < 5 {
        print(i)
        i = i + 1
    }
}
```

v0.1 完成标准：

```txt
ku run examples/hello.ku 可以运行
ku run examples/fib.ku 可以运行
ku check examples/error.ku 可以报出清楚错误
```

---

## 7. ku 第一版不要做什么

v0.1 不要做：

```txt
泛型
宏系统
异步
多线程运行时
复杂 GC
所有权系统
LLVM
JIT
包管理器
复杂模块系统
复杂 trait / interface
高级优化器
```

第一版目标：

```txt
小，但完整
简单，但稳定
能跑真实示例
```

---

## 8. ku 的语法方向

### 8.1 文件扩展名

建议：

```txt
.ku
```

示例：

```txt
main.ku
server.ku
tool.ku
```

### 8.2 变量

推荐：

```ku
let name = "Jason"
let age = 18
let mut count = 0
```

规则：

```txt
let 默认不可变
let mut 表示可变
默认不可变可以提升稳定性
第一版不要做复杂借用检查
```

### 8.3 类型标注

允许类型推导，但支持显式类型：

```ku
let name: string = "Jason"
let age: int = 18
let price: float = 9.9
let ok: bool = true
```

函数参数和返回值建议明确：

```ku
fn add(a: int, b: int): int {
    return a + b
}
```

### 8.4 函数

```ku
fn greet(name: string): string {
    return "Hello " + name
}
```

没有返回值：

```ku
fn log(message: string) {
    print(message)
}
```

### 8.5 条件语句

```ku
if age >= 18 {
    print("adult")
} else {
    print("child")
}
```

### 8.6 循环

第一版只做 `while`：

```ku
while i < 10 {
    print(i)
    i = i + 1
}
```

后期再加 `for`：

```ku
for item in items {
    print(item)
}
```

### 8.7 注释

```ku
// 单行注释

/*
多行注释
*/
```

### 8.8 数组

第二阶段加入：

```ku
let nums = [1, 2, 3]
print(nums[0])
```

### 8.9 结构体

第三阶段加入：

```ku
struct User {
    name: string
    age: int
}

fn main() {
    let user = User {
        name: "Jason",
        age: 18
    }

    print(user.name)
}
```

---

## 9. ku 的类型系统方向

ku 想稳定和高性能，必须有类型系统。

### v0.1 类型

```txt
int
float
bool
string
nil
```

### v0.2 类型

```txt
array
map
function
```

### v0.3 类型

```txt
struct
enum
option
result
```

### v0.4 以后

```txt
generic
interface / trait
module type
```

类型系统原则：

```txt
不追求极端复杂
优先让错误提前暴露
不要一开始模仿 Rust 所有权
不要一开始做复杂泛型
函数签名尽量明确
```

---

## 10. ku 的内存管理方向

ku 的目标是低占用和高速度，所以内存管理很关键。

但第一版不要直接做 Rust 式所有权。

### 可选路线

#### 简单 GC

优点：

```txt
开发快
语言简单
用户心智负担低
```

缺点：

```txt
可能有停顿
资源占用更高
不适合极致性能
```

#### 引用计数 ARC

优点：

```txt
实现相对简单
资源释放及时
比完整 GC 更可控
```

缺点：

```txt
循环引用需要处理
性能有一定开销
```

#### 手动内存管理

优点：

```txt
性能高
资源控制强
运行时小
```

缺点：

```txt
用户容易写错
稳定性差
学习成本高
```

#### 所有权系统

优点：

```txt
安全
高性能
低运行时成本
```

缺点：

```txt
设计难度极高
实现难度极高
用户学习成本高
```

### ku 推荐路线

早期推荐：

```txt
值类型优先
字符串和复杂对象使用简单运行时管理
先用保守方案跑通
中期使用 ARC 或简单 GC
后期再探索所有权 / 区域内存 / 手动控制
```

第一年不要死磕内存模型。先让语言跑起来、编译起来、能写程序。

---

## 11. ku 的执行路线

### 阶段 1：AST 解释器

```txt
ku 源码
  ↓
Token
  ↓
AST
  ↓
直接执行
```

作用：

```txt
验证语法
验证语义
方便调试
快速改设计
```

### 阶段 2：类型检查器

```txt
ku 源码
  ↓
AST
  ↓
Type Checker
  ↓
执行 / 编译
```

作用：

```txt
提升稳定性
为编译后端做准备
```

### 阶段 3：C 后端

```txt
ku 源码
  ↓
AST
  ↓
Type Checker
  ↓
C Code
  ↓
clang / gcc
  ↓
Binary
```

作用：

```txt
获得原生性能
降低后端难度
借助 C 生态
```

### 阶段 4：WASM 后端

```txt
ku 源码
  ↓
IR
  ↓
WASM
```

作用：

```txt
浏览器运行
边缘计算
插件沙盒
在线 Playground
```

### 阶段 5：LLVM 后端

后期再考虑：

```txt
ku 源码
  ↓
IR
  ↓
LLVM IR
  ↓
机器码
```

作用：

```txt
更强优化
更专业代码生成
更完整的编译型语言能力
```

---

## 12. ku 编译器目录设计

建议第一代 Rust 项目结构：

```txt
ku/
  Cargo.toml
  src/
    main.rs
    cli.rs

    token.rs
    lexer.rs

    ast.rs
    parser.rs

    type.rs
    checker.rs

    value.rs
    env.rs
    interpreter.rs

    ir.rs
    codegen_c.rs

    error.rs
    span.rs

  examples/
    hello.ku
    fib.ku
    loop.ku
    function.ku

  tests/
    lexer_test.rs
    parser_test.rs
    checker_test.rs
    runtime_test.rs

  docs/
    syntax.md
    roadmap.md
    stdlib.md
```

模块职责：

```txt
token.rs        定义 Token
lexer.rs        源码转 Token
ast.rs          定义 AST 节点
parser.rs       Token 转 AST
type.rs         定义 ku 类型
checker.rs      类型检查
value.rs        解释器运行时值
env.rs          作用域和变量环境
interpreter.rs  AST 解释执行
ir.rs           中间表示
codegen_c.rs    C 代码生成
error.rs        错误系统
span.rs         源码位置追踪
cli.rs          命令行入口
```

---

## 13. ku 命令行工具设计

工具名建议：

```txt
ku
```

命令设计：

```bash
ku run main.ku
ku check main.ku
ku build main.ku
ku fmt main.ku
ku test
ku version
```

### v0.1 命令

```bash
ku run main.ku
ku check main.ku
ku version
```

### v0.2 命令

```bash
ku fmt main.ku
ku test
```

### v0.3 命令

```bash
ku build main.ku
```

### v0.4 命令

```bash
ku init app
ku add package
```

不要一开始做复杂包管理。

---

## 14. ku 标准库方向

标准库不要贪多，要围绕真实场景。

### 第一批标准库

```txt
print
string
array
math
fs
path
time
json
process
```

### 第二批标准库

```txt
http
net
crypto
test
log
env
cli
```

标准库原则：

```txt
小而稳
命名一致
文档清晰
不引入过多魔法
默认低开销
高级功能放扩展库
```

示例：

```ku
fn main() {
    let text = fs.read("config.json")
    let config = json.parse(text)
    print(config)
}
```

---

## 15. ku 的性能方向

### v0.1

目标不是极致快，而是架构正确：

```txt
能运行
不崩溃
错误清楚
示例稳定
```

### v0.2

优化解释器和类型检查：

```txt
减少无意义复制
优化字符串处理
优化作用域查找
```

### v0.3

编译到 C：

```txt
获得接近 C 的基础性能
减少运行时开销
生成可执行文件
```

### v0.4

建立基准测试：

```txt
启动速度
内存占用
循环性能
函数调用性能
字符串性能
文件 IO 性能
```

### v1.0

关注：

```txt
编译速度
运行速度
二进制体积
内存峰值
错误提示质量
稳定性
```

---

## 16. ku 的稳定性工程

稳定性需要工程手段保证。

### 测试

必须有：

```txt
Lexer 测试
Parser 测试
Type Checker 测试
Interpreter 测试
Codegen C 测试
标准库测试
错误提示测试
```

### 示例程序

必须有：

```txt
hello.ku
fib.ku
calculator.ku
file_read.ku
json_format.ku
http_server.ku
cli_tool.ku
```

### 版本策略

```txt
v0.1 语言原型
v0.2 类型系统
v0.3 C 后端
v0.4 标准库
v0.5 工具链
v1.0 稳定版
```

v1.0 前可以改语法。v1.0 后要慎重破坏兼容。

---

## 17. ku 的错误提示方向

错误提示是新语言的核心体验。

不要只报：

```txt
Syntax Error
```

应该报：

```txt
error: expected expression after '='
  --> main.ku:3:12
   |
 3 | let name =
   |            expected expression here
```

错误系统从第一版就要记录：

```txt
文件名
行号
列号
错误类型
错误消息
错误建议
源码片段
```

---

## 18. ku 的格式化工具方向

ku 语法简单，也应该强制统一格式。

命令：

```bash
ku fmt main.ku
```

格式规则建议：

```txt
4 空格缩进
函数大括号不换行
if / while 大括号不换行
字符串双引号
每行尽量短
```

格式化工具可以后期做，但语法设计时就要考虑可格式化。

---

## 19. ku 的包管理方向

包管理不要第一版做。

后期可以设计：

```txt
ku.mod
```

示例：

```toml
name = "hello"
version = "0.1.0"

[dependencies]
json = "0.1"
http = "0.1"
```

命令：

```bash
ku init hello
ku add json
ku build
```

包管理至少等 ku 能编译到 C 或有稳定标准库后再做。

---

## 20. ku 开发阶段路线图

### 阶段 0：准备期

目标：

```txt
明确语言定位
确定语法风格
建立 Rust 项目
写 docs/roadmap.md
```

产出：

```txt
ku 语言总路线图
ku 语法草案
Rust 项目骨架
```

### 阶段 1：Lexer

目标：

```txt
把 ku 源码切成 Token
```

支持：

```txt
关键字
标识符
数字
字符串
符号
注释
行列号
```

完成标准：

```txt
可以正确识别 20 个示例文件
错误字符能给出位置
```

### 阶段 2：Parser + AST

目标：

```txt
把 Token 变成 AST
```

支持：

```txt
变量声明
函数声明
if
while
return
表达式
函数调用
```

完成标准：

```txt
可以把 hello.ku / fib.ku 解析成 AST
语法错误能提示位置
```

### 阶段 3：AST 解释器

目标：

```txt
直接执行 AST
```

支持：

```txt
变量
作用域
函数调用
if
while
return
print
```

完成标准：

```txt
ku run hello.ku 正常输出
ku run fib.ku 正常输出
```

### 阶段 4：类型检查器

目标：

```txt
运行前检查类型错误
```

支持：

```txt
int
float
bool
string
函数参数类型
函数返回类型
变量类型推导
```

完成标准：

```txt
1 + "hello" 能报错
函数参数传错类型能报错
return 类型不对能报错
```

### 阶段 5：C 后端

目标：

```txt
把 ku 编译成 C
```

流程：

```txt
ku
  ↓
AST
  ↓
Type Checker
  ↓
C Code
  ↓
clang
  ↓
binary
```

完成标准：

```bash
ku build main.ku
./main
```

可以运行。

### 阶段 6：标准库

目标：

```txt
让 ku 能写真实小工具
```

优先支持：

```txt
print
fs.read
fs.write
string.split
string.trim
array.len
json.parse
time.now
```

完成标准：

```txt
可以写 JSON 格式化工具
可以写文件批量处理工具
可以写简单 CLI
```

### 阶段 7：工具链

目标：

```txt
让 ku 可以被正常开发
```

支持：

```bash
ku run
ku check
ku build
ku fmt
ku test
```

完成标准：

```txt
别人下载 ku 后，可以照文档写出第一个 ku 程序
```

### 阶段 8：WASM / VM / LLVM

按需求选择：

```txt
想做浏览器和插件：优先 WASM
想做脚本和嵌入：优先 VM
想做极致性能：考虑 LLVM
```

不要三个一起做。

### 阶段 9：ku 写自己的生态

当 ku 能写真实程序后，开始用 ku 写：

```txt
标准库扩展
测试工具
文档生成器
包管理脚本
Formatter 的一部分
简单代码生成器
```

不要一上来重写编译器。

### 阶段 10：自举

最终目标：

```txt
ku 编译器可以编译 ku 编译器
```

路线：

```txt
Rust 版 kuc
  ↓
编译 ku 写的新 kuc
  ↓
新 kuc 编译自己
```

完成自举后，ku 才真正进入成熟语言阶段。

---

## 21. ku 版本目标

### ku v0.1

目标：

```txt
一个 Rust 写的 ku 解释器，可以运行基础 ku 程序。
```

必须包含：

```txt
ku run
lexer
parser
ast
interpreter
变量
函数
if
while
print
错误提示
示例程序
基础测试
```

不包含：

```txt
编译到 C
泛型
结构体
模块
包管理
异步
网络
复杂标准库
```

示例：

```ku
fn fib(n: int): int {
    if n <= 1 {
        return n
    }

    return fib(n - 1) + fib(n - 2)
}

fn main() {
    print(fib(10))
}
```

### ku v0.2

加入类型检查和更多数据结构。

包含：

```txt
类型检查器
数组
字符串基础方法
更好的错误提示
更多测试
语法文档
```

目标：

```txt
ku 代码写错时，尽量在运行前发现。
```

### ku v0.3

开始编译到 C。

包含：

```txt
C 后端
ku build
生成可执行文件
基础运行时
和 clang/gcc 对接
```

目标：

```txt
ku 从解释型原型变成真正编译型语言雏形。
```

### ku v0.4

建设标准库。

包含：

```txt
fs
string
array
json
time
cli
test
```

目标：

```txt
可以用 ku 写真实小工具。
```

### ku v0.5

建设工具链。

包含：

```txt
ku fmt
ku test
VS Code 语法高亮
错误提示优化
文档站点
```

目标：

```txt
ku 可以被别人试用。
```

### ku v1.0

稳定版必须满足：

```txt
语法稳定
标准库稳定
错误提示稳定
编译到 C 稳定
能写 CLI 工具
能写简单服务端程序
性能和资源占用有数据
文档完整
有测试体系
```

---

## 22. ku 需要避免的坑

### 坑 1：一开始做太大

不要第一版就想做成 Go + Rust + Python + TypeScript。

第一版越大，越容易失败。

### 坑 2：太早做 LLVM

LLVM 很强，但第一版会拖慢进度。

先编译到 C 更现实。

### 坑 3：太早做包管理

没有稳定语言和标准库，包管理没有意义。

### 坑 4：语法太花

新语言最怕语法炫酷但难懂。

ku 要简单直接。

### 坑 5：没有真实示例

只写编译器，不写 ku 程序，会导致语言设计脱离实际。

每个版本都要写真实示例。

### 坑 6：不重视错误提示

错误提示差，语言体验会非常差。

### 坑 7：过早自举

自举是后期荣誉，不是早期任务。

---

## 23. ku 应该参考哪些语言

### Go

学习：

```txt
简单语法
快速编译
工程化工具链
标准库设计
```

### Zig

学习：

```txt
低资源
显式控制
编译期能力
无隐藏运行时
```

但不要一开始学复杂 comptime。

### Rust

学习：

```txt
安全意识
错误提示
工程质量
工具链
```

但不要一开始学完整所有权系统和生命周期复杂度。

### C

学习：

```txt
底层模型
内存布局
ABI
编译到 C 的后端思路
```

但不要学 C 的不安全默认和头文件复杂性。

### Python / JavaScript

学习：

```txt
开发体验
表达简洁
生态易用性
```

但不要学动态类型导致的大项目不稳定和运行时过重。

---

## 24. ku 项目每一步可以问 AI 什么

不要问：

```txt
帮我做一门语言
```

要拆开问：

```txt
用 Rust 给 ku 写 Lexer，怎么设计 Token？
ku 的 Parser 应该手写递归下降还是 Pratt Parser？
ku 的 AST enum 应该怎么设计？
ku 的 Env 作用域怎么设计？
ku 函数调用和 return 怎么实现？
ku 类型检查器怎么设计？
ku 怎么从 AST 生成 C 代码？
ku 字符串运行时怎么设计？
ku 错误提示 span 怎么做？
ku 标准库第一批应该怎么设计？
ku 怎么做 ku run / ku check / ku build？
```

大方向自己掌握，细节让 AI 分模块解决。

---

## 25. ku 的最终主线

ku 的主线不要变：

```txt
Rust 写第一代解释器
→ 稳定 ku 语法
→ 加类型检查
→ 编译到 C
→ 建设标准库
→ 建设工具链
→ 支持 WASM / VM / LLVM
→ 用 ku 写自己的生态
→ 最终自举
```

ku 的核心价值不要变：

```txt
稳定
简单
低资源
高速度
```

ku 的第一阶段目标不要变：

```txt
先做一个小而完整的 ku。
```

ku 的长期目标是：

```txt
一门能写真实程序、能编译成高性能程序、运行时很小、语法简单稳定的语言。
```

---

# 最终结论

现在最应该做的是：

```txt
用 Rust 写 ku v0.1 的解释器。
```

ku v0.1 的目标不是极致性能，而是：

```txt
语法跑通
模型跑通
错误提示跑通
示例跑通
```

ku v0.3 以后再追求：

```txt
编译到 C
低资源
高速度
```

ku v1.0 再追求：

```txt
稳定语法
稳定标准库
稳定工具链
真实项目可用
```

一句话：

```txt
ku 先小，再稳；先跑，再快；先用 Rust 造出来，再慢慢让 ku 自己参与进来。
```
