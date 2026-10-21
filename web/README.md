# Ku 官网与文档站

Astro + Starlight 静态工程，源码在 `ku/web`；保留原有 `docs-site`。线上不需要常驻 Node 服务、业务后端或数据库。

## 运行与构建

Node 24.18.0（最低 22.22.0）及 npm。在本目录运行：

```sh
npm ci
npm run dev
```

```sh
npm run lint
npm run check
npm run build
npm run preview
```

搜索索引在 build 时生成，搜索验收使用 preview。preview 是仅本地使用的 loopback 静态服务器，复用生产入口判定函数以验证真实 HTTP 302；不是生产新增 Node 服务。终端打印实际地址，默认 `http://127.0.0.1:4321`，端口可用 `npm run preview -- --port 4322` 修改；Ctrl+C 停止。`dist/` 为最终静态产物，`.astro/` 为可重建缓存，均不提交。

## 语言

`src/i18n/languages.json` 是语言清单的唯一配置源，共 16 种：德语、希腊语、英语、西班牙语、波斯语、法语、意大利语、日语、韩语、波兰语、巴西葡萄牙语、罗马尼亚语、俄语、土耳其语、简体中文、繁体中文。

- 官网：`/{locale}/`；每语言保留核心文档，中英另提供按模块拆分的完整参考与工具/示例入口；本轮新增发布边界以简中/英文为完整版本，其余语言的英文补充有明确提示。
- 正文：`src/content/docs/{locale}/`；界面文案：`src/i18n/{locale}.json`；Starlight 的附加界面词条：`src/content/i18n/{lang}.json`。后者按 BCP-47 lang 命名（zh-CN、zh-TW、pt-BR），与路由 locale 区分，避免词条未加载。
- 波斯语为 `fa` / `فارسی`，使用 RTL；代码、终端和 API 标识保留 LTR。
- 构建校验字典键、数组和路由结构；缺少字典会失败，不隐式用英文页面冒充完整译文。
- 文档切换语言保持同一主题及文档版本。规范网址和 hreflang 保留语言、版本、主题三部分；不会把历史版 canonical 到当前版。
- 更新 API 时同时核对 `docs/syntax.md`、`docs/semantics.md`、`src/stdlib/metadata.rs` 与实现，再同步译文；代码和 CLI 命令不要翻译。

## 版本化文档

使用官方插件目录收录的社区插件 [starlight-versions](https://starlight-versions.vercel.app/getting-started/)，固定 0.10.1。它仍标记为 early development；已针对本项目的多语言、搜索、现有布局和归档行为集成验证，不自动追随新版本。

| 内容            | 路径示例                              | 维护方式                                     |
| --------------- | ------------------------------------- | -------------------------------------------- |
| 当前文档        | `/zh-cn/docs/guide/language/`         | 继续编辑 `src/content/docs/zh-cn/docs/…`     |
| 0.0.17 固定文档 | `/zh-cn/v0.0.17/`                     | 依据 v0.0.17 tag 整理，使用该版 CLI 校验示例 |
| 0.0.18 固定快照 | `/zh-cn/v0.0.18/docs/guide/language/` | 独立保留，不随当前文档更新                   |
| 当前发布记录    | `/zh-cn/releases/`                    | 随真实发布更新                               |
| 历史发布记录    | `/zh-cn/v0.0.18/releases/`            | 与该版文档一起冻结                           |

`src/data/versions.json` 集中管理当前版本与历史版本。当前文档保持原 URL，避免破坏已有链接。本轮把原 v0.0.18 开发快照定稿为该版冻结文档，当前文档和下载目标统一为 0.0.18。实际公开状态、安装包与 SHA256SUMS 仍以对应 GitHub Release 为准；构建成功或发布任务跳过不能当作已发布。此后该快照独立保留，不跟随下一版开发更新。

发布未来 0.1、1.0 文档时：

1. 完成并核对当前所有语言的文档和兼容性说明。
2. 在 versions.json 的 archives 中保留全部旧条目，每次只新增一个版本，例如 `{ "slug": "v0.1.0", "label": "0.1.0" }`。
3. 执行 `npm run build`。插件首次遇到新版本时复制正文、内部链接、侧栏和所需资源，并生成 `src/content/versions/{slug}.json`。没有准备好正文时，不要提前新增版本，因为 dev/build 都可能创建归档。
4. 提交新生成的 `src/content/docs/{locale}/{slug}/`、`src/content/versions/{slug}.json`、根目录 `src/content/docs/{slug}/.gitkeep` 及相应资源；它们是需要保留的历史资料。
5. 更新 current 为下一开发版本，继续编辑未带版本的正文。旧档不重新生成，不删除旧条目，不把它们重定向到首页。

本地集成层 `src/integrations/versioning.mjs` 在插件识别快照前恢复/保留版本目录标记：纯多语言归档会留下空的根版本目录，Git 不保留空目录；缺少标记会让上游误判为新快照。标记只保存归档存在状态，不重写历史正文。历史目录已排除在日常格式化之外。本轮 v0.0.18 是首次发布定稿，因此仅本次同步现有开发快照；以后的版本不得重写已经发布的历史正文。

另有 `scripts/patch-versioning.mjs` 通过 `postinstall` 修正 0.10.1 复制旧语言快照进入新版本的已复现问题，只改一个目录排除条件。脚本断言依赖版本及源文本，重复执行安全；源码或版本不符即停止安装，升级插件时必须复核并在上游修复后移除。不要使用 `npm ci --ignore-scripts` 跳过此补丁。

版本切换和前后页导航保持主题，文档搜索按版本过滤；首页搜索只查当前文档。历史页有明确提示和返回当前文档的入口。发布记录也随该版本归档，避免历史页显示未来发布事实。

## 索引与部署

未设置正式地址时，页面带 `noindex, nofollow`，robots 禁止抓取，不输出虚构 canonical。正式构建需要在环境中提供确认的 HTTPS `SITE_URL` 和 `INDEXABLE=true`，再执行：

```sh
npm run build:production
```

正式构建生成各语言/版本自己的 canonical、对应译文 hreflang、WebSite JSON-LD、robots 和 sitemap；预览与生产分别构建。未提供域名时跳过 sitemap 的提示是预期行为。

旧根入口的固定中文 301 和选择门页已移除。根路径必须由请求入口处理，不能再配置 `/ → /zh-cn/` 的 301/308。不要启用 SPA 回退，深层页面使用对应 index.html，不存在路径必须真实 404。仅上传 dist 到不支持请求处理的静态托管，不能获得真实 IP 地区分流。

正式域名、托管平台、缓存与区域访问质量仍需上线验收。HTML 重新验证，带哈希资源可长期缓存；回滚时恢复一次构建的完整 HTML、资源和 Pagefind 索引。

所有直接依赖固定版本，提交 package-lock.json。保留 postcss-selector-parser 7.1.6 的定向覆盖以处理旧依赖链已知问题，不使用 npm audit fix --force 随意降级框架。

发布范围、接口与限制维护在 `src/content/docs/zh-cn/` 和 `src/content/docs/en/`。本地历史核查报告独立保留，不作为本次网站源码交付或公网部署证据。

## 根入口自动语言选择（2026-10-19）

当前仓库没有已确认的官网域名或托管配置。交付采用平台无关逻辑，以及一个**尚未部署**的 Cloudflare Workers Static Assets 接入示例，不因此迁移平台、购买套餐或改变 DNS。

`src/entry/language.mjs` 是入口与 Cookie 规则的唯一来源：

1. 仅无语言根路径 `/` 的 GET/HEAD 作判断；其他根方法 405，其他路径交给正常静态路由。
2. 严格读取唯一且有效的 `ku_lang=zh-cn` / `ku_lang=en`，偏好优先，并且不访问地区来源。
3. 没有有效偏好时，可信国家代码 CN → 简体中文；其它代码（含 HK/MO/TW）、空值或读取失败 → 英文。不读取浏览器语言、时区、URL 地区参数或访客自行设置的国家/代理请求头。
4. 返回 302，Location 仅为站内固定语言前缀并保留查询参数；`Cache-Control: private, no-store`。自动跳转不写 Cookie，也不存 IP 或国家。

首页与文档共用 `LanguageSwitch`，通过 Starlight 的 LanguageSelect 组件接口接入。菜单仍含全部 16 种语言，使用可抓取、可键盘访问的普通链接。只有明确的普通左键/Enter 激活才写偏好；修改键打开新页不改当前浏览器偏好。简中/英文写一年 host-only Cookie，Path=/、SameSite=Lax，HTTPS 添加 Secure；其它 14 种按本次白名单要求仅清除旧偏好，不进入有效 Cookie 值。普通正文链接、Logo、预取和初始渲染均不保存选择。

`src/i18n/routes.ts` 从实际内容条目及渲染标题获取译文/锚点映射，保留主题与文档版本，不依赖框架生成的缺译回退 URL。没有目标译文时到目标语言首页的真实说明锚点，显示目标语言提示。有 JavaScript 时保留当前 query，原 hash 只有目标页确实存在该 ID 时保留；禁用 JavaScript 后链接仍可直接导航，但静态链接无法按每次请求动态附加 query 或验证当前 hash。文档元信息及 sitemap 只关联实际译文，框架缺译回退页设 noindex 并排除站点地图。

本地 dev 与 preview 的地区恒为未知，因此根入口默认英文；可用菜单实际写入 Cookie 后重开 `/` 验证偏好。没有供访客伪造地区的 query/header 调试后门。CN/IPv4/IPv6 等边界通过受控测试注入可信元数据验证，不能把它称为真实公网位置验收。

`src/pages/index.astro` 仅作错误接入纯静态托管时的无门页兜底：有 JS 读取同一 Cookie 并立即跳转，无 JS 则 meta refresh 到英文。它不是服务器 302，也不能识别国家；正确接入后根请求不会落到此文件。根 URI 的 fragment 不会发送给服务器；服务器只保留 query。浏览器是否继承 fragment 必须单独验收，不把浏览器行为说成服务器识别。

### 可接地区来源与缓存

参见 [Cloudflare 接入说明](deploy/cloudflare/CONNECTION.md)及 `wrangler.example.jsonc`：仅根 URI 配置 `run_worker_first: ["/"]`，否则 asset-first 会先返回根静态 HTML；正文/资源继续静态处理。Worker 只读取平台的 `request.cf.country`，不信任 `CF-IPCountry`、XFF 或 X-Real-IP。未使用离线 MMDB，所以没有数据库下载、更新任务、二进制数据或 DB-IP 署名。将来若确认现有网关是 Nginx，应在其真实配置中接同一规则和可信代理链，不能直接套未核验的生产片段。

实际 CDN 必须对**精确根 URI /** 绕过共享缓存并清除旧根入口 301/200；不要只靠 Vary: Cookie，也不要把带 ku_lang 的全站请求都禁用缓存。没有账号配置证据时，代码通过不等于 CDN 规则已经生效。

本方案不调用付费定位 API、不增加业务后端；并不意味着托管永久免费。Cloudflare 文档目前给 Workers Free 100,000 请求/日和 10ms CPU/次；Static Assets 的 worker-first 根请求超额可能返回 429，不会自动执行英文降级。应以实际账号计划及最新官方限制为准：[国家信息](https://developers.cloudflare.com/network/ip-geolocation/)、[Worker 静态路由](https://developers.cloudflare.com/workers/static-assets/routing/worker-script/)、[额度与超额行为](https://developers.cloudflare.com/workers/static-assets/billing-and-limitations/)。

上线前只缺：正式规范域名（含 www/裸域策略）、实际托管/网关配置及发布和根路径缓存规则的管理权限；若经源站请求头接地区，还需实际可信代理链及是否允许绕过 CDN 直连。线上入口行为仍须按本节所列条件在实际托管环境验收；本地历史验收报告不替代公网发布证据。

## v0.0.18 发布收尾范围（2026-10-20）

简中/英文以及已有 v0.0.18 快照已同步发布范围、安装目标、JSON 旧默认、进程限制和后续计划。其他 14 种语言保留已有内容；新增的英文发布说明有本地语言提示并链接完整参考，不冒充已翻译内容。

HTTP 已知问题准确记录为：历史出现过，本轮短测未复现，根因未确认，正式 30 分钟验收尚未通过；本版按维护方决定保留为已知问题，不单独阻塞发布。完整 M:N、通用网络调度、数据库完整平台矩阵移入后续版本，两个暂停项继续暂停。

`ku supervise` 仅在具备委托 cgroup v2 条件的 Linux 或允许 Job Object 的 Windows 上启用，Windows 限制提交量，macOS 原生拒绝。协调器自身没有 OS 硬限，单 Task 完整堆未精确计量，不能宣称整个系统内存绝对封顶。详见各语言支持页与仓库 `docs/process-execution.md`。

本轮只交付网站源码与构建验证，不把此前上传进度、预览或本地构建说成公网部署完成。

## 文档组织与示例核对

当前导航按入门、语言与工具、标准库、示例、项目组织。标准库有独立首页，HTTP、时间、文件、数据库等模块分别维护参数、返回值、错误、生命周期和后端要求。信息架构参考 [Go Documentation](https://go.dev/doc/) 与 [Rust Standard Library](https://doc.rust-lang.org/std/)。

0.0.17 内容来自真实 tag，不从当前页面自动复制；0.0.1–0.0.16 的原始开发记录在发布页集中列出，不虚构独立 Release。版本选择器只链接实际存在的同主题页，缺失时回到对应版本入口；没有该语言的历史版本时明确进入英文内容。

新增或修改 Ku 示例时：使用发布 CLI 的 `check`；自包含程序实际 `run`；声明 native 的程序还需实际构建运行。HTTP 使用本地回环、有界等待和端口清理；数据库需要真实服务的示例单列依赖，静态检查/编译不能写成查询已通过。临时脚本、输入文件和构建副本放项目外，完成后清理。不要把内部工作安排或 AI 提示写入用户参考页面。
