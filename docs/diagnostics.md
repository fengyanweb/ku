# Ku Diagnostics

诊断输出始终包含三类信息：出问题的位置、问题描述、修改方向。`ku check --json` 使用 JSON Lines，字段为：

```txt
level code message file line column endLine endColumn notes helps
```

成功时 `ku check --json` 静默输出。

## Unused

### E0901 unused import

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

当前普通函数参数 unused 仍未进入全局 error；HTTP handler 单独要求请求参数使用 `req`，不读取请求时必须写 `_req`。变量、常量、普通参数默认 warning 还需要先接 warning 管线，避免破坏 `ku check --json` 成功静默契约。
