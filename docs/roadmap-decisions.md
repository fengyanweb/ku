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

同一源码按 target 分别生成 PE32+ x86_64、ELF x86_64 和 Mach-O arm64，不设计万能二进制。命令只有在对应系统或具备匹配 target 编译器、sysroot 和动态库的环境中成功并通过产物校验后，才得到相应二进制。唯一推荐命令是：

```txt
ku build --backend c --release --target <target> .
```

决定如下：

- 每个显式非 host target 使用独立 `.ku/build/<target>/<profile>/`；host 使用 `.ku/build/<profile>/`。只有 Windows 最终二进制自动追加 `.exe`，IR/C/LLVM 中间文件始终使用不含 `.exe` 的 binary stem。
- IR/C/LLVM 默认落盘为 `.ku/build/[<target>/]<profile>/{ir,c,llvm}/<binary-stem>.<ext>`；显式 `-o` 是 `.ku/build/[<target>/]<profile>/{ir,c,llvm}/<output-path-sha256>/<binary-stem>.<ext>`。同目录多个入口、不同目录同名输出都不共享中间产物，也不会覆盖用户目录中的 C 文件；没有交叉 compiler/sysroot 时保留 C artifact 并报错。
- cross target 不允许自动降级成 host build；linker 成功后校验完整的目标主头、表边界与可加载段，并区分 Linux ELF、Windows PE32+ 和 macOS Mach-O，不匹配就拒绝安装最终产物。
- 链接先在最终输出的同一父目录内原子创建随机 `.ku-link-*` staging 目录，再把候选产物写入其中；Unix 目录权限固定为 `0700`，Windows 继承输出父目录 ACL，因此输出目录和构建账号是可信边界，不能把这套机制宣称为可抵御同账号写者。构建不会按名称扫描或删除用户输出目录中的既有文件。候选产物从同一个已打开句柄完成格式、动态依赖和内容身份校验，安装前再次确认路径仍指向该文件。已有目标使用同卷原子替换；初始不存在的 Unix 目标用 `hard_link` 的 create-if-absent 语义再移除 staging 名称，Windows 用不带 replace 标志的 `MoveFileExW`，两者都拒绝覆盖校验后才由其他写者创建的目标。RAII 只清理本次持有的 staging 目录。
- Zig/Clang 自动接收 target triple；普通 fallback `cc/gcc` 只用于 host。需要 target-prefixed GCC 等驱动时必须通过 `KU_CC` 显式配置，最终仍经过格式校验。
- package registry 发布的是确定性、无安装脚本和第三方本机二进制的 Ku 源码包；消费者为每个系统独立构建，不在 source package 中混装三平台二进制。

`ku build --native <file.ku>` 不带 `-o` 时继续作为源码旁只生成 `.c` 的单文件兼容入口；带 `-o` 时执行完整 native 链接和产物校验。跨系统发布只推荐 `--backend c --target <target>`，不把兼容入口生成的 C 宣称为目标二进制。

当前 runtime module 边界仍未完成，不能把“三目标路径存在”写成“所有程序均已跨平台”：

- 核心同步 ABI 与 `std.fs/std.json/std.time` 的 Windows/POSIX C 分支已在 Windows 2025、Ubuntu 24.04、macOS 15 完整 workspace CI 跑绿，三个目标的 native build/run 门槛也已通过；这仍不是生产负载 soak。
- native `std.http`、plain `std.net` 和 `std.redis` 的 Windows Winsock、Linux/macOS POSIX socket/poll/pthread 分支已在上述三系统 workspace CI 跑绿；Redis 新 client 的真实服务复验仍未完成。
- socket-free `ku-native-tls` runtime ABI 已固定 rustls/ring、WebPKI/显式 PEM CA 和资源上限，并在三系统 workspace CI 完成 crate 构建/测试；它尚未接入 generated C 的 `std.net`、HTTP 或 Redis，也没有最终消费者链接门槛，不能写成通用 TLS 已完成。
- `std.mysql` Unix host build 使用绝对 `KU_MYSQL_LIB`/`KU_MYSQL_INCLUDE`；Windows 可使用同一显式配置，也可发现常见的完整安装。候选 symlink 必须先解析为 canonical 非空普通文件，family、archive magic 和 loader identity 从 canonical target 与固定句柄判定；编译器只读取该句柄的私有副本。最终产物必须精确动态导入所选 loader identity，并完成 header/runtime ABI 握手；显式 non-host target 仍明确拒绝自动链接，可在目标系统分别构建，或自行链接保留的 C artifact。
- `std.pg` 已有 Windows/POSIX 同步与目标库格式处理；三系统统一通过绝对路径 `KU_PG_LIB` 的专用小目录提供匹配 target 的 shared/import libpq，不再扫描系统安装目录或走隐式 linker 搜索。候选 symlink 必须先解析为 canonical 非空普通文件；Windows import library 由有界解析器提取目标 `libpq.dll`，ELF/Mach-O 分别读取 `DT_SONAME`/`LC_ID_DYLIB`，链接字节从固定句柄复制。最终产物必须包含与本次选中库完全一致的 loader identity；缺失、静态回退、路径替换或同族不同 loader 都拒绝安装。PG/MySQL 的三系统精确动态库链接/启动门槛现已通过；identity 指最终直接依赖记录的 loader name，不证明部署时解析到同一文件 hash/path，也不验证传递依赖。
- native async lowering 仍未完成。

## 6. HTTP 专用响应 wire ABI

已决定：

- 普通 handler 只允许 `fn()` / `fn(req)`，不接收第二个 `res/writer` 参数。
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
