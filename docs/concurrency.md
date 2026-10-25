# Ku 并发与 HTTP 压测

> v0.0.18 按 [冻结清单](v0.0.18.md#冻结范围与原任务清单) 交付已实现子集；完整 M:N、通用网络调度和未接入组合仍未完成。历史 HTTP 504 和正式 30 分钟未通过状态见 [已知问题](v0.0.18-known-issues.md)。


Ku 的用户级并发模型保持简单：业务代码通过 `async fn` 启动一次性 task，然后用 `await task` 或 `await task?` 等待结果。用户不能手动 `task.spawn`、`Task.new`、`runtime.schedule` 或 `thread.spawn`。

## task 规则

- `async fn` 调用会立即启动一个轻量 task。
- task 是句柄，不是线程。
- 普通 task 是 move-only，不能隐式复制，不能 clone。
- `await task` 会消费 task；普通 task 只能 await 一次。
- `await task?` 等价于 `(await task)?`。
- HTTP server 内部可以使用 task，但普通 handler 不需要管理 task。

## 进程内存限制

`ku supervise` 以整个程序为 worker 单元，由 Linux cgroup v2 或 Windows Job Object 执行内存限制；运行时账本和单 Task 预算仍分别计量。第三方库在 worker 内的分配也受对应系统指标约束。原生 macOS 暂不提供此模式，明确拒绝启动；详见 [进程执行与内存限制](process-execution.md)。

## Task 作用域与取消合同

v0.0.18 第二阶段已采用以下规则；这不表示所有后端已实现。完整合同见
[Task 所有权、取消与清理预算](semantics.md#task-所有权取消与清理预算)。

- 当前持有 Task 句柄的所有权作用域负责清理；move 会转移责任，不永久绑定创建位置。
  作用域正常结束、return、错误传播、panic、超时或取消时，取消仍持有的未完成 Task，释放已完成但未 await 的 payload。
- 兄弟失败本身不取消其他兄弟。父任务处理错误后可继续；只有父作用域因错误传播退出时，才清理它仍持有的兄弟任务。
- cancel/timeout 是不可由普通 catch 捕获的内部终止，与完成竞争唯一终态。
  finally 的 return、fail、panic 和迟到成功均不能覆盖已经获准的取消/超时。
- 一次根取消只有默认总计一秒的单调时钟绝对预算，所有子任务、嵌套 finally 和 drop 共用，
  外层 shutdown 剩余预算更短则从短；不能按任务逐个续期。超时记录未完成清理，不能冒充成功。
- 取消展开先请求 owned 子任务取消，再由内向外执行 finally，随后 drop 本作用域局部。
  清理期间禁止新建 Task、await 或提交新的 sleep/timer、网络等待和 blocking job；同步 close/drop 与有限计算仍受预算约束。

解释器与各 native 切片的执行证据见实施记录；不同切片的测试结果不能互相替代。
v0.0.18 开发分支已接通 native C 的有界 worker 组有限源码子集：AST 经独立 Task IR
生成 Start、Move、Await、函数退出与 If 分支的正常 scope drain，不嵌入解释器或 runner 源码。
既有检查点证据与本次源码 If、Owned/Pending 专项分开记录；专项不能替代本次完整
workspace/native 全集或精确提交三系统 CI/sanitizer 验收，不是正式发布。
实际结果与历史失败/修复的分开证据见 [工作日志](v0.0.18-worklog.md)。
结果等待与 ACK 等待复用固定槽位，不按每次等待分配；函数退出先移交全部 sibling，
再等待逻辑清理 ACK。迟到 observer 可以保留控制存储，但不能保留已丢弃的 payload。
正常 scope 超期是外层运行时 `task/shutdown_timeout`，不同于业务 Result.err，
也不会把正常父任务标成 Cancelled；已有取消原因不变，绝对清理期限只收紧、不续期。
LLVM、`ku ir` 和 `--emit-ir` 的同步 IR 路径仍拒绝 async。
已经进入系统或外部库的阻塞操作仍不能硬杀，迟到结果只能清理，不得恢复已取消任务。

### 当前 native C 源码子集

import 展开后只能有顶层、非泛型 async 函数；入口必须是无参数的
`async fn main(): null!`。函数参数为 `int/bool/null/str`、对应单层 Result 或 owned 平面数组，
返回类型必须显式为 primitive 或平面数组的 `T!`。函数体支持局部绑定、源码 `if` / `else` / `while`、整数 `for` 及词法作用域、循环内的 `break` / `continue`、直接 async 调用、
Task move、Await、`ok`、`?`、primitive print/println、显式 return，以及字符串常量 fail。
Copy 表达式支持 int 的一元 `-`、`+ - * / %`、`== != < <= > >=`，以及 bool 的
`!`、`== !=`、`&& ||`；不做 bool/int 隐式转换。整数运算先检查边界，溢出和除/余零
分别报告 `integer overflow`、`division by zero`；`MIN / -1` 与 `MIN % -1` 均是溢出。
这些是外层 runtime failure，不是普通 Result.err：即使写 `result = await child`
而不写 `?`，也不能收到该错误后继续业务语句；仍经原有 owner/child 清理链退出。

普通二元式先完整求值并保存左值，再求右值；左 Copy 值在右侧 Await 挂起期间保活。
`&& ||` 只执行必要的右侧表达式，未选中的 Start、实参 move、Await、Print 和 `?`
都不执行，但右侧仍受静态类型与预算检查。若 Task 在逻辑表达式前已经创建，短路
跳过它的 await 不免除当前 scope 的最终清理责任，也不会撤销它的立即启动；
该 Task 可能在 scope 发起取消前已经执行，不能据短路推断它没有产生副作用。
字符串接通 `text.len()`、`text.byte_len()`、`text.clone()` 和 `left + right`。
len 计 Unicode 标量，byte_len 读取 UTF-8 字节数，均不消费输入；静态 clone 共享
只读正文，Owned clone 独立复制（非空指针的空串计真实 1 字节）。拼接在右侧求值前
clone 左侧快照；右侧 Await 或显式 move 原值不会破坏它。成功后释放专用快照；
失败、提前退出或取消沿原初始化位清理。快照、输入和新结果同时存活时都计入原预算，
分配不足为外层 memory_limit/out_of_memory，不变成普通业务 Err。长度溢出在分配前
拒绝。没有新增字符串 is_empty 方法，判断空串可用 `text.byte_len() == 0`。

```ku
async fn Suffix(text: str): str! { return ok(text) }
async fn main(): null! {
    text = "中😀"
    println(text.len())       // 2
    println(text.byte_len())  // 7
    joined = text + (await Suffix("!"))?
    println(text)             // 原值仍可读取
    println(joined)
    return ok(null)
}
```
接纳/OOM 拒绝仍消费源码已经移动的实参，返回可 await 的失败 Task，错误不再申请内存。

If 包括嵌套 `else if`，条件必须是 bool，可使用当前子集的 Await、`?` 和短路表达式。
只执行选中的分支，但两臂都必须通过静态类型、所有权和资源预算检查。非空分支正常
结束时，先移交并等待它仍拥有的 Task 的真实清理 ACK，再释放本臂局部 Owned Value，
之后才能进入汇合点。return/fail/`?` 错误退出复用最终退出清理，不执行另一臂或汇合点。
内层可以用显式类型声明 shadow 外层同名局部；初始化表达式先读取原环境，离开分支
后恢复外层名字。已有 int/bool/null、字符串、平面数组、单层 Result 和 Error 局部
支持普通赋值 `name = expression`：RHS 在原环境完整求值后才写回原槽。Copy 值复制，
Owned 值在同一成功端点先释放旧值再 move；自赋值只读、不消费。失败的 RHS 不执行
赋值自身的旧值释放，但 RHS 已显式 move 的值不会因失败而恢复。旧值和新值在求值
期间同时存活的容量仍计入原预算。`rows = []` 沿用已有平面数组目标的元素类型，
不开放无类型空数组声明或一般 `ok([])` 推断。祖先 VALUE 绑定保留原词法清理来源。
常量和函数参数仍不可赋值，由 checker 前置拒绝；Task/Callable 重赋值、复合赋值
与自增/自减仍不支持。
分支可以直接 await 尚可用的祖先 Task，
但不能将 Task move 到另一个词法作用域；只有每条仍会到达汇合点的路径都保有同一
Task 时，才能在汇合后 await 它。臂内局部不能在臂外访问。
ScopeEnter 本身不启动期限；空/inline failed 集合不创建不存在的清理 D。

源码 `while` 每轮从条件表达式入口重新求值，条件必须是 bool；支持当前子集的
Await、`?` 与短路。循环体正常结束先完成本轮 Task drain 与局部 Owned drop，再
通过强制 Suspend 回到条件；空循环体也保留这条协作让出边。return/fail/`?` 错误
退出不进入下一轮，取消走原绝对期限下的清理。循环外 Copy 初值不在回边重新初始化，
body 内 typed shadow 不替代外层值。Owned/Task 的逐轮重复消费仍受 checker 与
原始 IR MUST/MAY 双重检查；现有 Owned 值可重赋值，Task 重赋值仍拒绝。有限用例的实际运行、内部
Scope 两轮取消与完整发布门禁分别记录，不推断任意循环终止、公平时限或性能。

整数 `for item in limit` 在进入循环前只求值并保存一次上限，依次绑定 `0` 到
`limit - 1`；修改 `item` 或原上限变量不改变迭代次数，退出后恢复外层同名变量。
上限与循环体可使用当前子集的 Await；负上限经原 runtime-failure 清理路径报告
`for int iterator expects a non-negative int`。每轮仍先 drain 子 Task、释放局部
Owned Value，再 Suspend 回到独立计数器；提前退出沿用最终清理。

循环内的 `continue` 先清理本轮作用域，再进入 for 的计数器更新或 while 的条件重求值；
`break` 只退出最近一层循环，保留循环外的局部和 Task。嵌套分支的逐层 Task drain
与最后局部 Value drop 共用一次退出的原始绝对期限，不逐层续期；期限耗尽转原
`task/shutdown_timeout`，不再执行退出目标后的用户语句。取消和外层更短期限仍优先。

```ku
async fn Child(value: int): int! { return ok(value) }
async fn main(): null! {
    first = Child(3)
    second = Child(4)
    moved = first
    a = (await moved)?
    b = (await second)?
    println(a)
    println(b)
    return ok(null)
}
```

可恢复错误的 `try/catch` 可接收 `fail`、`?` 和 `await` 后的普通 Result 错误，
先清理 try 内的局部和子 Task，再进入最近的 catch。Error 的 domain/code/message
支持重复读取，也可 `fail problem` 保留原错误继续向外传播。取消、handler 超时和
算术运行错误继续走外层退出，不会误进业务 catch。受限 `try/catch/finally` 已接入：
普通完成、错误、return、break、continue 都经过对应 body，返回值先保存；finally
中的新普通跳转可覆盖原跳转，旧 Owned 返回值随替换释放。取消和超时保留原原因，
所有子任务移交后才执行 protected body，同一退出链的绝对截止只收紧、不续期。
若取消首次发生在已经正常执行的 finally 中，停止当前 body 剩余语句并进入外层清理，
不会从头重放当前 body；进入前已取消时则按 protected 规则执行。

```ku
async fn Load(): int! { fail "读取失败" }
async fn main(): null! {
    try {
        value = (await Load())?
        println(value)
    } catch (problem) {
        println(problem.domain)
        println(problem.message)
    } finally {
        println("处理结束")
    }
    return ok(null)
}
```

顶层无捕获 async 函数可作为局部函数值，支持完整注解、默认 move、显式 clone 和
重复调用。调用仍创建真实 Task；先保存 callee，再按从左到右的顺序求值参数，参数
中的 Await 不会改变选择或重复执行已求值部分。只检查实际可能调用的目标集合，
函数值别名也不能绕过无递归限制。捕获、函数值返回、高阶 helper 的函数值绑定及原绑定重赋值仍未开放。无捕获函数值参数的有限入口见下文。

```ku
async fn Add(value: int): int! { return ok(value + 1) }
async fn main(): null! {
    selected: async fn(int): int! = Add
    println((await selected(4))?)
    copied = selected.clone()
    moved = copied
    println((await moved(8))?)
    return ok(null)
}
```

仍拒绝嵌套数组、递归、Task/Callable 重赋值、复合赋值与自增/自减、跨词法作用域 Task move、捕获闭包、返回函数值、高阶 helper 的函数值绑定、
同步用户函数调用、借用 async 参数、Task 参数/返回/容器/clone、未绑定的 Task 临时、
float/混合类型算术、str/null/Result/Task 比较、下述平面数组以外的动态堆表达式，以及下述 sleep/文件读写、exists、read_dir 和局部 TCP/bytes 以外的异步标准库 I/O。
finally 体只接有限 primitive/字符串/Error 操作、Copy 与现有 Owned 值赋值、输出、分支与局部
try/catch；其中创建 Task、Await、sleep、循环及再次嵌入 finally 仍在生成 artifact 前
明确拒绝。普通 try body 中的多层 finally 链支持由内向外退出。
未支持形式在生成 artifact 前明确报错。
用户 cleanup 仍不能 Await；内部 ACK continuation 不是新的用户语法。
该子集使用有界 worker 组及共享容量的全局/本地固定队列，不递归 poll child；不同 Task
可以由不同 OS worker 同时执行，同一 Task 的 RUNNING/executor 仍互斥。本地 owner
按 FIFO 取任务，空闲 worker 从其它本地队列尾部窃取；外部恢复/取消优先进入全局，
连续最多8个全局选择后给一个本地机会，防止持续外部流量饿死本地任务。全部队列仍
共用原 mutex 和同一 capacity/字节接纳，没有每 worker 扩一份容量或新增 Task owner。
内部 Driver ABI10，旧生成 C 须重编译；这不代表完整 M:N 或已测得吞吐提升。源码 root 在执行目标上按
`min(32, max(4, OS 报告的逻辑处理器数))` 选择 worker 数，不读取编译宿主的 CPU 配置。
这不是对进程 affinity/cgroup 配额的完整识别，也不是最优性能配置承诺。
工作条件与状态观察条件分离，停车/退出状态通知不再唤醒其他空闲 worker。
root 使用最多 1024 个固定驻留槽；字节接纳按固定存储和生成 instance 大小计费，
不是操作系统 RSS 限制。编译器的函数/槽/操作硬限也不限制程序累计执行时间：
无递归调用图仍可产生大量顺序工作。普通计算等待不擅自增加全局超时。
表达式没有提高原有 64 函数、每函数 64 槽/256 状态、全程序 4096 操作及
1,000,000 字面量字节/分析工作量上限。Parser/Checker 的语句体嵌套限制为32层；Task
lower 对已构造 AST 的 If 另限32层，表达式仍受 `depth > 64` 拒绝与共同资源预算约束。
这些是分别拒绝的边界，不是复杂源码必定能达到32层或源码可写64层的承诺。
整数毫秒的 `std.time.sleep(ms)` 已接入该 Task 路径，沿用 `null!` 返回；
无需增加 `await`，`time.sleep(ms)?` 即可。仅识别实际标准库导入，参数先求值和快照一次；
负数返回原 `time/invalid_duration`，零毫秒可以立即完成。正数使用 driver 的固定 timer
记录与原条件等待，同一 worker 可以继续执行其它就绪 Task，不创建新的线程。
每个实例同时最多一项 timer 登记，计入固定存储和 instance 大小；取消、退出及槽复用
都会撤销登记，旧 token 不能影响后来的等待。清理体仍不能提交新的 sleep。
这不包含 duration 对象、异步网络 I/O 或其他尚未接通的阻塞操作。文件读写的固定池见下文。

```ku
import time from "std.time"

async fn Later(): int! { time.sleep(10)? return ok(42) }
async fn main(): null! {
    pending = Later()
    time.sleep(0)?
    value = (await pending)?
    println(value)
    return ok(null)
}
```

### Native 平面数组与 for

局部数组支持 `[int]`、`[bool]`、`[str]` 的字面量、move、显式 clone、len/is_empty、try_get(index) 与
真实运行期 for。直接 async 调用也接受这三类 owned 参数，默认 move，保留来源需显式 clone；
多跳转交沿原 Start 移交费用。元素从左到右只求值一次，可包含已有支持的 Await；
空局部数组需显式注解，直接调用的空数组实参可用已声明的形参元素类型。
try_get(index) 返回对应的 T!，不消费原数组；负数或越界返回 array/index_out_of_bounds，
可用 ? 或 try/catch 处理。成功的 str 为独立结果：STATIC 叶子复用，Owned 叶子按实际
长度复制，真实空 Owned 字符串仍分配1字节。接收者和下标各求值一次；可确认只读的
普通下标直接读取，可能移动接收者的下标先保留快照。快照和结果同时存活时均计入原预算。
native 错误消息沿用同步实现，不承诺与解释器的错误消息逐字相同，domain/code 保持一致。
for 创建独立 snapshot，来源仍可再次遍历。int/bool 按值读取，str 从 snapshot 移入
本轮 Owned 变量，因此循环体可 Await 或把文本传给另一个 async 函数。

```ku
async fn Show(text: str): null! { println(text) return ok(null) }
async fn ShowWords(words: [str]): null! {
    for word in words { (await Show(word))? }
    return ok(null)
}
async fn main(): null! {
    words = ["甲", "乙"];
    first = words.try_get(0)?
    println(first)
    try { words.try_get(-1)? } catch (err) { println(err.code) }
    (await ShowWords(words.clone()))?;
    for word in words { println(word) }
    empty: [int] = []
    println(empty.len())
    return ok(null)
}
```

vector 按实际 capacity 而非 len 计费；Owned 字符串叶子另外按实际容量计费。
快照部分构造即登记到持久 frame，复制失败只释放已完成叶子和 vector，不能提前退款。
continue 保留快照进入原 latch；break/正常结束释放快照；return/fail/取消沿原清理顺序
与绝对 D 处理当前元素、快照及来源。循环回边仍有 Suspend，原64槽/16KB frame、分析及
总字节预算不提高。数组字面量最多64个元素；运行期目录数组最多10000项，不等于增加 frame 槽位。嵌套、push、
索引写入与捕获尚未开放，finally 体中的循环也仍拒绝。参数准入先检查 header、vector
及活跃字符串叶子的范围与已知别名，再一次累计实际容量；失败不留下部分所有权移交。

### Native 平面数组结果

`[int]!/[bool]!/[str]!` 可通过 ok/return、Await 与 `?` 移交；未拆开的数组 Result
也能跨等待保存。无捕获函数值沿相同路径返回数组。空结果先声明有类型的局部数组，
不把 `ok([])` 当成已开放的普遍类型推断。返回前后的 vector 与 Owned 字符串容量仍
归唯一 owner，取回分阶段排除与 request/结果输出/parent 的已知别名并核对费用，成功
转交后才 move；失败保留原 payload 和费用。

```ku
async fn Words(): [str]! { values = ["甲", "乙"] return ok(values) }
async fn main(): null! {
    fetch: async fn(): [str]! = Words
    result: [str]! = await fetch()
    words = result?
    for word in words { println(word) }
    return ok(null)
}
```

内部 outcome kinds1–4保持，5/6/7分别是三类数组结果；旧 union 的大小、对齐和偏移
由 C 编译期断言核对。Frame5/Control2/Driver10及原预算不变，数组字面量仍最多64个元素；
运行期数组可承载最多10000项的 read_dir 结果，返回、传参、clone 与遍历按实际容量和字符串叶子计费。

### Native 函数值参数

```ku
async fn Add(value: int): int! { return ok(value + 1) }
async fn Apply(action: async fn(int): int!, value: int): int! {
    return await action(value)
}
async fn main(): null! {
    action = Add
    println((await Apply(action.clone(), 4))?)
    println((await action(8))?)
    return ok(null)
}
```

直接 async 和顶层无捕获 Callable 都接收平面数组及其单层 Result 参数。
Result 成功分支先验证完整 vector 并计入实际容量，错误分支只处理三个 Error 字符串；
失活分支不读取。全部参数的基础范围登记后才读取字符串叶子。裸 `[]` 在直接或
间接数组实参位置使用签名元素类型；`ok([])` 仍需先建立有类型的空数组局部。

函数值参数默认 move；要继续使用原绑定需显式 clone。接收者可 clone、多次只读调用或
再传给另一直接 async 调用；参数本身的签名限 primitive、平面数组及各自单层 Result 形参，允许 primitive
或平面数组 Result 返回。把这种高阶 helper 本身再绑定成函数值、返回函数值或捕获 closure 仍明确拒绝。
参数之间有 Await 时，前面已选中的函数身份只求值一次，使用原活跃值保存。

编译器只传播各真实调用点已证明的 ID，不把全部同签名函数当作候选。生成入口在
任何其他 Owned 实参移动或实例预留前核对全部 callback ID，ID0仍可为合法函数。
原始 C factory 只接受验证计划中已证明到达该形参的 ID；没有已证明来源的入口没有
可接纳 ID，不能据此推导开放的外部函数指针 API。原64函数/64槽和分析预算不变。

### Native 文件与目录操作的固定后台池

`import fs from "std.fs"` 后，async 函数可直接使用 `fs.read(path)?` / `fs.try_read(path)?`
取得 `str!`，或以 `fs.write(path, content)?` / `fs.try_write(path, content)?` 取得 `null!`。
调用本身可挂起，无需增加 await。View 输入不被消耗，参数按顺序求值一次，等待期间
只持有原值和独立的后台副本。固定 worker 数沿用目标系统的
`min(32, max(4, CPU 核心数))` 策略，job 表与队列上限1024，不新增公开配置。
没有上述文件或目录调用的程序不建立该池。网络和未列出的其他阻塞操作仍不属于这一入口。

原生路径转换（包括 Windows 临时 UTF-16 缓冲）与内容复制在每次实际分配前计费；
pool 固定存储按真实 sizeof 纳入 driver fixed_bytes。副本尚未退休时，finally 中的
投影、原值释放或父 Task 终态都不能提前退回其额度；实际 job 完成并被消费后才退回。
这些计数不覆盖 libc FILE 内部缓冲、OS 线程栈或整个进程 RSS。

worker 开始前观察取消/关闭，已取消的排队工作跳过文件操作。已经开始的 OS 写入
不能强行中断，取消也不能撤回已写数据。用户 finally 与原值释放可以先运行，但
后台 job 真正退出前不会给外层清理 ACK，也不释放实例或它仍依赖的驱动存储。
超出原绝对 D 时报告尚未完成的清理并保留存储，晚完成与重试不续期。
POSIX 在线程最后一次存储访问之后才 pthread_join；OS 返回尾部没有硬时长保证，
沿用原 driver 的边界。finally 内提交这些文件操作仍明确拒绝。

```ku
import fs from "std.fs"

async fn main(): null! {
    content = "中文🙂"
    fs.write("result.txt", content)?
    decoded = fs.read("result.txt")?
    println(fs.exists("result.txt"))
    println(content)
    println(decoded)
    return ok(null)
}
```

读取保留 UTF-8 校验与 1000000 字节文件上限，内容中的 NUL 不被截断。输出从小容量
渐进增长，按实际容量及扩容时新旧缓冲同时存活的峰值预付；原 Task 总字节预算不足时
仍可能先返回 memory_limit，文件上限不是无条件接纳保证。worker 只持有 job 输出，
不会写 frame 或 token；结果在实际读取、校验和关闭文件结束后移交给 Ku。成功结果可
传参、重复读取及再次写入，取消时的迟到结果按原字符串释放规则清理。

`fs.exists(path): bool` 同样使用此池并保留 View 输入；文件、目录或有效链接为 true，
缺失、无效路径及 metadata 失败为 false。队列满、关闭、配额或分配失败保持 Task 运行错误，
不能冒充不存在。它仅持有原生路径副本，不分配输出堆对象；真实查询和句柄关闭完成后
才退休 job、释放费用并允许 ACK，取消与晚完成不续原 D。

`fs.read_dir(path): [str]!` 也经同一后台池挂起恢复，保留 View 输入；结果按路径排序，沿用最多 10000 项及路径文本合计 1000000 字节的目录上限。目录 vector、字符串叶子、平台路径临时区和分配记录在分配前计入原 Task 字节预算；扩容期间新旧容量同时计费，原预算可能先拒绝，目录上限不是保证接纳。worker 关闭目录句柄后移交独立持有的结果，取消或迟到完成清理实际分配后才退款，不强行打断已经开始的 OS 调用。

### native Task 的局部 TCP 与 bytes

`net.client({ host: ..., port: ..., ... })`、`client.read(n)`、`client.write(data)` 和 `client.close()` 使用同一个固定后台池挂起恢复，调用处不额外加 `await`。配置必须为内联对象，沿用 host、port、connect_timeout_ms、read_timeout_ms、write_timeout_ms、max_read_bytes、tls、tls_server_name、tls_ca_pem 九个既有键；值按源码顺序求值。连接和 bytes 只能由当前帧局部持有，包括它们的局部 Result；不能作为 Task 参数、返回值、容器元素或函数值载荷。原 Task completion/参数 ABI 和 Driver10 不变。

局部 bytes 支持 `bytes.from_str`、`bytes.from_array`、`clone`、`len`、`get` 和 `to_str`，保留二进制 NUL、UTF-8 检查和既有错误。读取可只返回当前可用的部分数据；EOF 与再次使用已关闭连接仍沿用原错误。写入读取 bytes，不消耗原值。

提交成功后，job 独占连接，帧中的物理指针清空；read/write 完成后连接与结果一并交回。配置和写入内容使用独立副本。取消先清理帧时不能释放仍被操作使用的连接：等待实际 DONE 后丢弃 job 持有的数据并关闭连接，再退回额度。队列等待计入提交时建立的同一绝对期限；已经开始的 DNS 查询不能强制中断。Task 预算覆盖连接头、输入副本与读取结果；TLS ciphertext pending、TLS/解析器库内部、系统 socket 与线程资源仍不在此账本。

此处只是已接入的 TCP 子集，通用网络 readiness provider、解释器完全可挂起调度和完整 M:N 仍未完成。

print/println 目前仍调用同步 stdio；阻塞输出不是已接入 netpoll 或 blocking pool 的 I/O。
M:N、通用 Task 网络等待/netpoll、完整阻塞 I/O 覆盖、完整 RSS 预算及性能/soak 尚未完成；HTTP 的单 I/O 线程接入见下文。
真实双 worker 重叠执行的定向测试不等于吞吐、扩展性或长期低 CPU 占用验收。

## 同步只读借用与 async

`&name: T` 的借用期只覆盖当前同步调用。第一版 `async fn` 不能直接声明 borrowed 参数，checker 返回 E0913；不会根据参数是否出现在第一个 `await` 之前放宽规则。借用值也不能进入闭包捕获或 task frame。

async 函数可以拥有普通参数，并在函数内部调用同步借用函数。例如解释器可执行：

```ku
fn Count(&text: str): int { return text.len() }

async fn CountLater(text: str): int! {
    return ok(Count(text))
}
```

同步调用结束后借用即结束；后续 `await` 不会携带这份借用。async 函数也可以拥有 `fn(&str): int` 类型的同步 callback 值，这与 async 函数自身声明 `&` 参数不同。callback 的捕获与同次调用的重叠仍接受普通借用冲突检查。

`&` 保证不消费句柄及不通过 borrowed 根直接写透明值，不表示函数没有 I/O 或 opaque client 内部状态变化。解释器对 borrowed 读取还检查调用所在线程和 task，跨线程 / task 使用会被拒绝。它不增加用户线程、spawn、detach 或手动调度 API，Task 仍为 move-only，`await` 仍消费一次。上述同步调用/函数值示例不属于首片 native async 子集，native 构建仍会拒绝。

## runtime 有界策略

当前解释器 runtime 的默认边界：

```txt
active task 上限: 1024
task queue 上限: 1024
blocking queue 上限: 1024
await 深度上限: 64
```

超过上限时，Ku 不会无限排队，也不会无限重试；超出的提交会结构化拒绝并进入 runtime 指标。
空闲 task/blocking worker 阻塞等待有界队列并由新任务唤醒，不做固定间隔轮询；单 worker 内部仍可在 `await` 时执行一个已排队子任务，避免嵌套等待饥饿。这些都是 runtime 内部行为，不增加用户级并发 API。
但当前 await、blocking completion 等待和 shutdown 路径仍含短间隔检查；取消清理期间不再帮跑用户任务。
固定 worker 和有界队列不等于已经实现可挂起的 stackless M:N 调度或通用 Task 网络等待。

v0.0.18 开发分支的解释器 HTTP 已将 socket 读取、等待 handler、分段写出和关闭交给每个 listener 的单个 I/O 线程。Linux/macOS 使用批量 poll，Windows 使用 WSAPoll；主线程仍串行运行 Ku handler，不把 Ku 值或闭包送往 I/O 线程。max_connections、max_active_requests 和 max_pending_requests 保留原连接许可、活动连接和 FIFO 等待队列的含义；等待读写不会创建逐连接 OS 线程。空闲 I/O 线程阻塞等待 readiness 或原绝对期限，handler 完成和退出使用内部通知唤醒；主解释器仍保留原有短间隔取消检查。

解释器各读取、handler、写出阶段沿用一次创建的期限；读取请求后转交原有字符串，响应头与 body 分段持有。每个请求有独立一次性返回通道；连接超时或关闭后迟到结果只释放数据，槽位复用以 generation 区分。

native C HTTP 使用 Linux epoll、macOS kqueue 的持久兴趣注册，Windows 保留 WSAPoll。每个 owner 独占其连接的读取、分段响应写出及关闭；完整请求输入独立持有后交给有界 handler worker，等待读写不占用 handler worker。连接记录 C（max_connections）、已接纳请求租约 A（max_active_requests）和共享 FIFO 等待名额 P（max_pending_requests）分别计量：首次观察到请求字节后才取得 A，空闲连接不占 A，也不预分配 header 缓冲；A 从接纳持续到响应完成和实际 Task/job 引用退役。原始等待 socket 和已有连接的后续请求共用一个 P 队列，按完整连接代次与请求序号识别；取消或超时归还各自名额，部分读写与等待接纳不刷新原绝对期限。native handler 的期限仍从实际调用时建立，沿原协作检查与清理路径返回；阻塞系统/FFI 调用不会被 I/O 线程强制打断。若所有 worker 都创建失败，保留原 inline 降级，此时执行 handler 仍会阻塞 I/O 线程。有限 KU_HTTP_MAX_REQUESTS 达到接受数量后停止监听，实际排空已接受的读取、job、响应及引用，再 join worker 并退出。

两端默认复用 HTTP/1.1 连接；HTTP/1.0 仍拒绝。每连接仅执行一个 handler，最多保留一次 socket 读取产生的 8192 字节后续请求，按顺序处理流水线；显式 Connection: close（大小写无关的完整 token）、错误/超时及 1xx 响应结束连接。解释器仍沿用原空闲连接让出活动槽的策略。native 中仅 A/P 压力不关闭空闲 keep-alive；后续请求与已等待请求按同一 FIFO 竞争 A，连接记录 C 真正耗尽时仍可回收空闲连接，客户端须处理服务端关闭。各请求沿用原 header/body/handler/write 绝对期限，复用空闲等待使用 idle_timeout_ms，不增加 handler worker。

`max_buffer_bytes` 默认 67108864（64 MiB），必须为正整数，最大 1073741824（1 GiB）。它限制每个 HTTP listener 的 transport 保留额度，覆盖受控 header/body/carry、请求与响应字符串缓冲及保守的临时预占；额度不足返回 503 并关闭连接。该额度不等于整体 Ku 堆、allocator 元数据、容器桶、线程栈、内核 socket 缓冲、外部库内存或进程 RSS 的上限，也不提供公开 stats 方法。

通用 Task 网络等待、跨模块完整资源预算/指标和正式长期负载验收仍未完成。

## 开发者 HTTP 压测 demo

native 调度器维护者可使用[固定工作量观察工具](native-task-scheduler-probe.md)。
它验证实际 Task 回收并记录独立样本，不是 HTTP 吞吐、全量资源预算或发布验收。

启动 HTTP 服务：

```powershell
ku run examples\http_capacity_10m.ku
```

另开一个终端发起压测：

```powershell
powershell -ExecutionPolicy Bypass -File examples\http_bench.ps1 -Url http://127.0.0.1:8080/health -Requests 10000000 -Concurrency 1000 -TimeoutSeconds 600
```

这个 demo 是开发者视角：业务代码只写 `http.service()`、`app.get/post`、`fn()` / `fn(req)` 和 `return http.text/json(...)`。并发调度由 Ku runtime 处理，普通业务代码不导入 `std.task`，也不手动创建或调度 task。

压测输出应重点看这些字段：

- `Requests`：总请求数。
- `Concurrency`：客户端并发请求数。
- `WallMs` / `RPS`：总耗时和吞吐。
- `Errors`：网络错误或超时数。
- `LatencyP50Ms` / `LatencyP95Ms` / `LatencyP99Ms`：延迟分位。
- `StatusCounts`：HTTP 状态码分布，正常应主要是 `200`。

## 不能混淆的两件事

“千万请求压测”不等于“千万个活跃连接/协程同时常驻”。

10,000,000 个请求可以由一个或多个压测客户端分批并发发出；10,000,000 个真实 HTTP keep-alive 连接同时常驻则会受到操作系统 fd/端口、内核 socket buffer、内存、网卡、负载均衡、客户端压测机数量和超时策略限制。如果要验证千万级同时在线连接，需要单独的多机压测方案、内核参数、连接复用策略和服务端 runtime 配置。

仓库根目录的 `test.ku` / `run-test.ps1` 仍是 runtime 维护者使用的内部诊断入口，用来验证 active task 有界、超限结构化拒绝且不无限排队。它们不作为普通开发者业务示例。

## HTTP 并发边界

HTTP server 当前提供：

- `max_connections`：同时在线连接上限。
- `max_active_requests`：native 已接纳请求租约 A 的上限，覆盖读取、handler、响应写出及实际 Task/job 退役；空闲连接不占用。解释器保留原活动连接规则。
- `max_pending_requests`：有界共享 FIFO 上限 P；native 的原始等待 socket 与已有连接的新请求共用该限额，队列中的请求不占 A。
- `max_buffer_bytes`：HTTP transport 保留额度，默认64 MiB，最大1 GiB，必须为正整数；不表示整体进程内存上限。
- `handler_timeout_ms`：handler 执行超时返回 504。
- `idle_timeout_ms`：连接首字节等待超时。
- header/body/write timeout：网络读写不无限等待。

普通 handler 用 Return 模型：

```ku
app.get("/health", fn() {
    return http.text("ok")
})

app.get("/user/{id}", fn(req) {
    return http.json({ code: 0, msg: "ok", data: { id: req.params.id } })
})
```

`fn(req, res)`、`res.write`、`res.end`、`reply.send` 不属于普通 handler 模型。

### HTTP 共享捕获的静态边界

v0.0.18 正在实现并验证 [HTTP 共享函数绑定规则](syntax.md#http-共享函数绑定规则)。它补足注册时只检查旧函数体、之后却能把共享函数变量换成写外层状态的函数这一缺口；本项尚不能视为完整验证或生产高并发完成。

handler 不能修改外层捕获（E0703）。路由注册后，直接或间接共享捕获的函数绑定按词法 `BindingId` 禁止重绑定（E0704），同签名只读替换也不例外。分支汇合、循环回边、提前创建的 setter 及其 clone/move 别名必须保留这项限制；不能证明有关组合的捕获或调用效果时明确拒绝，不把未知效果视为安全，也不自动解冻。

这不是冻结所有 Copy 或函数变量。正常调用、未共享绑定的赋值、同名新局部及原有参数规则保持不变；直接传函数值注册不会仅因此冻结保存该值的原变量，但不会擦除函数值内部的捕获关系。闭包 clone 共享环境，环境引用计数只负责存活期，不等于对捕获内容加锁或创建快照。

普通 Copy 初始化未被 E0704 一并禁止，不代表允许 handler 写共享状态，也不证明任意外层并发写入无竞态。这里不引入 `std.atomic` / `std.sync` 或新的共享状态接口；完整资源预算、通用 Task 网络等待、性能与 soak 的未完成边界不因新增静态检查而改变。
