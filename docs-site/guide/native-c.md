# Native C

native build 管线会先展开完整 import graph，再生成 C；链接模式随后产出当前目标的独立二进制。生成的 C 和二进制都不会通过 runner 回读原 `.ku` 源码；没有目标 C 工具链时，C artifact 仍是可审查、可测试的硬门槛。

当前 native ABI 已覆盖正式 `KuString`（长度输出、static/owned storage、clone/move/drop/concat/equal）、带边界检查和 `push`/`len` 的 array、开放寻址 dynamic object 与 tagged `KuValue`、统一 `KuError` 和复杂 `Result<T>`、`?` / `try/catch/finally`、函数值及带引用计数环境的捕获 closure。`std.fs`、`std.json` 和 `std.time` 也有 native 实现及运行测试。

这仍不是“所有 Ku 程序都可 native 编译”的承诺。IR/native 目前会明确拒绝 async/await、递归 value struct/enum 布局、object 解构、optional chaining、break/continue、捕获 `for` 循环绑定以及尚未进入 C runtime 的标准库能力；字符串 `trim`/`lower`/`upper` 也还没有 native Unicode 实现。遇到这些边界必须 fail closed，不允许静默换语义。

跨系统发布按目标分别构建，不存在一个二进制同时运行于三个系统。唯一推荐命令是：

```powershell
ku build --backend c --release --target x86_64-windows .
ku build --backend c --release --target x86_64-linux .
ku build --backend c --release --target aarch64-darwin .
```

三次构建分别要求匹配的 compiler、sysroot 和目标库，并产出 PE32+ x86_64、ELF x86_64、Mach-O arm64。host 上能生成目标 C artifact 不等于已经完成目标链接；链接失败会保留目标隔离的 C，不会降级安装 host binary，链接成功后还必须通过格式和 CPU 架构校验。

IR/C/LLVM 中间产物统一写入 `.ku/build/[<target>/]<profile>/{ir,c,llvm}/<binary-stem>.<ext>`；显式 `-o` 时写入 `.ku/build/[<target>/]<profile>/{ir,c,llvm}/<output-path-sha256>/<binary-stem>.<ext>`。方括号表示 target 层仅在显式非 host target 时出现，哈希来自完整输出路径，只用于并发隔离。Windows 的最终文件 `app.exe` 使用 stem `app`，所以对应中间文件是 `app.ir`、`app.c`、`app.ll`。同目录多入口、不同目录同名输出不会共享中间产物，也不会覆盖用户输出目录里的 C 文件。

`ku build --native <file.ku>` 不带 `-o` 时是源码旁生成 `.c` 的单文件兼容模式，不执行链接；带 `-o` 时进入完整 native 链接和产物校验流程。发布三系统二进制时使用上面的 `--backend c --target` 形式，不把兼容模式生成的 C 当成已经完成链接。
