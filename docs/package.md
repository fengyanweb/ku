# Ku Package Draft

0.0.7 固定最小 package 草案，目标是先把本地包边界做清楚，不做远程包下载。

## ku.mod

包根目录放 `ku.mod`：

```txt
name = "demo_pkg"
root = "src"
cache = ".ku/cache"
```

字段：

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `name` | 是 | 包名，必须以小写 ascii 字母开头，只允许小写字母、数字、`_`、`-` |
| `root` | 否 | import root，默认 `src` |
| `cache` | 否 | 包本地缓存目录，默认 `.ku/cache` |

0.0.7 的 `ku.mod` 只接受 `key = "value"`，`#` 后面是注释。

## Import Root

有 `ku.mod` 时：

- `import { Value } from "util"` 从 `root` 下找 `util.ku`。
- `import { Value } from "./util.ku"` 仍从当前文件相对路径找。
- import 结果必须留在 package import root 内，不能 `../` 跳到包外。

没有 `ku.mod` 时，保持 0.0.6 的相对导入规则。

## Cache

0.0.7 只固定缓存位置，不做远程包解析：

```txt
<package>/.ku/cache
```

未来远程包、版本锁、校验和、全局缓存会在这个边界上继续做。

## 循环依赖

package import 复用现有 `ModuleLoader`：

- canonical path 去重
- visiting/done 状态检测循环依赖
- 1MB 源码保护
- 私有/导出规则保持不变

## 暂不支持

- 远程包下载
- 版本解析和 lockfile
- 包发布
- 多 package workspace
