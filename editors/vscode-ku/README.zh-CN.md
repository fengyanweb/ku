# Ku VS Code 插件

适配 Ku 0.0.19。提供 `.ku`、`ku.mod`、`ku.lock` 高亮、代码片段、内置补全和悬浮提示、本地/导入符号跳转、Outline、格式化，以及调用现有 Ku CLI 的命令。没有完整 LSP，不推断任意变量类型，不解析包缓存做依赖跳转。

## 安装与解释器

在 VS Code 扩展面板选择 **… → 从 VSIX 安装**，选取本次交付的 `ku-language-0.0.19.vsix` 并重载窗口。同版本重装可执行：

```sh
code --install-extension /绝对路径/ku-language-0.0.19.vsix --force
```

插件与 CLI 分别安装，不包含数据库驱动或 native target pack。本次为本地交付，不代表市场或 GitHub Release 已公开发布。

`ku.executablePath` 支持文件夹级设置、多根工作区、中文和空格路径。填写可执行文件路径，不加 shell 引号或命令参数：

```json
{
  "ku.executablePath": "${userHome}/tools/Ku SDK/ku"
}
```

支持 `${workspaceFolder}`、`${userHome}`、`~/`、相对工作区路径；单独打开的文件以文件所在目录解析相对路径。显式路径错误会提示，避免悄悄换用其他 Ku；探测会同时检查退出码和 `ku <版本>` 输出。

留空时依次查找 VS Code 扩展进程的 PATH、当前文件所属工作区 `release/<rust-target>/ku[.exe]`、`target/release`、`target/debug`。bundle 映射覆盖 Windows x64、Linux x64、macOS ARM64；其他架构请设置路径或使用 PATH。插件不修改 SDK、PATH。远程窗口使用远端扩展宿主的 CLI。受限模式和虚拟工作区不激活执行代码的扩展。

## 命令的实际行为

| 命令 | CLI | 结果 |
| --- | --- | --- |
| Ku Run | `ku run FILE` | 在进程任务终端运行，需要 main。 |
| Check Current File | `ku check --json FILE` | 先保存，再在 Problems 和 Ku Output 显示诊断。 |
| Show IR | `ku ir FILE` | 终端打印 IR。 |
| Build Current File | `ku build FILE` | 使用 CLI 默认后端/profile，需要对应版本工具链。 |
| Emit Native C Source | `ku build --native FILE` | 只生成 C 源码；现有 CLI 的这个兼容形式不链接可执行程序。 |
| Package GC | `ku package gc FILE` | 按所属项目回收无用依赖缓存，会实际删除符合条件的缓存项。 |
| Show Version | `ku version` | 显示实际 CLI 及插件版本。 |

命令先保存脏文件，保存失败即停止。工作区进程任务直接传入可执行文件和参数，保留原生路径；可在 VS Code 任务界面停止。未打开文件夹时，直接把 CLI 作为终端可执行文件并传入同一参数数组，可关闭该终端停止。构建输出位置与工具链错误由 CLI 决定。

默认打开/保存时检查。CLI 读取磁盘文件，未保存编辑会使相关旧诊断失效，不会把旧磁盘内容当作当前编辑结果。`ku.checkOnChange` 只对已保存文档防抖，不新增未保存缓冲区协议。检查最多同时运行两个作业，同根只保留最新待办，过期检查会取消，沿用 15 秒上限。只有明确不支持 `--json` 的旧 CLI 才回退文本检查；启动失败、超时会保留错误说明。

诊断指向实际出错文件，包含导入文件，并把 Unicode 标量列映射为 VS Code UTF-16 位置。多个入口对同一导入文件的诊断按来源聚合，不互相清除。关闭入口、修改 Ku 配置、停用扩展时取消待办。读取诊断源码有文件数、大小和时间边界；无法映射列时会明确说明。

## 补全和格式化边界

补全依赖插件内置 API/名称模型，包括 async 一次性 Task、HTTP、JSON、bytes/net 和时间 API，不代表真实类型推导。本地 import 补全保留目录前缀，支持中文、空格及对应引号中的另一种引号；依赖前缀从 `ku.mod` 读取，不提供完整包缓存导航。

格式化调整缩进、运算符/逗号空格、空行及相邻 else/catch/finally，遵循编辑器缩进设置和原换行符，保护字符串（含多行内容）及注释文字。未闭合字面量/注释保持原文。它不调用 CLI 格式化器。快速修复、Outline 和定义查找仍为启发式功能。

## 本地构建

需要 Node.js 20+ 和 npm，在本目录运行：

```sh
npm ci --ignore-scripts
npm run lint
npm run typecheck
npm test
npm run build
npm run package -- --out /绝对输出目录/ku-language-0.0.19.vsix
```

lint 使用 TypeScript 严格的未使用符号检查，无新增依赖；复用已有模型、格式化和诊断测试。VSIX 只含编译产物、语法、片段、文档和许可证，不包含开发依赖、源码或临时测试。开发宿主可在编译后使用 `code --extensionDevelopmentPath=/绝对路径/editors/vscode-ku` 启动。

本次记录见 [CHANGELOG.md](CHANGELOG.md)、[VERIFICATION.md](VERIFICATION.md)。Apache-2.0；分发包保留原始 `LICENSE.txt` 与 `NOTICE`。
