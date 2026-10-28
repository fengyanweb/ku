# Ku Language for VS Code

[简体中文](README.zh-CN.md) · [English](README.md#english)

本插件为 `.ku` 文件提供语法高亮、代码片段、编译器诊断、运行命令和编辑辅助功能。插件版本为 **0.0.19**，需要另行安装匹配的 Ku CLI。

## 功能

- Ku 0.0.19 语法高亮：`async` / `await`、普通字符串、模板字符串、数字、内置类型和标准库调用；同时标记不受支持的 `let` / `mut` / `const`。
- `ku.mod` 与 `ku.lock` 语法高亮，以及注释、括号、自动补全括号和包围选区的语言配置。
- 代码片段：同步／异步 main 和函数、`await task?`、泛型函数、struct、enum、match、try/catch/finally、`std.fs`、`std.http`、`std.config`、`std.task`、有界 Task 压力示例、HTTP 响应、数组 map、字符串方法及 `array.try_get`。
- 打开和保存文件时调用 `ku check`，在**问题**面板和编辑器红色波浪线中显示诊断。
- 命令面板提供 Run、Check、Show IR、Build、Build Native C、Package GC 和 Show Version。
- 编辑器和资源管理器的 `.ku` 文件右键菜单提供 **Ku Run**；没有 `fn main()` 时显示明确提示。编辑器标题栏也提供 Run、Check、IR 和 Build 按钮。
- 状态栏从 `package.json` 读取插件版本，并与实际选中的 CLI 版本比较，提示解释器与插件版本不一致。
- 悬浮说明覆盖 async/await、Error、Result、`std.fs`、`std.http`、`std.config`、`std.task`、match，以及字符串和数组辅助函数。
- 补全覆盖关键字、基本类型、内置函数、标准库模块／函数、Error 和 HttpResponse 字段、字符串／数组方法、导入路径及包依赖前缀。
- 成员补全按上下文处理：输入 `http.se` 可补全 `service` / `server`，构造器插入 `service()` / `server()`，不会生成旧的属性式 `http.service` / `http.server`。
- 补全还覆盖 `std.bytes`、`std.net`、`std.lexer`、`std.parser`、目录操作，以及 `time.instant` / `time.elapsed` / `time.steady_millis`。`time.now()` 返回 Unix 毫秒，`time.instant()` 返回 Time 对象。
- HTTP handler 补全识别 `req` / `app` / `router` 名称；`req.` 提供请求字段。普通路由模板在不读取请求时使用 `fn()`，需要请求时使用 `fn(req)`。
- 普通 `async fn` 返回一次性 Task 句柄，插件不会为它建议生命周期句柄的 `status` / `cancel` / `await_timeout` 方法。
- 类型化箭头函数是一等值。对象索引默认严格，显式可空查找使用 `object[key]?`；对象解构片段覆盖重命名、默认值和 rest。
- JSON 格式的编译器诊断定位到实际出错的导入文件；过期检查结果不会覆盖更新后的保存或编辑结果。
- 定义跳转覆盖本地函数／类型、导入文件及导入模块的导出符号；大纲覆盖模块、函数、struct、enum 和局部函数。
- 快速修复覆盖缺失的 `std.http` / `std.fs` / `std.config` / `std.task` 导入，以及 `let` 和 `switch`。
- 内置格式化使用四空格缩进、压缩多余空行、整理运算符／逗号间距，并合并 `} else` / `} catch` / `} finally`。

## 从 VSIX 安装

在 [v0.0.19 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) 实际发布后，从匹配平台的安装包中取得 `ku-language-0.0.19.vsix`。是否可下载以 Release 实际附件为准。CI 的 `native_ci-*` 是验证夹具，不是完整 CLI 安装包；部署范围以发布说明和具体业务验收为准。

已有插件可重新从 VSIX 安装，或执行：

```sh
code --install-extension ./ku-language-0.0.19.vsix --force
```

需要自行构建时，从仓库根目录执行：

```powershell
cd editors\vscode-ku
npm ci --ignore-scripts
npm test
npx --no-install vsce package --out ku-language-0.0.19.vsix
code --install-extension .\ku-language-0.0.19.vsix
```

安装后，在命令面板执行 **Developer: Reload Window**，或重启 VS Code，再打开 `.ku` 文件。

## 不使用命令行安装

1. 打开 VS Code 的**扩展**面板。
2. 点击 `...`，选择**从 VSIX 安装…**。
3. 选择从匹配安装包解压或本地构建的 `ku-language-0.0.19.vsix`。
4. 执行 **Developer: Reload Window**，或重启 VS Code。

## 复制扩展目录安装

先在 `editors/vscode-ku` 中执行 `npm ci --ignore-scripts` 和 `npm run compile`。不使用 `vsce` 时，也可把编译后的目录复制到 VS Code 扩展目录，必须包含 `out/`、`LICENSE.txt` 和 `NOTICE`。以下 Windows PowerShell 示例从**仓库根目录**执行：

```powershell
$target = "$env:USERPROFILE\.vscode\extensions\ku-lang.ku-language-0.0.19"
New-Item -ItemType Directory -Force -Path $target | Out-Null
Copy-Item -Path .\editors\vscode-ku\* -Destination $target -Recurse -Force
```

复制后重启 VS Code。

## 开发模式

用 VS Code 打开 `editors/vscode-ku`，按 `F5` 启动 Extension Development Host。

## Ku 程序路径

插件按以下顺序尝试可执行程序：

1. 已配置的 `ku.executablePath`。
2. PATH 中的 `ku`。
3. 工作区 `release/<rust-target>/ku[.exe]` 中匹配宿主的包。
4. 工作区的 `target/release/ku[.exe]` 和 `target/debug/ku[.exe]`。

发布目标映射为 Windows x64 → `x86_64-pc-windows-msvc`，Linux x64 → `x86_64-unknown-linux-gnu`，Apple Silicon macOS → `aarch64-apple-darwin`。其他宿主架构需要使用 `ku.executablePath` 或 PATH 提供可用 CLI。

## 能力边界

补全和悬浮说明来自插件内置 API 模型，不是完整的编译器语言服务器，不能推断所有接收者类型和链式调用。编译器诊断由 `ku check` 提供，运行和构建命令仍需要已安装的 Ku 可执行程序。

插件不会安装 CLI、数据库驱动或 native target pack。`std.bytes` / `std.net` 需要所选 Ku 包支持的 native C 执行路径。补全出现某个方法，不代表所有后端均支持它。

## 许可证

Apache-2.0。原始 `LICENSE.txt` 和 `NOTICE` 随插件分发；第三方组件保留各自的许可声明。
