# Ku VS Code 0.0.21 验证与交付（2026-11-22）

## 对齐的实际版本

- 扩展 ID `ku-lang.ku-language`，发布者、已有命令 ID 和四个设置键保持不变。插件版本独立升至 `0.0.21`。
- 日常源码基线 `a8b61eaba732ad2c7c7ea112dfb73b15c7c3a2f8`，目标分支 `codex/v0.0.19-daily-delivery`；独立开发分支 `codex/vscode-current-sdk`。
- 固定验收 CLI：`/Users/mac/.local/opt/ku/0.0.20/source-264e53d44f21/aarch64-apple-darwin/ku`，版本 `0.0.20`。
- CLI SHA-256：`49d744a40bc38c917ee09eddb336262c151491dd6f5bab2037d02bdcba4c1abd`。
- doctor build identity `8f71eb1511c99169c108f62539703dd3bb7e1dd4a71c1d8e3f7d5f3b589e8d08`；source identity `264e53d44f21971d142de437392a817c9b78861abf6ebd94b430897641bd05f5`，SDK source commit `c9a30173f6618390d9fa6b8a2b4116985215ddb5`。不将此能力等同于所有公开 0.0.20 包。
- 原常用编辑器安装版本为 `0.0.20`，实际 README 仍写 0.0.19。其编译代码与旧 VSIX 匹配；旧包 SHA-256 `f5b06d4651153405f74858a603b48e2b59befacd44e4031cee4c937becea3fe1`，保留作回退。
- 旧队列修复 `0db545aff60816310a9825756302dd138635ce22` 已是本次基线祖先，本轮保留并验证，没有重新移植。

## 原因、最小修改与风险

现有插件只有版本号更新；正文、名称猜测式补全及正则定义查找仍是早期实现，依赖 import 只认识前缀。方案 A 是只改文案和静态列表，无法解决作用域/锁定依赖。采用方案 B：保留既有 CLI 和诊断调度，补文档局部语义模型、紧凑签名元数据、项目锁定路径解析和少量命令交互；没有完整 LSP、编译器或包管理器重写。

| 位置 | 修改与原因 |
| --- | --- |
| `src/semanticModel.ts`、`src/metadataModel.ts` | 词法作用域、struct/已知对象返回、Task/Result、嵌套解构 rename/default/rest、闭包别名、可选访问；标准库实际重载与成员。未知类型不猜字段，模块仅在对应导入后提供。 |
| `src/imports.ts`、`src/packageModel.ts` | 最近 ku.mod、当前 lock 指纹/唯一 cache_key、实际公开声明和绑定路径；缺缓存/失配解释，不跨项目挑版本，不自动执行或联网。读取和目录遍历有界。 |
| `src/language.ts`、`src/toolTerminal.ts` | 作用域补全/悬停/定义/签名提示；选择 CLI、显式依赖安装、构建选项；Run 保留原生可输入终端；其他工具 tee 输出并遮蔽凭据，取消仅指向本次子进程树。保存/选择期间变更和信任变化再次检查。 |
| 同上 | 非 main 包文件按 CLI 要求选择显式输出；后端不改。规范路径与符号链接别名按文件身份比较，诊断映射回已打开 URI，防 macOS /var 与 /private/var 误判 main 或红线落错标签。 |
| grammar/snippets/completionModel | 保留原语法修改和 CRLF；新增实际语法/签名、九个已检查片段，移除不存在的模块调用猜测。formatter 未复现问题，保持既有实现。 |
| manifest/lock/README/CHANGELOG | 版本和真实能力说明、受限模式 limited、已有设置保留；Ku 默认关闭普通词建议以突出语义补全，显式用户设置仍优先。无新增 npm 依赖。 |
| 既有 `test-completion-model.js` | 仅同步已新增标准库/成员的预期数据，没有删除已有测试。 |
| `.github/workflows/vscode-extension.yml` | 复用现有 npm 检查命令，增加独立三平台插件矩阵，避免为插件运行整个 Ku 编译/性能流程。定义已校验，本次没有提交远端运行记录。 |

模型按文档版本缓存，外部模块缓存最多 32 项；不会每次打开扫描整个 SDK。取消检查仍保留 clean 原请求和 reveal 意图，最多两个作业，旧进程退出前不释放槽位。工具输出按流有界聚合后脱敏，超长行保守遮蔽；没有新增重试或无界轮询。静态分析是保守启发式，编译器仍为最终依据。

## 已实跑：macOS ARM64

环境：VS Code `1.147.0`、Node `24.18.0`、现有 lockfile 安装的依赖。

1. `npm ci --ignore-scripts --no-audit --no-fund`、lint、typecheck、已有 test、build、VSIX package 均通过。语法文件继承原 CRLF，使用 `git -c core.whitespace=cr-at-eol diff --check` 检查。
2. 最终代码打包后安装到隔离 extensions/user-data。测试驱动为另一独立临时扩展，目标 Ku 扩展从已安装目录正常激活，没有把工作树/F5 当安装包验收；全部文件位于 Ku 仓库之外。
3. 主 Host 15 组流程通过：作用域/重命名/rest，函数内同名跳转，嵌套调用字段补全/hover/definition，未知类型和未导入模块不猜字段，Date/time 重载，typed PG 四参数签名，两实际异步示例字段与格式化/幂等/真实 check，显式本地安装并保存清单，实际公开导出/跳转，两个项目各自 1.2.3/2.0.0 锁定版本，右键 URI 覆盖活动编辑器，原生终端实际收到输入，默认 main 构建及非 main 显式输出和 IR，独立诊断/修复红线消失，检查 A 时编辑 B 仍完成，缺 CLI 静态补全且不回退。
4. 单独无项目 Host：中文空格单文件实际 Run 写出中文 emoji 标记、Check 通过，CRLF 多行字符串/注释原内容和两空格格式化保留。
5. 真实受限 Host `workspace.isTrusted=false`：静态字段补全成功，显式 Run/Check/Install 均零 CLI 执行。
6. 新进程重开项目：仍补全真实锁定导出；临时移走缓存源码后 hover 解释缺失与 `ku i`，恢复文件后清单和锁字节不变，没有自动安装。
7. 使用 VS Code 自带 TextMate + Oniguruma 引擎，对安装包 grammar 标记两个实际示例共 325 行；async/await、rest、可选字段/Result、箭头、HttpRequest/Response 的 scope 符合预期。
8. 内存定向回归覆盖语义 21 组、统一表达式提取 13 组、集成 provider 14 组，以及保存失败、对话框期间保存/信任变化、跨块日志脱敏/64KiB边界、EPIPE、取消/reset/旧结果、Windows 终止 helper 失败回退等。适配器验证不当作 Windows 实机验收。
9. 本地真实 `ku i ./vendor/pysubs2` 和 Python 文件绑定安装均通过；公开导出和 UTF-16 声明位置可读。拒绝过期清单、缺缓存、其他版本替代、跨项目、路径越界和符号链接。
10. 最终 README 五分钟对象/解构例子：check、ku run、默认原生 build 及生成程序均退出 0，实际输出“你好🙂”和 2。

## 实际应用验收

`native_task_structured_http.ku`、`http_pg_async.ku` 从当前日常目录只读复制。两者格式化前后 check 均为 0、二次幂等。

- 结构化 HTTP 仅替换临时副本的回环上游端口；check、ku run、默认 C build/生成程序、Native C build/生成程序均退出 0。三个运行收到 9 条预期请求（GET 200、GET 404、POST 201 各三次），中文、emoji、结构化 header 和错误恢复正确。
- HTTP/PG 使用新建 PostgreSQL 17.11、独立 role/database/schema、随机回环端口；只写本次自有 db.conn/http.addr，未读取任何用户连接配置。check、默认 C build 和 Native C build 通过。两个程序均完成 POST 201、GET 200、重复 request_id 200、冲突 409、非法标题 400；SQL oracle 为 2 条独立记录。两个服务排空后退出 0，私有 PG fast stop 退出 0，临时数据库及应用目录已删除。
- HTTP/PG 的 ku run、Runner 和 typed PG 实库行为没有在本次额外实跑；实库范围为真实异步示例的普通 query。没有 Ku 自身构建、七语言或其他性能工作负载。

## 限制与未实跑

- Linux、Windows：本轮没有实机/远端 CI 运行。专用 CI 定义覆盖三平台模型/构建/打包；真实终端、文件选择器和进程终止仍需对应平台验收。最低 VS Code 1.85、远程扩展宿主也未实跑。
- 日常 SDK 的原生 async 子集仍有限制：初版入门例子在 async main 内解构 rest 后直接读取字段，check/ku run 成功，但默认原生 build 报不支持 field projection；捕获该 rest 的闭包改写也被原生后端拒绝。未修改编译器或切换后端掩盖错误。最终入门例子改用已完整验证的同步对象/命名解构；上述两个真实异步应用的原生构建结果仍为通过。
- 日常编译器 `input()`、`input(prompt)` 都实测 E0001 undefined function，不提供虚构片段；原生终端 stdin 通过临时 CLI 包装器输入后转发真实 CLI 的方式验证，不声称 Ku 已有 input API。
- 在线 registry/Git/PyPI 安装、本次未选择的 Runner/Release/locked 构建组合、实际 GC 删除没有冒充已通过。安装由真实 CLI 管理，必要的 registry/Python/工具链配置仍由 CLI 报告。
- 静态模型不是完整语言服务器；动态 JSON、复杂控制流、任意泛型和所有跨模块类型关系不保证推导。第三方缓存按锁定身份/回执定位，未额外全量重算内容树哈希；只有二进制的包不伪造源定义。
- 原生文件选择/保存对话框的取消与保存/信任竞态由适配器验证；Host 对非 main 构建用明确输出 URI 验证实际 CLI，不声称逐一物理点击所有系统对话框。
- 保留用户显式的普通词建议设置时，VS Code 自身可能仍给出文本建议；这与插件的有类型字段补全区分，不据此编造类型。

## 同步、数据库和清理

仅同步插件路径和新插件专用 CI。日常原插件元数据、原语法修改内容被保留并在此基础升级，原 staged 状态与无关修改保留，旧 VSIX 保留。新扩展安装到常用 VS Code 并核对 ID、版本和目录；已打开用户窗口不强制关闭/重载，需按编辑器提示重载以确保启用新版。

没有项目数据库 migration/SQL/schema 修改；数据库测试只使用已清理的私人专库。SDK/CLI/PATH/语言核心/官网/发布标签均不替换或发布。临时测试、驱动扩展、包装器、隔离安装、构建和数据库夹具在验收后删除；保留已有测试、必要 out 文件和本精简记录。最终 VSIX 的路径、SHA-256 与合入提交见本次交付回复，避免把自身摘要写回包内造成循环变化。
