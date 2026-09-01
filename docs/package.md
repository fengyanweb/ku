# Ku Package

当前 Ku package 链路包含本地 `file://` 开发依赖，以及第三方 HTTPS registry 的确定性打包、发布、签名 index、传递依赖求解、精确 lock、内容寻址 cache 和 `@name/path` 导入。registry v1 使用项目显式固定的 Ed25519 公钥；没有无签名或自动信任模式。仓库提供可自行部署的有界参考服务 `ku-registry`，并以真实 TLS listener 覆盖发布、消费、ACL、并发和重启闭环；它不是官方托管服务，也没有生产吞吐量或高并发基准。配置与安全边界见 [Registry API v1](registry-api.md)。

包命令只有 `ku package pack [path]`、`ku package publish [path]`、`ku package yank [path]`、`ku package resolve [path] [--locked|--offline]` 和 `ku package gc [path]` 这些 `ku package ...` 入口；`path` 默认 `.`，可以是 package 内文件或 package 目录。文中的 `.` 表示当前 package，不另设 bare `ku pack/publish/yank/resolve/gc` 别名。

## ku.mod

包根目录放 `ku.mod`：

```txt
name = "demo_pkg"
version = "0.1.0"
root = "src"
main = "main.ku"
out = ".ku/build"
cache = ".ku/cache"
template = "basic"

dep.util = "1.0.0"
dep.util.source = "file://C:/work/util"

registry.url = "https://packages.example/v1/"
registry.public_key = "ed25519-<64 hex digits>"
dep.math = "^1.2.0"
```

字段：

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `name` | 是 | 包名，必须以小写 ascii 字母开头，只允许小写字母、数字、`_`、`-`；所有平台统一拒绝 Windows 设备基名 `CON`/`PRN`/`AUX`/`NUL`/`COM1..9`/`LPT1..9` |
| `version` | 否 | 包版本，格式是 `major.minor.patch` 数字 |
| `root` | 否 | import root，默认 `src` |
| `main` | 否 | build 默认入口，相对 `root`，默认 `main.ku` |
| `out` | 否 | build 输出根目录，相对包根，默认 `.ku/build` |
| `cache` | 否 | 包本地缓存目录，默认 `.ku/cache` |
| `template` | 否 | `ku create/init` 生成项目时使用的模板名 |
| `type` | 否 | package 类型，当前 `lib` 只表示库模板意图 |
| `registry.url` | 有 registry 依赖时是 | HTTPS API 基址；不能含凭据、query、fragment 或空白，必须以 `/` 结尾 |
| `registry.public_key` | 有 registry 依赖时是 | 固定的 Ed25519 验签公钥，格式 `ed25519-` 加 64 位 hex |
| `dep.<name>` | 否 | 依赖版本；resolver 支持精确 `1.2.3` 和 caret `^1.2.3`，`~` 暂不进入求解 |
| `dep.<name>.source` | 否 | 仅本地开发覆盖使用绝对 `file://` 路径，路径分隔符固定为 `/`；省略时从项目固定的 registry 解析，不增加第二套 registry source 语法 |
| `dep.<name>.checksum` | 否 | 可选的本地快照额外 pin，格式为 `ku-fnv64-` 加 16 位十六进制；通常省略并由 resolver 计算后写入 lock；registry checksum 来自签名 index |

`ku.mod` 只接受 `key = "value"`，`#` 后面是注释。同一个 key 只能出现一次；普通字段、`registry.*`、`dep.<name>`、`dep.<name>.source` 和 `dep.<name>.checksum` 的重复声明都会报错，不使用 last-wins。

## Build Entry

`ku build` 无显式文件时会读取当前目录或指定目录向上的 `ku.mod`：

```txt
entry = <package>/<root>/<main>
output = <package>/<out>/<profile>/<name>
```

例如：

```powershell
ku build .
ku build --release -o dist\demo.exe
```

构建前会先使用同一个 package resolver 固定依赖图。native C/backend 会展开完整 import graph，生成物不再依赖运行时回读原 `.ku` 源码。

真实 TLS registry 集成测试还会停服后重新离线发射 C、链接并搬移二进制，再删除该测试的作者源码、消费者源码、lock/cache 和 registry 数据，验证产物仍执行已发布包的实现。C 发射始终是硬门禁；仅明确未找到 C 编译器时跳过链接/执行，工具链损坏或编译失败不能算通过。该组合验收已在本地 Windows 跑通，Linux/macOS 仍需各自 CI 实测。

## Create / Init

`ku create <name> --template <template>` 创建新目录；`ku init --template <template>` 在当前目录写入 `ku.mod` 和 `src/main.ku`。默认模板是 `basic`。项目目录名允许大小写字母、数字、`_`、`-`，但 `ku.mod` 里的 package `name` 仍按包管理规则保持小写；`ku create HelloWorld` 会生成 `name = "helloworld"`。

```powershell
ku create hello
ku create HelloWorld --template http
ku create my-api --template http
ku init --template cli
ku template list
ku create --list
```

内置模板：`basic`、`cli`、`http`、`json`、`fs`、`lib`。`create` 负责新建项目，`init` 负责初始化当前目录，`run` 只负责运行当前 package 或指定 `.ku` 文件。

## Import Root

有 `ku.mod` 时：

- `import { Value } from "util"` 从 `root` 下找 `util.ku`。
- `import { Value } from "./util.ku"` 仍从当前文件相对路径找。
- `import { Value } from "@util/util"` 从依赖 `util` 的缓存根目录下找 `src/util.ku`。
- 每个依赖包里的普通/相对 import 都锚定该依赖自己的 `src`，不会错误回到消费者项目。
- 每个包只能通过 `@name/path` 访问自己声明过的跨包依赖；本地 import 结果必须留在当前 package import root 内，不能 `../` 跳到包外。

这是唯一推荐的 import 形式：包内 root import 使用 `"util"`，相对 import 使用 `"./util.ku"`，跨包 import 使用 `"@<包名>/<路径>"`；没有第二套 package import 别名。

没有 `ku.mod` 时，保持 0.0.6 的相对导入规则。

## Dependency Cache

`ku check` / `ku run` / `ku build` 会先解析 `ku.mod` 中的依赖，并都直接接受一个可选的 `--locked` 或 `--offline`。本地开发覆盖使用绝对文件路径；即使在 Windows 上也只写 `/`，不接受 `\`、相对路径或网络共享路径：

```txt
dep.util = "1.0.0"
dep.util.source = "file://C:/work/util"
```

file dependency 按解析出的实际版本和快照 checksum 复制到 package 本地内容寻址缓存：

```txt
<package>/.ku/cache/packages/<name>/<name>-<actual-version>-fnv64-<16 hex>/
```

资源保护：

```txt
最大文件/目录条目数: 512
最大总字节数: 10MB
最大目录深度: 32
```

运行快照只包含 source 根的 `ku.mod`（bare package 可无）和必需的 `src/` 普通文件；在任意层级递归排除 `.git`、`.hg`、`.svn`、`.ku`、`target`、`node_modules`、`ku.lock`、`.env` 与 `.env.*`。被纳入快照的 symlink 或其他特殊文件会被拒绝，不会跟随到 source 外。checksum 只覆盖这个快照，并按稳定路径顺序以 64 KiB 块流式计算 FNV-64；改动被排除的文件不会改变依赖 checksum，也不会把密钥或 consumer cache 复制进去。

如果 manifest 写了 checksum，Ku 会先与实际快照比较；不匹配就失败，不进入解释或编译。如果没有写 checksum，refresh 检测到快照变化时会安装新的内容寻址 root，旧 root 不被覆盖或删除；locked/offline 只接受 lock 固定且重新校验通过的 `cache_key`。此 FNV hash 用于本地开发可重复性，不是网络下载用的密码学强校验。

带 `ku.mod` 的 file dependency 会校验 name、实际 version 与固定 `src` root，其传递 registry dependencies 进入同一个有界 solver。同名 file override 在整张图中优先，但必须满足所有版本约束；不满足就报冲突，不会改为下载同名 registry package。没有 `ku.mod` 的 bare source 只允许作为 direct dependency 使用 exact version。

## Cache GC

清理当前 manifest 不再引用的 package cache：

```powershell
ku package gc .
```

GC 只保留与当前 manifest 匹配、由当前 lock 固定且重新校验通过的内容寻址 `cache_key`，清理旧 checksum/version root、无关包 root 和超过 24 小时的崩溃 staging。CLI 单次最多删除 64 个 cache root，对应最多扫描 4096 个条目，并有 5 秒的独立扫描预算；预算只在扫描边界检查，超出后本次停止、下一次命令继续。锁等待、文件系统调用和已选条目的删除时间不属于这 5 秒扫描预算，因此这不是“整个 GC 必定 5 秒内返回”的 SLA。GC 与解析/导入通过共享/独占系统文件锁互斥，不会删除仍在使用的 cache。

## Lockfile

有 `ku.mod` 的 package 在默认模式的 `ku check` / `ku run` / `ku build` 解析 import 时会生成或原子更新本地 `ku.lock`。`--locked` 与 `--offline` 都不会改写它：

```txt
package = "demo_pkg"
version = "0.1.0"
root = "src"
main = "main.ku"
out = ".ku/build"
cache = ".ku/cache"

[[dependency]]
path = "src/util.ku"
cache_key = "ku-fnv64-..."

[[dependency]]
path = "@util/helper.ku"
cache_key = "ku-fnv64-..."

[[package_dependency]]
name = "util"
requirement = "1.0.0"
version = "1.0.0"
source = "file://C:/work/util"
checksum = "ku-fnv64-..."
cache_key = "util-1.0.0-fnv64-..."
```

`[[dependency]]` 记录实际 import 到的 `.ku` 文件和内容 hash：项目内模块写项目相对路径，依赖模块固定写 `@包名/相对路径`。file `[[package_dependency]]` 记录 requirement、解析出的实际 version、绝对 source、快照 checksum 与派生 cache key；lock 不写机器上的绝对 cache 路径。registry 依赖的 lock 数据可随项目移动。绝对 `file://` source 是显式的本地开发覆盖例外，换机器时必须更新或移除该覆盖，不能把它误解为 portable registry lock 数据。

registry package 还记录 `requirement`、解析后的精确 `version`、`registry`、无 query 的稳定 artifact `url`、`sha256-*` 与派生 `cache_key`。传递依赖也逐项写入。恢复时根据当前项目 cache 根和 `cache_key` 重新定位，所以 portable registry lock 不依赖旧机器上的绝对 cache 路径。

## 循环依赖

package import 复用现有 `ModuleLoader`：

- canonical path 去重
- visiting/done 状态检测循环依赖
- 单文件最多 1 MB；整张 import graph（含入口）最多 4096 个源码模块、递归深度 32、累计源码 32 MB
- 展开后最多物化 65536 个顶层 item，重复导入的 source-equivalent 克隆预算为 32 MB；超限在复制 AST 前返回结构化 `import/*_limit` 错误
- 私有/导出规则保持不变
- 写入 `ku.lock` 的依赖列表和 cache key

## Registry、传递依赖和 cache

唯一的 registry 依赖写法是省略 `dep.<name>.source`：

```txt
registry.url = "https://packages.example/v1/"
registry.public_key = "ed25519-<64 hex digits>"
dep.math = "^1.2.0"
```

`ku check`、`ku run`、`ku build` 与 `ku package resolve` 共用同一个解析器。普通 check/run/build 优先复用满足约束的 lock；无参数的 `package resolve` 会重新获取签名 index，并把兼容范围内的最新解写回 lock：

- 获取 `packages/<name>/index.toml` 和 detached `.sig`，对 index 原始字节做 Ed25519 验签。
- index 每个版本包含 `version/url/checksum` 及其 `dep.*`；传递依赖也在签名范围内。
- resolver 合并 exact/caret 约束并做确定性、有界回溯；整张依赖图最多 256 个包、20000 个求解步骤，不会无限搜索。
- 下载后核对归档 `ku.mod` 的 name、version、固定 `src` root 和依赖集合。
- 同时校验 artifact SHA-256、archive 文件树摘要和解包文件树摘要；cache 源码被改会 fail-closed。
- 在线解析发现已存在的内容寻址 cache 损坏时，会先取得该 cache key 的跨进程系统锁并再次校验；只有确认 target 是 `cache/packages/<name>/<content-key>` 下的真实目录后，才原子改名为同一 package 根内的 quarantine，再重新下载、校验和不可变发布。并发安装者由同一把锁合并，后到者复用修复结果。`--offline` 只报 `offline_cache_miss`，不会 quarantine、删除或改写损坏内容；cache/packages/name/target 任一层是 symlink 或 Windows reparse point 时也会拒绝自动修复，且不会尝试把身份复核失败后的未知 quarantine 移回。
- 每个 package 根已有 4 个 quarantine 前缀条目时，自动修复不再新增隔离；格式未知、非目录或被替换的条目也保守占用名额而不会被自动删除。另一个按包名共享的跨进程 repair 锁覆盖容量扫描、待隔离树检查和 rename，因此同包不同 version/content key 也不能并发突破上限。直接子项扫描最多取 4096 次迭代；不能在预算内确认遍历结束时报告 `registry_quarantine_scan_limit`，达到隔离上限时报告 `registry_quarantine_limit`，均保留当前 target、停止下载，并提示显式 `ku package gc` 或检查 GC 无法安全移除的条目。此限制只作用于需要修复的 cache，不影响正常缓存复用或 offline 校验。
- 隔离前复用 GC 的普通有界树检查：拒绝 symlink/reparse/special entry、过深/过长路径、超过 4100 个条目、单文件超过 32 MB 或总文件长度超过 160 MB 的 target；检查未完整结束则保留原树并 fail-closed，不继续追加下载。容量、树扫描、repair 锁与 rename 共用原操作 deadline，rename 紧前重新核对目录身份。这是每包自动产生 quarantine 的资源边界，不是整个 cache、已有异常数据或文件系统实际占用的全局磁盘硬上限；同权限恶意本机进程的命名空间 TOCTOU 仍属于下一条所述信任边界。
- `.package-locks`、`.registry-slots` 和 `.registry-downloads` 都按真实单层目录逐级打开并固定 identity/final path；staging 只能使用内部生成的 `<name>-<version>-<nonce>`，创建前后及下载、解包、发布边界会重复校验。若 staging 父目录被本机其他进程替换，清理 guard 会保留无法确认身份的目录而不是沿新路径递归删除。在拥有 cache 写权限的恶意本机进程模型下，跨平台路径 API 的各次校验与 rename/create 仍不是一个内核事务；因此 cache 根权限仍是安全边界，检测到命名空间变化会 fail-closed，但不能宣称消除了所有纳秒级 TOCTOU。
- 从一次 package 联网操作开始，cache usage/install/repair/download 锁等待、求解、registry 获取、校验与安装共享一个 300 秒绝对预算；文件哈希、归档/文件树遍历、HTTP body 分块读取和重试退让都在边界检查剩余预算，quarantine/安装的 rename 紧前再次检查。等待消耗预算后，后续 HTTP 尝试会重新按剩余时间缩短连接和读取超时，退让也不会主动睡过截止。它不是整个 check/run/build 编译执行阶段的 deadline，也不是硬实时保证：已经进入内核的单次文件读取/同步/rename 以及同步 DNS 解析不能由 Rust 层在绝对截止时刻强制取消，可能让实际返回时间越过预算；这些调用返回后会再次检查并 fail-closed。若 rename 在截止前进入内核、截止后才完成，操作会报告 `registry_resolve_timeout`，但可能已留下完整验证的 immutable cache，下一次重试可以复用，不会回滚或删除已提交内容。
- 同一 package cache 根下有 8 个跨进程共享的全局下载槽；不同 cache key 合计最多并行 8 个，相同 cache key 同一时刻只有一个安装者，成功后等待者复用结果；若持锁者最终失败，后续接管者可以在自己的剩余预算内重试，因此不能把失败波次解释为严格一次网络请求。进程退出会自动释放系统锁。
- 单包压缩归档最大 32 MB、解包后最大 128 MB、4096 个文件/目录条目、单文件 16 MB；整张图在启动下一批安装前预留压缩/解包总量预算。
- artifact URL 必须是稳定、无 query 的 HTTPS `.tar.zst` URL；HTTPS 请求禁 redirect、凭据和 fragment。index 与 detached `.sig` 发生签名不匹配时，最多重新获取 3 个完整 pair；持续不一致就 fail-closed。只对明确瞬时 GET 错误有限重试，publish 不自动重试。

显式解析命令：

```powershell
ku package resolve .
ku package resolve . --locked
ku package resolve . --offline
```

不带选项会刷新兼容版本。`--locked` 禁止重新求解或改写 `ku.lock`，但 registry 缺 cache 时可以按 lock 的 exact URL/checksum 安装；file dependency 只有在本地 source 仍与 lock checksum 一致时才能补 cache。`--offline` 禁止所有 registry 网络访问，也不读取绝对 `file://` source，只允许复用与 lock 完整匹配且校验通过的 cache；lock 不完整、约束漂移、cache 缺失或内容被改都会失败。

同一模式也可直接用于实际消费者命令，不需要先执行另一套准备命令：

```powershell
ku check --locked .\src\main.ku
ku run --offline .\src\main.ku
ku build --native --offline .\src\main.ku
```

每条命令最多选择一个模式；重复或同时给出 `--locked` / `--offline` 会直接报错。

## 打包、发布与 yank

```powershell
ku package pack .
# 由 CI secret store 或交互式密码输入设置 KU_REGISTRY_TOKEN；不要把 token 字面量粘进命令
ku package publish .
# 对已发布的问题版本做单向撤回
ku package yank .
```

`pack` 要求 `ku.mod` 有 name、version，发布 root 固定为 `src`。输出是：

```txt
.ku/packages/<name>-<version>-sha256-<digest>.tar.zst
```

同一静止源码重复打包得到相同 bytes、checksum 和路径。归档只包含 allowlist，不包含 `.ku`、`ku.lock`、VCS、`.env` 或任意安装脚本；打包后立即用受限解包器和内容树摘要自验。读取文件时会固定 package 根句柄：Unix 逐路径组件使用 `openat` 且不跟随 symlink，Windows 拒绝最终 reparse point，并用已打开句柄的最终路径拦截中间 junction 逃逸；枚举与读取前后的文件 identity、大小和修改状态必须一致，变化直接失败，不做无界重试。

`publish` 从 `KU_REGISTRY_TOKEN` 读取凭据，以有 Content-Length、checksum 和 Idempotency-Key 的流式 HTTPS PUT 上传。服务端成功后，CLI 会重新获取并验签 index，确认 checksum 与依赖元数据都已提交才返回成功。token 不进入参数、manifest、lock 或错误文本。

`yank` 使用当前 `ku.mod` 的 name/version 和同一个 `KU_REGISTRY_TOKEN`，发送无 body、固定 Idempotency-Key 的 HTTPS PUT。它是唯一的撤回操作且只能单向执行：首次与重复调用都成功，不提供 delete/unyank，重复 publish 相同 artifact 也不会恢复可见性。服务保留不可变 artifact；fresh/refresh 从完整签名 index 中不再选该版本，既有 `--locked` 仍可按固定 URL/checksum 补 cache，`--offline` 仍可使用已校验 cache。最后一个可见版本被 yank 后，只有 name、没有 `[[version]]` 的空版本 index 仍是合法且必须验签的完整 index。

yank 不是强制吊销已锁定代码：v1 没有 signed monotonic revision，能回放旧签名 index 的 registry/CDN 也可能展示 pre-yank 快照。需要阻止已锁定消费者或抵御历史响应回放时，应轮换/停用 registry 服务并升级到未来的撤销协议，不能把 v1 yank 当作密码学吊销。

完整 HTTP 约定、状态码、服务端 CAS/事务要求见 [registry-api.md](registry-api.md)。

## 受限归档规则

- 单根目录且根内必须有普通文件 `ku.mod` 和真实目录 `src`。
- 允许顶层：`ku.mod`、`src`、`README`、`README.md`、`LICENSE`、`LICENSE.md`、`docs`、`examples`、`tests`。
- 拒绝绝对路径、`.`、`..`、反斜杠、Windows drive/ADS/保留设备名、尾随空格/点、超长/过深路径。
- 拒绝重复路径、大小写冲突、symlink、hardlink、设备、socket、fifo 和其他特殊条目。
- cache 安装内容不可覆盖；崩溃 staging 与超过 24 小时、扫描确认是普通有界树的 repair quarantine 由有界 GC 清理，活跃 staging、包含 link/reparse/special entry 或未完整扫描的 quarantine 不会被递归删除。

## v1 明确不提供

- preinstall/postinstall/build script 或第三方本机动态库加载。
- 自动 signed-roots、在线 key 吊销和透明轮换；v1 由项目审查后显式更新公钥 pin。
- 多 package workspace。
