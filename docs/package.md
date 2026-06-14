# Ku Package Draft

0.0.7 固定最小 package 草案，0.0.11 增加 `file://` dependency、checksum、`ku.lock` package dependency 记录和 cache GC。本阶段目标是把本地 package 边界、文件依赖缓存和可重复校验做清楚，再进入 HTTP/registry。

## ku.mod

包根目录放 `ku.mod`：

```txt
name = "demo_pkg"
version = "0.1.0"
root = "src"
cache = ".ku/cache"

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
| `cache` | 否 | 包本地缓存目录，默认 `.ku/cache` |
| `dep.<name>` | 否 | 依赖版本，支持 `1.2.3`、`^1.2.3`、`~1.2.3` 的语法校验 |
| `dep.<name>.source` | 否 | 当前只支持 `file://` 目录 source |
| `dep.<name>.checksum` | 否 | 依赖目录稳定 hash，格式为 `ku-fnv64-` 加 16 位十六进制 |

`ku.mod` 只接受 `key = "value"`，`#` 后面是注释。

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

## 暂不支持

- HTTP/registry package 下载
- 远程版本解析和依赖求解
- 下载签名、强校验和缓存淘汰策略
- 包发布
- 多 package workspace
