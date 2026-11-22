# Ku VS Code 插件

插件 **0.0.21**，扩展 ID 仍为 **ku-lang.ku-language**。提供三个开发流程：写并运行 Ku 文件，维护结构化异步 HTTP/PG 服务，使用已安装并锁定的第三方依赖。插件版本独立于语言版本；本次按日常 **Ku 0.0.20**、源码身份 `264e53d44f21` 开头的 SDK 核对，同为 0.0.20 的其他安装包可能有能力差异。

## 安装和选择 CLI

用扩展菜单“从 VSIX 安装”选择 `ku-language-0.0.21.vsix`；需要时重载窗口。也可运行：

```sh
code --install-extension /绝对路径/ku-language-0.0.21.vsix --force
```

命令 **Ku：选择可执行文件** 可选择 `ku` 或 Windows 的 `ku.exe`。状态栏及 Show Version 显示实际工具路径、CLI 版本和插件版本。插件不替换 SDK，不更改 PATH。

沿用 `ku.executablePath`、`checkOnOpen`、`checkOnSave`、`checkOnChange`。可执行路径支持中文、空格、`${workspaceFolder}`、`${userHome}`、`~/` 和相对工作区路径，不加 shell 引号/参数。显式设置错误会提示选择，不会偷偷改用别的 SDK。留空按扩展宿主 PATH、所属工作区 release/target 目录查找。远程窗口需要远端 CLI。缺 CLI 和受限模式均可静态编辑，工具执行需要信任工作区。

## 补全、签名和跳转

- 识别词法作用域、函数参数/局部声明、闭包别名；同名局部变量不跳到另一个函数。
- 从 struct、对象字面量、已知函数返回、Task/await/Result 解包推导真实字段；支持嵌套解构、字段重命名、默认值、rest 和可选字段访问。未知或分支冲突的类型不编造字段。
- 标准库提示来自紧凑的已核对元数据：time 日期/Duration 及重载、array concat/first/last、HTTP service/client/request/response、普通/typed PG，以及 async 函数内已有的 PG query 路径；没有虚构 query_async 接口。
- 高亮和片段覆盖上述结构化异步语法。非 import 字符串和注释内不弹代码补全。
- 格式化保护中文、emoji、字符串、模板、多行内容与注释，遵循换行符和缩进设置。真实 structured HTTP、async HTTP/PG 示例格式化后仍通过 check，第二次不再变化。

这是有边界的本地编辑器模型，不是完整 LSP。按文档版本缓存解析，不在每次打开文件时扫描整个 SDK；动态对象和复杂控制流无法保证推断。CLI 编译器诊断是最终依据。

## 运行、检查和构建

Run 使用可输入的集成进程终端，编辑器按钮针对当前文件，资源管理器右键针对被点的文件。先保存再执行，取消/保存失败即停止。项目内采用最近 ku.mod 的目录；单文件也能工作。

| 入口 | 实际行为 |
| --- | --- |
| Ku Run | `ku run FILE`，终端保留 stdin。 |
| Check Current File | `ku check --json FILE`，诊断进 Problems；命令、cwd 和错误进 Ku 输出。 |
| Build Current File | `ku build FILE`，服从 CLI 默认后端；本次日常 SDK 默认为原生 C。 |
| Ku：构建选项 | 一个选择器提供 CLI 已支持的默认、Release、锁定依赖、Runner；Runner 先检查匹配工具链。 |
| Show IR | `ku ir FILE`。 |
| Emit Native C Source | 保留 `ku build --native FILE`，兼容形式仅生成 C。 |
| Package GC | 用户主动触发项目依赖缓存回收。 |
| Ku：安装依赖 | 用户主动调用 `ku i` 恢复、输入来源或选择本地文件/目录。 |

构建非 ku.mod main 的文件时，会让用户选择 CLI 要求的显式输出路径，仍不改后端。

构建、IR 和安装输出同步到任务终端及 Ku 输出；后者遮蔽常见凭据字段。停止任务或关闭终端只终止本次进程树。插件不把运行称为断点调试。

当前日常编译器对 `input()` 和 `input(prompt)` 均返回未定义函数，因此本次不提前提供这两种片段。Run 仍提供能接收 stdin 的原生终端，供支持读取输入的程序/工具链使用。

## 第三方依赖

项目需有真实 ku.mod/ku.lock。主动执行“Ku：安装依赖”，可选择恢复，输入 CLI 接受的来源，或选择本地 Ku 包/Python 文件；清单与锁由 CLI 管理。可用 `./vendor/pysubs2` 这样的本地小包离线复现；Git、PyPI 来源按所选 CLI 的 `ku help i` 配置。

安装后，`import { … } from "@pysubs2/time"` 只提示该项目锁定包实际公开的导出。能读到 Ku 源码或生成绑定声明时才能跳转，不使用其他项目缓存中的同名版本。缺锁、失配或缺缓存时，悬停 import 可查看原因和手动恢复步骤；二进制包不伪造声明位置。

打开、保存、补全、跳转不会自动安装、联网、起服务或连接数据库；受限工作区只提供静态浏览和编辑。

## 五分钟例子

先选择 CLI，在中文或带空格目录保存以下文件。输入 `rest.` 查看真实字段，悬停 `today` / `time.date`，对 `name` 跳转；随后使用 Run、Check、Build。

```ku
import time from "std.time"

struct Reply { title: str, count: int }

async fn Make(): Reply! {
    return ok(Reply { title: "你好🙂", count: 2 })
}

async fn main(): null! {
    reply = (await Make())?
    {title: name, ...rest} = reply
    println(name)
    println(rest.count)
    today = time.date()
    println(today.year)
    return ok(null)
}
```

## 开发与验收

Node.js 20+，在本目录运行 `npm ci --ignore-scripts`、`npm run lint`、`npm run typecheck`、`npm test`、`npm run build`、`npm run package`。复用已有测试，插件专用 CI 矩阵覆盖 macOS/Linux/Windows，不构建 Ku 或跑性能工作负载。CI 定义不等于三平台已经实跑；安装后的 VSIX、实际平台和限制见 [VERIFICATION.md](VERIFICATION.md)，更新见 [CHANGELOG.md](CHANGELOG.md)。Apache-2.0，保留 LICENSE.txt/NOTICE；本次不发布 Marketplace 或修改 Ku 版本标签。
