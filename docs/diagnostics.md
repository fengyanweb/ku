# Ku Diagnostics

诊断输出包含出问题的位置、问题描述；已细分的诊断还提供修改方向。`ku check --json` 使用 JSON Lines，字段为：

```txt
level code message file line column endLine endColumn notes helps
```

成功时 `ku check --json` 静默输出。

## 诊断码唯一登记表

`src/error.rs` 的 `diagnostic_registry!` 是编译器诊断 ID、code、说明、notes 和 helps 的唯一登记来源。它同时生成 `DiagnosticId` 与 `DIAGNOSTIC_REGISTRY`；新增分类必须在这里登记，不得为不同的已识别问题复用一个 code。`tests/diagnostic_registry_test.rs` 遍历实际登记表检查 ID/code 唯一性和帮助文本，并通过 Lexer → Parser → Checker 的真实失败程序验证 HTTP 与 match 分类。

错误产生点可以使用 `KuError::with_diagnostic_id(DiagnosticId::...)` 显式指定内部身份；该身份不依赖英文 message，并随 clone 和导入文件的诊断上下文保留。尚未迁移的产生点仍经过 `legacy_diagnostic_id` 的文案分类适配层，`ku check` 的部分内部接口也仍包装已渲染文本。这不是完整的 typed diagnostic transport；登记表唯一性不能代替逐个 producer 的迁移和集成测试。

本轮拆分的分类如下。实验版本允许这些 code 收敛，JSON 字段、坐标约定、错误 message 和成功静默合同不变；依赖原有混用 code 的工具需按新分类更新。

| code | 唯一登记分类 | 修改方向 |
| --- | --- | --- |
| E0501 | match 不穷尽 | 补齐缺少的变体，或增加无 guard 的兜底分支 |
| E0502 | match 分支不可达 | 删除不可达分支，或把兜底分支移到具体模式之后 |
| E0601 | 未知 std 模块 | 检查实际模块名及小写规则 |
| E0604 | 成员未导出 | 确认成员存在；用户 fn/struct/enum 导出名须以 ASCII 大写字母开头 |
| E0605 | std 模块缺少显式 import | 添加对应 import |
| E0701 | 普通 HTTP handler 签名或响应写法不合法 | 使用返回 HttpResponse 的 `fn()` / `fn(req)` |
| E0703 | HTTP handler 修改捕获变量 | 移除可能被并发访问的可变捕获 |

其余已识别分类保留：E0104 `switch`、E0105 `let`、E0301 类型不匹配、E0302 非 bool 条件、E0401 无效 Result `?`、E0602 构造器缺少调用、E0603 unused import、E0702 handler 返回类型、E0802 非法 task 操作、E0803 task clone、E0804 重复 await，以及下文的 E0901/E0904/E0905/E0910–E0919。

通用回退分类明确标记为**未细分**：E0001 runtime、E0101 lexical/syntax、E0600 import、E0606 package、E0700 HTTP。它们不表示所有具体错误均已获得独立 ID；E0001/E0101 也可能没有通用修复 help。可恢复运行时错误的 `domain` / `code`（例如 `array/index_out_of_bounds`）是另一份 Result/Error 合同，不能被编译器的 `E` 编号替代。

## Ownership

### E0901 use of moved value

Ku 默认 move；`str` / `array` / `object` / `struct` / `enum` / 函数值是 owned。owned 值赋值或传参默认 move，move 之后再读取源值报错：

```ku
fn main(): null! {
    a = "hello"
    b = a        // a 被 move
    println(a)   // error[E0901]: use of moved value 'a'
    return ok(null)
}
```

需要保留原值时显式 `a.clone()`；只读取参数的同步函数也可以声明 `&a: T`，调用处仍写普通实参。同类错误还包括从 owned 字段 / 索引元素直接 move、以及在循环体内 move 外层 owned 值。新增借用不会取消普通 owning 参数的 E0901。

### E0910–E0919 只读借用参数

这些诊断对应源码 `&name: T` 的参数模式。`&` 不是普通引用类型或调用处的地址运算符。文本输出继续给出文件、起止位置和源码标记；JSON Lines 保持本文开头的字段集合，新诊断的 `helps` 包含可执行修改方向，成功时保持静默。

| code | 含义 | 修改方向 |
| --- | --- | --- |
| E0910 | `cannot modify through borrowed parameter`：直接修改 borrowed 根、字段或元素 | 确实需要取得所有权并修改时，删除声明处 `&` |
| E0911 | `cannot move out of borrowed value`：把 borrowed owned 值移出、保存或返回 | 对需要独立拥有的值显式 `.clone()` |
| E0912 | `borrowed value escapes current call`：闭包捕获 borrowed 参数 | 在创建闭包前先 clone 到 owned 局部变量 |
| E0913 | `async functions cannot declare borrowed parameters` | 让 async 函数拥有参数，需要保留来源时由调用者显式 clone |
| E0914 | `callable parameter mode mismatch`：函数值的 owned / borrowed 槽位不一致 | 使声明与期望函数类型的 `&` 模式精确匹配 |
| E0915 | `cannot pass borrowed value`：borrowed owned 值传给 owning 参数 | 显式 `.clone()`，或把接收参数声明为只读借用 |
| E0916 | `borrow conflicts with move or mutation in the same call` | 先完成借用调用，再 move / 修改同一根；不要只调整参数顺序 |
| E0917 | `borrowed operation is not supported`：当前没有安全 borrowed 路径的操作 | 先显式 clone 为 owned 值，再执行该操作 |
| E0918 | `'&' is not written at the call site` | 写 `inspect(value)`，由函数签名决定 borrow / move |
| E0919 | 单独 `&` 出现在参数槽位以外 | 声明写 `&name: T`，函数类型参数写 `fn(&T): R`；借用箭头加括号 |

例如返回 Copy 字段合法，返回 owned 字段需要 clone：

```ku
struct User { name: str, age: int }
fn Age(&user: User): int { return user.age }
fn Name(&user: User): str { return user.name } // E0911
fn CopyName(&user: User): str { return user.name.clone() }
```

E0916 按根绑定保守判断，包括借用父对象同时消费其字段、以及可触及同一来源的 callback 捕获。多个只读借用同一根合法。字符串拼接、clone 或已完成的嵌套读取产生独立结果，不会把已结束的短期读取当作仍在借用。

当前 E0917 包括 borrowed array 的 `for`、borrowed match 的 owned payload binding、fallible object lookup，以及尚未迁移的 stdlib borrowed 路径（如 `array.first`、`array.map`、`string.chars`）。这是能力边界，编译器不会隐式 clone。消费式对象解构或直接对 borrowed Result 使用 `?` 也必须先取得 owned 副本。

## Unused

### E0603 unused import

未使用的 named import / namespace import 默认是 error：

```ku
import { http } from "std"

fn main() {
    println("ok")
}
```

修改方向：

```ku
import { http } from "std"

fn main() {
    app = http.service()
    println("ok")
}
```

如果是有意保留，使用 `_` 或 `_name` alias：

```ku
import { Helper as _Helper } from "./helper.ku"
```

当前 glob / side-effect import 暂不做 unused 判断，避免误报副作用导入。

### E0905 unused local binding

`ku check --deny-unused` 会把未读取的本地变量/常量升级为 error：

```ku
fn main() {
    unused = 1
}
```

修改方向：

```ku
fn main() {
    _unused = 1
}
```

当前普通函数参数 unused 仍未进入全局 error。HTTP handler 读取请求时写 `fn(req)`；不读取请求时主写法是 `fn()`。`fn(_req)` 保留给中间件、接口适配器、测试 mock 等签名必须带参数但暂时不用它的场景。变量、常量、普通参数默认 warning 还需要先接 warning 管线，避免破坏 `ku check --json` 成功静默契约。
