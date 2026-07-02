# Ku 待决策问题

这份文档只保留需要你决定的问题。已经决定并进入执行队列的内容，不再重复堆在这里；已固定语法/API 写入 `docs/syntax.md`，package/registry 写入 `docs/package.md`，IR 优化写入 `docs/ir.md`。

## 当前需要你决定

### 1. 函数类型语法采用哪一种

native closure 第一阶段要做“无捕获同步函数值 + 精确间接调用”，需要有可写、可诊断的函数签名类型。需要确定用户层语法。

可选方向：

- 建议方案：`fn(int, int): int` 表示函数类型。优点是和普通 `fn` 声明一致，和箭头函数表达式不冲突。
- 备选：`(int, int) => int` 表示函数类型。优点是贴近箭头函数，但 parser 要额外区分类型位置和表达式位置。

影响范围：parser、checker、IR `FunctionPtr(params, return)`、C/LLVM 间接调用、native closure 第一阶段拒绝捕获时的错误提示。

### 2. registry 官方根公钥与自定义 registry 信任配置格式

当前已实现 Ed25519 detached signature verifier，但还没有 CLI 可用的根公钥配置。需要确定信任根写在哪里。

可选方向：

- 建议方案：内置官方 registry 根公钥，同时允许 `ku.mod` 或用户配置显式写自定义 registry 公钥。优点是官方源易用，自定义源显式可信。
- 备选：完全不内置公钥，所有 registry 都必须在项目或用户配置里写公钥。优点是更透明，但新手体验更重。

需要一并决定：公钥编码使用 hex 还是 base64；key id 格式；key rotation/revocation 是写进 signed index，还是单独 signed roots 文件。

### 3. package 归档格式和受限解包规则

远程 registry 下载后还需要受限解包。用户已倾向 `.tar.zst`，但需要确定文件布局和禁止项。

可选方向：

- 建议方案：`.tar.zst`，归档根目录必须包含 `ku.mod`，源码只能在 package root 内；拒绝绝对路径、`..`、Windows drive prefix、symlink/hardlink、设备文件、超限文件数和超限总大小。
- 备选：先只支持 `.tar.gz`。优点是生态工具更常见，但压缩率和长期目标不如 zstd。

需要确认：最大归档大小、最大解包后总字节数、最大文件数、是否允许 README/LICENSE 这类非源码文件。

### 4. unused import / unused variable 何时默认变成 error

当前 `ku check --deny-unused` 已做本文件局部变量/常量第一阶段，`_` / `_name` 表示有意丢弃；函数参数和 import 暂未默认进入 error。

可选方向：

- 建议方案：先保持显式 `--deny-unused`，等 import-origin、示例/测试豁免和跨文件导出影响闭环后，再升级为默认 error。
- 备选：0.0.16 直接默认 error。优点是严格，但会更容易打断现有示例和用户迁移。

需要确认：HTTP handler 未使用的 `req/res` 是否允许 `_req/_res`；测试/示例是否也必须完全零 unused。

### 5. `object.get_or` API 形状

对象默认严格缺键，宽松读取必须显式写出来。`object.get_or` 是后续补充 API，需要确定调用形式。

可选方向：

- 建议方案：`object.get_or(obj, "key", default)` 和实例方法 `obj.get_or("key", default)` 都支持；缺键返回 default，存在则返回值。
- 备选：只支持函数式 `object.get_or(obj, key, default)`。优点是实现简单，缺点是和 `array.len` / 实例方法体验不完全一致。

需要确认：default 是立即求值，还是缺键时才惰性求值。

### 6. 最终 native binary 的第一批目标平台

`ku build` 当前是解释器打包型二进制；最终 native binary 需要 import graph 打包、runtime ABI lowering、object file/linker 和 cache。需要先定第一批目标平台。

可选方向：

- 建议方案：先支持当前 host 平台 + `x86_64-windows`，再补 `x86_64-linux` 和 `aarch64-darwin`。
- 备选：一开始就做 Go 式多平台 target matrix。优点是目标清晰，代价是 linker/runtime matrix 过早变大。

需要确认：第一阶段是否允许依赖系统 C compiler/linker，还是必须直接生成 object 并自己驱动 linker。
