# Ku Package Draft

0.0.7 固定最小 package 草案，0.0.11 增加 `file://` dependency、checksum、`ku.lock` package dependency 记录和 cache GC。0.0.12 补齐 HTTPS registry 请求、SHA-256 执行和内容寻址 cache；0.0.13 增加 `ku build` 入口字段 `main` 和输出字段 `out`；0.0.14 增加 `ku create` / `ku init` 模板入口和 `template` / `type` manifest 字段。当前已提供 Ed25519 detached signature verifier 和受限 `.tar.zst` 解包，但生产 CLI 在根公钥、轮换/吊销和 registry trust 配置完全固定前仍保持 fail-closed。

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
dep.util.checksum = "ku-fnv64-..."
```

字段：

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `name` | 是 | 包名，必须以小写 ascii 字母开头，只允许小写字母、数字、`_`、`-` |
| `version` | 否 | 包版本，格式是 `major.minor.patch` 数字 |
| `root` | 否 | import root，默认 `src` |
| `main` | 否 | build 默认入口，相对 `root`，默认 `main.ku` |
| `out` | 否 | build 输出根目录，相对包根，默认 `.ku/build` |
| `cache` | 否 | 包本地缓存目录，默认 `.ku/cache` |
| `template` | 否 | `ku create/init` 生成项目时使用的模板名 |
| `type` | 否 | package 类型，当前 `lib` 只表示库模板意图 |
| `dep.<name>` | 否 | 依赖版本；resolver 支持精确 `1.2.3` 和 caret `^1.2.3`，`~` 暂不进入求解 |
| `dep.<name>.source` | 否 | 当前只支持 `file://` 目录 source；如果 import `@name/...` 时未配置 source，会按 fail-closed 报错，不能读取旧 cache |
| `dep.<name>.checksum` | 否 | 依赖目录稳定 hash，格式为 `ku-fnv64-` 加 16 位十六进制 |

`ku.mod` 只接受 `key = "value"`，`#` 后面是注释。

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

当前 `ku build` 默认生成解释器打包型二进制；它会嵌入口文件源码，但带 import 的程序仍要求原源码依赖路径可访问。最终 native binary 的完整 import graph 打包仍在 native ABI 队列里。

## Create / Init

`ku create <name> --template <template>` 创建新目录；`ku init --template <template>` 在当前目录写入 `ku.mod` 和 `src/main.ku`。默认模板是 `basic`。

```powershell
ku create hello
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
- 本地 import 结果必须留在 package import root 内，不能 `../` 跳到包外。

没有 `ku.mod` 时，保持 0.0.6 的相对导入规则。

## Dependency Cache

`ku check` / `ku run` 会先解析 `ku.mod` 中的依赖。当前支持本地文件源：

```txt
dep.util = "1.0.0"
dep.util.source = "file://C:/work/util"
dep.util.checksum = "ku-fnv64-..."
```

依赖会复制到 package 本地缓存：

```txt
<package>/.ku/cache/packages/<name>/<version>/
```

资源保护：

```txt
最大文件数: 512
最大总字节数: 10MB
```

如果写了 checksum，Ku 会对 source 目录按稳定文件顺序计算 hash，和 manifest 中的值比较。不匹配会失败，不会进入解释执行。如果没有写 checksum，Ku 仍会比较 source 和 cache 的内容 hash，同版本 file dependency 发生变化时会刷新本地 cache。当前 hash 用于本地开发可重复性，不是网络下载用的密码学强校验。

## Cache GC

清理当前 manifest 不再引用的 package cache：

```powershell
ku package gc examples\package\src\main.ku
```

GC 按 `<name>/<version>` 精确保留当前依赖版本，清理同名旧版本和无关包版本。单次最多删除 64 个 cache 版本，避免一次命令误删过多目录。

## Lockfile

有 `ku.mod` 的 package 在 `ku check` / `ku run` 解析 import 时会生成本地 `ku.lock`：

```txt
package = "demo_pkg"
version = "0.1.0"
root = "src"
cache = ".ku/cache"

[[dependency]]
path = "src/util.ku"
cache_key = "ku-fnv64-..."

[[package_dependency]]
name = "util"
version = "1.0.0"
cache = "C:/work/app/.ku/cache/packages/util/1.0.0"
source = "file://C:/work/util"
checksum = "ku-fnv64-..."
```

`[[dependency]]` 记录本地实际 import 到的 `.ku` 文件和内容 hash。`[[package_dependency]]` 记录 manifest 声明的 package 依赖、source、checksum 和缓存路径。

## 循环依赖

package import 复用现有 `ModuleLoader`：

- canonical path 去重
- visiting/done 状态检测循环依赖
- 1MB 源码保护
- 私有/导出规则保持不变
- 写入 `ku.lock` 的依赖列表和 cache key

## Registry Resolver 与下载计划

离线 registry schema 和 resolver 已实现：

- 精确版本 `1.2.3`。
- caret 范围 `^1.2.3`，按 semver 的首个非零位限制兼容上界。
- 同名依赖合并约束并选择满足全部约束的最高版本。
- 没有共同版本时返回 `package/dependency_conflict`，不做无限回溯。
- lockfile 始终记录精确版本和 `sha256-*` checksum。

registry 网络执行层已经实现：

- 下载尝试次数必须在 1 到 8 之间。
- 连接和读取超时必须显式有界，最大 300 秒。
- 单个 `.tar.zst` 压缩归档最大 32 MB。
- URL 必须是 HTTPS，拒绝 HTTP、凭据、fragment 和自动 redirect。
- 静态 index 支持相对/绝对 HTTPS URL、版本排序和重复版本冲突检查。
- 只接受 `.tar.zst` 归档 URL，不接受 `.tar.gz`。
- 已存在且 checksum 匹配、且解包 package root 含 `ku.mod` 的 cache 直接复用。
- 缓存缺失时下载到 cache 外的唯一 staging 目录，边读取边计算 SHA-256；校验通过后按受限解包规则解到 package root，再安装到 `name + exact version + SHA-256` 内容寻址目录。
- 已验证的内容寻址目录不可覆盖。同版本不同 checksum 不会互相替换。
- 安装锁按完整 cache key 隔离，等待最多约 1 秒；旧锁恢复有时间上限。
- GC 不进入下载 staging，也不删除持有安装锁的目录。
- 不对 checksum mismatch、manifest/schema 错误或确定性 4xx 重试；只对明确瞬时错误执行有限退避。
- Windows 路径检查拒绝 drive prefix、根路径和 `..`，dependency import canonicalize 后必须仍在依赖根内。

受限 `.tar.zst` 解包规则：

- 归档必须只有一个根目录，根目录下必须有 `ku.mod`。
- 允许顶层内容：`ku.mod`、`src`、`README`、`README.md`、`LICENSE`、`LICENSE.md`、`docs`、`examples`、`tests`。
- 拒绝绝对路径、`..`、`.`、Windows drive prefix、路径超过 240 bytes、深度超过 32。
- 只允许普通文件和目录；拒绝 symlink、hardlink、设备文件、socket、fifo 等特殊条目。
- 解包后总大小最多 128 MB，文件数最多 4096，单文件最多 16 MB。

当前尚未把该执行层接入 `ku check/run` 的远程 import。原因不是下载/解包能力缺失，而是必须先确定 registry index 签名信任根、key rotation/revocation 和 roots 配置格式。未配置 verifier 时返回 `package/registry_trust_unconfigured`，不能传 no-op 信任进入正式 CLI。`Ed25519RegistryIndexVerifier` 已能验证 registry index 的 detached signature；签名覆盖 exact index bytes，篡改 index 会返回 `package/registry_signature_mismatch`。

## 暂不支持

- 内置官方根公钥、自定义 registry 公钥配置、key rotation/revocation
- CLI resolver/download/cache/import 全链路启用
- 包发布者签名
- 包发布
- 多 package workspace
