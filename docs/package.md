# Ku Package Draft

0.0.7 固定最小 package 草案，0.0.10 增加 version、ku.lock 依赖列表和 import cache key。本阶段目标仍是先把本地包边界做清楚，不做远程包下载。

## ku.mod

包根目录放 `ku.mod`：

```txt
name = "demo_pkg"
version = "0.1.0"
root = "src"
cache = ".ku/cache"
```

字段：

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `name` | 是 | 包名，必须以小写 ascii 字母开头，只允许小写字母、数字、`_`、`-` |
| `version` | 否 | 包版本，格式是 `major.minor.patch` 数字 |
| `root` | 否 | import root，默认 `src` |
| `cache` | 否 | 包本地缓存目录，默认 `.ku/cache` |

`ku.mod` 只接受 `key = "value"`，`#` 后面是注释。

## Import Root

有 `ku.mod` 时：

- `import { Value } from "util"` 从 `root` 下找 `util.ku`。
- `import { Value } from "./util.ku"` 仍从当前文件相对路径找。
- import 结果必须留在 package import root 内，不能 `../` 跳到包外。

没有 `ku.mod` 时，保持 0.0.6 的相对导入规则。

## Cache

当前只固定缓存位置，不做远程包解析：

```txt
<package>/.ku/cache
```

未来远程包、版本锁、校验和、全局缓存会在这个边界上继续做。

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
```

0.0.10 的 lockfile 会记录本地 package 元数据，以及实际 import 到的本地 `.ku` 文件。`cache_key` 是基于文件内容的稳定 hash，用于后续 import cache 复用和失效判断。远程依赖、语义版本解析、下载校验和缓存淘汰还没有实现。

## 循环依赖

package import 复用现有 `ModuleLoader`：

- canonical path 去重
- visiting/done 状态检测循环依赖
- 1MB 源码保护
- 私有/导出规则保持不变
- 写入 `ku.lock` 的依赖列表和 cache key

## 暂不支持

- 远程包下载
- 远程版本解析和依赖 lock
- 下载校验和缓存淘汰
- 包发布
- 多 package workspace
