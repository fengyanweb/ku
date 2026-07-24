# Ku Diagnostics

诊断输出始终包含三类信息：出问题的位置、问题描述、修改方向。`ku check --json` 使用 JSON Lines，字段为：

```txt
level code message file line column endLine endColumn notes helps
```

成功时 `ku check --json` 静默输出。

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

需要保留原值时显式 `a.clone()`。同类错误还包括从 owned 字段 / 索引元素直接 move、以及在循环体内 move 外层 owned 值。

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
