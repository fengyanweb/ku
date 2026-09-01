# Ku 待决策问题

这份文档只保留需要你决定的问题。已经决定并已写入实现/文档的内容不再重复堆在这里。

## 2. registry 自动 roots 是否进入 v2

registry v1 已固定并接入 `ku check/run/build`：项目用 `registry.url` 与 `registry.public_key = "ed25519-..."` 显式 pin 一个 HTTPS registry；签名 index、传递依赖、精确 lock、cache 完整性、pack/publish/resolve 均使用这一条路径。协议见 [registry-api.md](registry-api.md)。

自动 signed-roots、在线吊销和透明 key rotation 明确不冒充 v1 已有能力。若进入 v2，仍需决定 roots 的单调版本、过期策略、key id、公钥 hash 与旧根签新根规则。

## 4. unused warning 管线

已实现：

- unused import 默认 error。
- `ku check --deny-unused` 会把本地未读取变量/常量报 `E0905`。
- `_` / `_name` 表示有意未使用。

仍需要你决定 warning 输出方式：

- 方案 A：`ku check` 默认打印 warning，退出码仍为 0。
- 方案 B：只有 `--json` 输出 warning；普通文本先不打印。
- 方案 C：先不做 warning，只保留 `--deny-unused` error，等 VS Code diagnostics 支持 warning 后再开。

还需要决定：`ku build --release` 是否自动等价于 `--deny-unused`。

## 5. native target 与 runtime module matrix

跨平台产物规则已经决定并实现，不再作为开放问题：

```txt
host
x86_64-linux
x86_64-windows
aarch64-darwin
```

同一源码按 target 分别生成 PE32+ x86_64、ELF x86_64 和 Mach-O arm64 三个二进制，不设计万能二进制。唯一推荐命令是：

```txt
ku build --backend c --release --target <target> .
```

决定如下：

- 每个显式非 host target 使用独立 `.ku/build/<target>/<profile>/`；host 使用 `.ku/build/<profile>/`。只有 Windows 最终二进制自动追加 `.exe`，IR/C/LLVM 中间文件始终使用不含 `.exe` 的 binary stem。
- IR/C/LLVM 默认落盘为 `.ku/build/[<target>/]<profile>/{ir,c,llvm}/<binary-stem>.<ext>`；显式 `-o` 是 `.ku/build/[<target>/]<profile>/{ir,c,llvm}/<output-path-sha256>/<binary-stem>.<ext>`。同目录多个入口、不同目录同名输出都不共享中间产物，也不会覆盖用户目录中的 C 文件；没有交叉 compiler/sysroot 时保留 C artifact 并报错。
- cross target 不允许自动降级成 host build；linker 成功后校验完整的目标主头、表边界与可加载段，并区分 Linux ELF、Windows PE32+ 和 macOS Mach-O，不匹配就拒绝安装最终产物。
- 链接先写同目录的唯一 `.ku-link-*` staging。构建启动时仅扫描至多 256 个目录项、删除至多 16 个超过 24 小时且名称严格匹配的普通文件；目录、符号链接和近期文件不会被删除。
- Zig/Clang 自动接收 target triple；普通 fallback `cc/gcc` 只用于 host。需要 target-prefixed GCC 等驱动时必须通过 `KU_CC` 显式配置，最终仍经过格式校验。
- package registry 发布的是确定性、无安装脚本和第三方本机二进制的 Ku 源码包；消费者为每个系统独立构建，不在 source package 中混装三平台二进制。

`ku build --native <file.ku>` 不带 `-o` 时继续作为源码旁只生成 `.c` 的单文件兼容入口；带 `-o` 时执行完整 native 链接和产物校验。跨系统发布只推荐 `--backend c --target <target>`，不把兼容入口生成的 C 宣称为目标二进制。

当前 runtime module 边界仍未完成，不能把“三目标路径存在”写成“所有程序均已跨平台”：

- 核心同步 ABI 与 `std.fs/std.json/std.time` 已有 Windows/POSIX C 分支；仍需在 Linux/macOS 真机 CI 跑完整 native suite。
- native `std.http` 和 `std.redis` 已有 Windows Winsock、Linux/macOS POSIX socket/poll/pthread 源码分支；Windows 已本地验证，Linux/macOS 仍待对应真机 CI 首次跑绿。
- `std.mysql` 尚无 portable cross-target library contract；显式 target 自动链接会明确拒绝，可自行链接保留的 C artifact。
- `std.pg` 已有 Windows/POSIX 同步与目标库格式处理，但 cross build 必须提供匹配 target 的 shared libpq 或 sysroot。
- native async lowering 仍未完成。

## 6. HTTP 专用响应 wire ABI

已决定：

- 普通 handler 只接收 `req`，不接收 `res/writer`。
- 普通 handler 返回 `HttpResponse` 或 `HttpResponse!`。
- writer 只通过 `http.stream(fn(writer) { ... })` 暴露。
- writer 不提供 `end()`；stream 函数正常返回后 runtime 自动结束。
- `del` 是唯一删除路由 API，对应 HTTP `DELETE`；不新增 `delete` 别名。

仍需要你决定专用响应的底层形状：

- `http.file(path)` 是否允许文本模式先落地，还是必须等二进制 body ABI 一次做对。
- `HttpResponse.body` 是否继续是 `str`，还是升级为 `str | bytes | stream`。
- `http.stream` 返回错误时，响应已开始后的日志格式和是否暴露给用户配置。
- `http.sse` 的事件对象字段是否固定为 `{ event, id, retry, data }`。
- `http.websocket` 第一阶段是否只允许 runtime 管理 upgrade，不给用户暴露底层 socket。
