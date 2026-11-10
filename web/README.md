# Ku 官网与文档站

[English](README.en.md)

Astro + Starlight 静态站点，当前版本 **0.0.20**。在线体验使用真实 Ku 解释器的浏览器预览；构建需要完整 Ku 仓库中的浏览器适配源码，不需要数据库或执行后端。

## 本地开发

需要 Node.js 22.22.0 或更新版本。已验证 Node.js 24.18.0。

```sh
npm ci
npm run dev
```

## 构建与上传

```sh
npm run build
```

在本目录运行上述命令，会自动核对发行版并构建 WASM 和官网，完整网站输出到 `dist/`。将 `dist/` 里面的全部文件上传到网站根目录，保留 `_astro/` 静态资源和 `pagefind/` 搜索索引的目录结构。

默认使用已确认的官网地址 `https://ku-lang.com` 并允许正常页面被搜索引擎收录。需要其他域名时通过 `SITE_URL` 指定。可选检查命令为 `npm run lint` 和 `npm run check`。

如需禁止索引的本地预览，在未设置 `SITE_URL`、`INDEXABLE` 等正式环境变量的终端运行 `npm run build:preview`。`npm run preview` 只启动预览服务器，不改变已有构建的索引设置。缺译回退页继续保留 `noindex`。

## WASM 在线体验与发行版绑定

`/{语言}/playground/` 覆盖官网全部 16 种语言，在访问者的专用浏览器 Worker 内检查和运行，不上传用户源码。每次任务使用新实例；停止、离页、隐藏和执行期限到达时直接销毁 Worker。原生网络、数据库、进程、任意磁盘及原生 async 调度不属于浏览器能力。

Rust **1.99.1** 由仓库根目录 `rust-toolchain.toml` 固定；`scripts/wasm-toolchain.json` 固定 `wasm32-unknown-unknown` 和 wasm-bindgen crate/CLI **0.2.100**。首次准备工具（不改变全局默认 Rust）：

```sh
rustup target add --toolchain 1.99.1 wasm32-unknown-unknown
cargo +1.99.1 install --locked --version 0.2.100 --root "$HOME/.cache/ku-web/wasm-bindgen-0.2.100" wasm-bindgen-cli
npm run wasm
```

Windows 可使用相同的 Cargo 参数，将安装目录设置为用户目录下 `.cache/ku-web/wasm-bindgen-0.2.100`；或用 `KU_WASM_BINDGEN` 指定该版本的 CLI 完整路径。`KU_WASM_CACHE` 可指定本任务缓存目录。下载依赖需要代理时只在当前命令设置 `HTTPS_PROXY`/`HTTP_PROXY`。

`npm run build`、`npm run build:preview` 和开发启动会先构建引擎。流水线每次在线核对 GitHub 最新已发布 release（包含实验预发行），要求标签、真实提交和 Ku 主包版本与 `src/data/versions.json` 一致。若 GitHub 已更新而网站/适配尚未更新，构建直接失败，不沿用旧 WASM。

当前 GitHub `v0.0.20` 只发布了原生安装包，没有发布浏览器 WASM。构建会取该标签的真实源码，只叠加 `crates/ku-web/release-adapter.patch` 和浏览器入口源码，不把本地其他未提交的解释器改动带入引擎。产物明确标记 **browser preview**，不冒称 GitHub 已发布的 WASM 安装包。页面版本与构建 ID 来自运行中的 `engine_info()`。

构建先在系统临时目录编译匹配的 WASM/JS/type/manifest，校验真实模块内存上限 **128 MiB**、版本和构建身份，再接入 Astro。最终 `dist/playground-build.json` 记录源发行版、各项 hash 和原始/gzip/Brotli 体积。生成目录 `src/generated/ku-web/` 与 Rust 缓存都不应当手工编辑；生成失败会终止整个构建。

更新版本时先核对新的 GitHub release，更新 `versions.json` 与官网包版本，再基于新标签审查浏览器适配。显式运行 `npm run wasm -- --refresh-lock`，审查并保留 `crates/ku-web/release-Cargo.lock` 的依赖差异；常规构建始终使用 `--locked`。旧补丁不能干净应用或实际引擎身份不符时必须修复适配，禁止通过保留旧产物绕过。可单独运行 `node scripts/build-wasm.mjs --verify-release` 只读核对当前选择。

## 版本与内容

- `src/data/versions.json` 管理页面版本、下载标签和历史文档；`package.json` 与锁文件的版本必须一致。构建会检查，避免包名更新而页面仍显示旧版。
- 当前文档为 0.0.20；0.0.19、0.0.18 与 0.0.17 使用各自的归档正文和路径。旧版本不得跳转到当前版本，也不得用当前正文覆盖。
- `src/data/archive-pages.json` 列出已保留的历史页面。缺失任何页面时构建直接报错；新增归档时需显式登记，不能通过重新扫描空目录来掩盖丢失。
- 首页和文档有16种语言；完整模块参考首先维护简体中文与英文。变更示例须用对应版本 Ku 检查并运行。
- 安装页直接链接已发布的三平台包；不要填写不存在的安装器或下载地址。
- 微信赞赏图为 `public/images/wechat-donation.png`，用于官网“支持 Ku”页及仓库根中英文 README，复用同一份原图；反馈渠道为 GitHub Issues 和 `kagura.gc@qq.com`。

`dist/`、`.astro/` 和 `node_modules/` 均为生成文件，不提交。依赖按锁文件安装；`postinstall` 中的版本插件补丁用于保护现有多语言归档，不能跳过。

## 部署说明

用户自行部署。将完整 `dist/` 目录内的文件上传到网站根目录，包括 Pagefind 搜索索引与静态资源。不要启用 SPA 回退；深层页面对应自己的 `index.html`，不存在的路径返回404。

静态服务器必须为 `.wasm` 返回 `application/wasm`、为 Worker/JS 返回 JavaScript MIME，不存在的引擎资源应返回真实 404。现有本地预览已配置这些 MIME；实际服务器仍需核验。哈希资源适合长期 immutable 缓存，HTML/版本资料保持可刷新，并保留上一构建的哈希资源窗口。若托管另有 CSP，只合并 `worker-src 'self'` 与所需的 `script-src 'wasm-unsafe-eval'`，不要开放通用 `unsafe-eval`；不需要 SharedArrayBuffer/COOP/COEP。

根路径的语言选择、Cookie 与可信地区信息接入见 [Cloudflare 接入说明](deploy/cloudflare/CONNECTION.md)。本地预览不能证明线上地区分流已生效。

源码使用 Apache-2.0，第三方组件遵循各自许可。

## 本目录保留的入口与维护细则

## 语言与搜索

`src/i18n/languages.json` 是语言清单，共 16 种：德语、希腊语、英语、西班牙语、波斯语、法语、意大利语、日语、韩语、波兰语、巴西葡萄牙语、罗马尼亚语、俄语、土耳其语、简体中文、繁体中文。

正文位于 `src/content/docs/{locale}/`，首页界面位于 `src/i18n/{locale}.json`，搜索界面位于 `src/content/i18n/{lang}.json`。后者使用 BCP-47（如 zh-CN、zh-TW、pt-BR），与 URL locale 区分。波斯语为 RTL，代码与终端保持 LTR。

构建检查全部字典的键、数组、路由及搜索占位符。`src/i18n/routes.ts` 从真实内容生成译文和锚点关系；文档缺译时保留版本和主题，显示对应英文文档并提示，不把框架的合成回退页当成译文。版本切换优先进入同名文档，缺页时进入目标版本的文档入口。两类切换都保留查询参数和目标存在的锚点。canonical 与 hreflang 保留语言、版本和主题；x-default 使用对应英文页面。合成回退页 noindex，并排除 sitemap。首页搜索只查当前文档，文档搜索区分历史版本。

## 版本化文档

使用固定版本 `starlight-versions` 0.10.1。当前入口 `/{locale}/docs/…` 对应 **0.0.20**；`/{locale}/v0.0.19/…` 保留修改前的 0.0.19 文档，0.0.18 文档继续保留，版本菜单可切换；已有 0.0.17 归档继续保留。构建前检查归档元数据，缺失时直接报错，不自动用新内容重建旧快照。

增加新快照前先完成内容校准，再在 versions.json 的 archives 末尾新增条目并运行构建。提交插件生成的 `src/content/docs/{locale}/{slug}/`、`src/content/versions/{slug}.json`、`src/content/docs/{slug}/.gitkeep` 与所需资源。不要重写已发布历史正文，不提前添加未准备好的快照，dev/build 也可能生成新归档。

`src/integrations/versioning.mjs` 保留纯多语言归档的根标记，防止 Git 不保留空目录导致归档被误判为新版本。`scripts/patch-versioning.mjs` 在 postinstall 修正 0.10.1 复制旧语言快照进入新版本的问题，只改一个排除条件，断言版本与源文本。升级时必须复核，不能通过 `npm ci --ignore-scripts` 绕过。历史目录已排除日常格式化。

## 根入口与自动语言

保留现有平台无关入口 `src/entry/language.mjs` 与可选 Cloudflare 示例，具体接入见 `deploy/cloudflare/CONNECTION.md`。本次内容校准不迁移托管、不修改 DNS 或生产 Nginx。

- 显式语言 URL 直接访问，只有根路径 `/` 的 GET/HEAD 自动判定；其他根方法 405。
- 有效且唯一的 `ku_lang=zh-cn` 或 `ku_lang=en` 手动偏好优先。没有偏好时，可信国家 CN 进入简中；其他、未知、读取失败均进入英文。
- 返回 302 并保留 query，设置 `Cache-Control: private, no-store`。不读浏览器 GPS、时区或访客伪造的地区头，不存储 IP 或地区。
- 菜单仍保留 16 种语言；简中与英文写一年 host-only、SameSite=Lax 偏好，HTTPS 使用 Secure。其他语言按现有白名单策略清除旧偏好。普通链接、初始渲染及修饰键打开新页不写偏好。
- 切换语言尽量保留主题、版本、query 及目标页有效锚点。存储不可用时仍可导航。

本地 dev/preview 国家恒为未知，可验证英文回退和手动偏好；受控注入可信 CN 元数据不等于真实大陆 IP 验收。根入口需由实际网关处理，不能恢复 `/ → /zh-cn/` 固定 301/308。仅上传 dist 时，根 index.html 只是无门页兜底，不能自行识别国家或产生服务器 302。

实际 CDN 必须仅对精确根路径绕过共享缓存并清除旧根跳转，不能让一个人的偏好污染其他访客。未知生产代理链不得直接照搬请求头判定。Cloudflare 示例不是要求切换平台，也不是托管永久免费承诺。

## 构建与上线边界

用户自行部署 `dist/` 目录内的文件。线上更新需作为同一次构建完整替换 HTML、带哈希资源与 Pagefind 索引；回滚也恢复完整构建。静态深层页面应对应 index.html，不启用 SPA 回退，不存在的页面返回真实 404。

正式环境的根入口、缓存、地区识别、公共 HTTP 状态仍由实际托管环境验证。本地预览和历史上传进度都不代表已经上线。

当前版本的发行标签、源码提交和文档引用集中记录在 `src/data/versions.json`。
