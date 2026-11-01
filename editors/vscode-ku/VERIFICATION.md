# 本地验证记录（2026-11-01）

本次仅为独立集成候选，未同步日常目录或日常插件，未公开发布。

- 基线：`f85f0added0ff62953d961e47c65474b1a9871a8`，由运行时主会话指定。
- 分支：`codex/vscode-ku-hardening`。
- worktree：`/Users/mac/.codex/worktrees/ku-vscode-hardening/ku`。
- 交付包：本目录 `ku-language-0.0.19.vsix`（生成物，未纳入 Git 提交）；同版本安装需显式覆盖。
- 环境：macOS ARM64、VS Code 1.144.0、Node 24.18.0、npm 11.16.0。
- 固定只读 CLI：Ku 0.0.19，`/Users/mac/.local/opt/ku/0.0.19/f85f0ad/aarch64-apple-darwin/ku`。
- CLI SHA-256：`d68369e6b7430bbb586467fb92beb9d556f25aea948c5488ac3d10698822973e`。

## 已确认的问题与最小修改

| 文件 | 原因、修改与风险 |
| --- | --- |
| `src/language.ts`、`out/language.js` | 原终端命令在 POSIX 改写路径分隔符，假定 Windows shell；改直接进程参数。修正保存失败、资源 URI 丢失、任务身份冲突、状态竞态。无文件夹时真实 Host 的任务进程退出码为 undefined，改为直接终端可执行文件，并验证 CLI 实际写出的标记。 |
| 同上 | 原检查拿脏文件的旧磁盘内容、关闭后仍可执行、共享导入诊断互删；改取消/队列（最多两个检查作业）、保存文件守卫及按根聚合。旧 CLI 回退仅限明确不支持 JSON；保持原超时，不改 CLI 协议。 |
| `src/imports.ts`、`out/imports.js`，补全/跳转区 | 原补全丢目录前缀、混淆引号、别名不跳转，调用表达式被误判为定义；局部修正路径上下文、范围、别名及 IO 异常。包缓存语义保持不支持。 |
| `src/formatter.ts`、`out/formatter.js`，格式化 provider | 原规则拆坏 inline block comment，改写多行字符串，可能交换 +/++、-/-- 的 token；先保护完整字符串/注释，再改代码空白并保持 token 边界。遵循 CRLF 和缩进选项，未闭合字面量保持原文。格式化仍为启发式，非编译器 formatter。 |
| `package.json`、`tsconfig.json`、`.vscodeignore` | 增加明确的 lint/typecheck/build/package 入口，资源设置及受限模式声明；只分发所需产物。保留包内相对文档链接，避免 vsce 将子目录文档误改写成仓库根路径。没有新增依赖或更改 lock 的必要。 |
| README 中英文、CHANGELOG、本记录 | 说明实际功能、安装、命令、边界与验证结果。 |

方案 A 为继续修 shell 引号和逐行格式化规则，修改较少但仍依赖 shell/跨行状态。采用方案 B：VS Code 直接进程参数、受保护文本片段、局部诊断状态修复；影响仅在插件，复用现有 CLI，无完整 LSP、核心或根构建修改。

## 通过

- `npm ci --ignore-scripts --no-audit --no-fund`（288 个开发包，未新增依赖）；`npm run lint`、`npm run typecheck`、`npm test`、`npm run build`、VSIX 打包与 `git diff --check`。
- 原有 completion、formatter、diagnostic Unicode/导入源码/有界读取、executable 模型测试；已有测试源码未修改。
- 固定 CLI 在中文/空格 SDK 与源码路径下：version、正常/失败 JSON check、run、IR、emit native C；一个 hello 的 `build --backend c` 及生成程序执行均通过。错误检查的退出码 1 为预期结果。
- 隔离用户数据/扩展目录安装 VSIX 后，真实 VS Code Host 的 11 项断言通过：激活，Run/IR/C 源码任务退出 0 与参数完整，Problems 诊断，编辑清旧诊断，手动检查先保存，中文/空格/单引号本地 import 补全，成员补全，格式化编辑应用后保护多行字符串/注释/CRLF/两空格，别名跳转。
- 未打开文件夹的独立 Host：真实 CLI Run 写出预期标记，终端可执行路径与参数完整；验证后关闭该终端。
- 临时内存 adapter：共享诊断聚合去重、dirty/closed/保存失败守卫、最多两个作业、最新请求替换、取消/reset、JSON 回退边界、真实子进程超时/取消；资源按钮目标、多根资源配置、显式路径失败不回退、受限模式零 spawn、版本状态竞态。
- 1603 组运算符/标点等 token 对比；泛型、复合赋值、增减、布尔、可选访问/rest/箭头合法程序，格式化前后 CLI check 均通过。Ku 不支持指数数字，相关样例前后均为预期失败，未扩展语法。
- 验证脚本初版的临时路径别名、转义及格式化结果单条编辑假设已纠正；真实 Host 最终验证通过，未将脚本错误计作产品修复。

## 未验证与限制

- Windows/Linux 真实安装和 shell/终端行为；最低 VS Code 1.85.0 实机兼容、远程宿主、受限模式 UI 的完整宿主验收。
- 默认 runner 后端的 `ku build FILE` 未实际重构建，避免影响运行时整改；仅验证命令参数及实际 Native C hello 构建。大项目构建、服务、性能压测均未执行。
- Package GC 的实际删除未执行；命令参数经过内存 adapter 检查。PATH 自动发现/工作区 bundle 回退覆盖模型，真实 Host 使用显式 CLI 路径及 workspace 变量。
- 不支持未保存缓冲区编译、完整语义类型推断、@dep 缓存文件导航；不代表所有语言程序的格式化都已穷尽验证。

## 数据库、清理与集成

无数据库新增/修改/删除，无 migration/SQL；数据库测试不适用。没有生产库、API 服务或部署变更。

所有临时脚本、夹具、hello 生成文件、隔离 VS Code 安装和日志在系统临时目录验证后删除；开发依赖 node_modules 验证后删除。只保留源代码、既有测试、必要编译 JS、VSIX 和本精简记录，不保留新增一次性测试。主目录现有语法文件及其他未提交修改未复制、覆盖或提交。主会话未被暂停或终止，没有本机重型 Ku 构建或压测。

集成时先审阅本分支，仅应用插件独立提交；主目录同路径未提交改动应单独处理，不直接覆盖。日常插件需另行安装并重载后才生效。
