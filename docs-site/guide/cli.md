# 命令行

```txt
ku <file.ku>
ku run <file.ku>
ku check <file.ku>
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
ku build --native <file.ku>
ku package gc <file.ku>
ku version
ku -h | -help
```

当前没有 `ku fmt` / `ku test` 命令。

`ku build` 默认生成解释器打包型可执行文件，输出到 `.ku/build/<profile>/<name>`；有 `ku.mod` 时可以无参读取 `root + main`，默认 `src/main.ku`。`--emit-c`、`--emit-ir`、`--emit-llvm` 会把调试产物写到 build 目录；完整 native ABI 仍在后续路线中。
