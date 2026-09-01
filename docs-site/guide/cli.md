# 命令行

```txt
ku <file.ku>
ku create <name>
ku create <name> --template <template>
ku create --list
ku init
ku init --template <template>
ku template list
ku run [--locked|--offline] [file.ku]
ku check [--locked|--offline] [file.ku]
ku check --json
ku check --json <file.ku>
ku ir <file.ku>
ku llvm <file.ku>
ku build [file.ku]
ku build .
ku build -o <path> [file.ku]
ku build --release [file.ku]
ku build --profile <debug|release|small|fast> [file.ku]
ku build --emit-c [file.ku]
ku build --emit-ir [file.ku]
ku build --emit-llvm [file.ku]
ku build --backend c [file.ku]
ku build --native [--locked|--offline] <file.ku>
ku package gc [path]
ku package pack [path]
ku package publish [path]
ku package yank [path]
ku package resolve [path] [--locked|--offline]
ku version
ku -h | -help
```

自托管参考 registry 使用独立的 `ku-registry` 二进制，只有以下管理入口；无参数仍是启动服务：

```txt
ku-registry
ku-registry token issue <exact-package-name>
ku-registry token revoke <exact-package-name>
ku-registry --help
```

`issue` 只向 stdout 输出一次新 token，凭据文件只保存 hash。`revoke` 的 token 只能来自 `KU_REGISTRY_TOKEN`，不允许放进参数。两种修改都读取 `KU_REGISTRY_CREDENTIALS_FILE`；服务启动时静态加载 ACL，所以修改后必须重启。该工具是离线 ACL 运维，不是公共注册/登录系统。

当前没有 `ku fmt` / `ku test` 命令。

`ku create` 创建新项目目录，`ku init` 初始化当前目录，`ku run` 只负责运行当前 package 或指定 `.ku` 文件。项目目录名允许大小写字母、数字、`_`、`-`；`ku.mod` 的 package `name` 仍保持小写。内置模板：`basic`、`cli`、`http`、`json`、`fs`、`lib`。

`ku check`、`ku run`、`ku build` 共用依赖解析器。每条命令可选择一个 `--locked` 或 `--offline`：前者固定 lock 图且不改写 lock，后者还禁止 registry 网络和 `file://` source 回读；重复或同时使用会报错。

`ku build` 默认生成解释器打包型可执行文件，输出到 `.ku/build/<profile>/<name>`；有 `ku.mod` 时可以无参读取 `root + main`，默认 `src/main.ku`。`--emit-ir`、`--emit-c`、`--emit-llvm` 分别写入 `.ku/build/[<target>/]<profile>/{ir,c,llvm}/<binary-stem>.<ext>`；显式 `-o` 时三类目录都会增加 `<output-path-sha256>` 层。Windows 的 `app.exe` 使用 stem `app`。完整输出路径哈希只用于隔离并发构建，不是签名。

`ku build --native <file.ku>` 不带 `-o` 时是源码旁生成 `.c` 的单文件兼容模式，不链接；带 `-o` 时进入完整 native 生成、编译、链接和目标格式校验。跨系统发布使用 `ku build --backend c --release --target <target> .`，为 Windows、Linux、macOS 分别构建，不存在三系统通用单一二进制。同步程序的 KuString、array、dynamic object、Result/Error、closure 以及 fs/json/time native ABI 已接入，async native lowering 仍是明确边界。
