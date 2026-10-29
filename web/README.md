# Ku 官网与文档站

[English](README.en.md)

Astro + Starlight 静态站点，当前版本 **0.0.19**。可单独复制整个 `web` 目录构建，无需上层 Ku 源码或数据库。

## 本地开发

需要 Node.js 22.22.0 或更新版本。已验证 Node.js 24.18.0。

```sh
npm ci
npm run dev
```

## 检查与打包

```sh
npm run lint
npm run check
npm run package
```

打包会重新构建网站，生成 `ku-website-v0.0.19.zip` 和同名 `.sha256` 校验文件。ZIP 内直接包含网站文件，不需要额外一层目录。命令只生成本地文件，不上传、不部署。macOS/Linux 使用系统 `zip`，Windows 使用 PowerShell 的 `Compress-Archive`。

`npm run package` 默认使用已确认的官网地址 `https://ku.baiduc.fun`；需要其他域名时通过 `SITE_URL` 指定。`npm run build` 生成禁止索引的本地预览，`npm run preview` 启动预览服务器。

## 版本与内容

- `src/data/versions.json` 管理页面版本、下载标签和历史文档；`package.json` 与锁文件的版本必须一致。构建会检查，避免包名更新而页面仍显示旧版。
- 当前文档为 0.0.19；0.0.18 与 0.0.17 使用各自的归档正文和路径。旧版本不得跳转到当前版本，也不得用当前正文覆盖。
- `src/data/archive-pages.json` 列出已保留的历史页面。缺失任何页面时构建直接报错；新增归档时需显式登记，不能通过重新扫描空目录来掩盖丢失。
- 首页和文档有16种语言；完整模块参考首先维护简体中文与英文。变更示例须用对应版本 Ku 检查并运行。
- 安装页直接链接已发布的三平台包；不要填写不存在的安装器或下载地址。
- 微信赞赏图为 `public/images/wechat-donation.png`，仅在“支持 Ku”页展示；反馈渠道为 GitHub Issues 和 `kagura.gc@qq.com`。

`dist/`、`.astro/`、`node_modules/` 与打包产物均为生成文件，不提交。依赖按锁文件安装；`postinstall` 中的版本插件补丁用于保护现有多语言归档，不能跳过。

## 部署说明

当前任务不部署官网。未来部署时使用完整 ZIP，包括 Pagefind 搜索索引与静态资源。不要启用 SPA 回退；深层页面对应自己的 `index.html`，不存在的路径返回404。

根路径的语言选择、Cookie 与可信地区信息接入见 [Cloudflare 接入说明](deploy/cloudflare/CONNECTION.md)。本地预览不能证明线上地区分流已生效。

源码使用 Apache-2.0，第三方组件遵循各自许可。

## 本目录保留的入口与维护细则

## 语言与搜索

`src/i18n/languages.json` 是语言清单，共 16 种：德语、希腊语、英语、西班牙语、波斯语、法语、意大利语、日语、韩语、波兰语、巴西葡萄牙语、罗马尼亚语、俄语、土耳其语、简体中文、繁体中文。

正文位于 `src/content/docs/{locale}/`，首页界面位于 `src/i18n/{locale}.json`，搜索界面位于 `src/content/i18n/{lang}.json`。后者使用 BCP-47（如 zh-CN、zh-TW、pt-BR），与 URL locale 区分。波斯语为 RTL，代码与终端保持 LTR。

构建检查全部字典的键、数组、路由及搜索占位符。`src/i18n/routes.ts` 从真实内容生成译文和锚点关系；文档缺译时保留版本和主题，显示对应英文文档并提示，不把框架的合成回退页当成译文。版本切换优先进入同名文档，缺页时进入目标版本的文档入口。两类切换都保留查询参数和目标存在的锚点。canonical 与 hreflang 保留语言、版本和主题；x-default 使用对应英文页面。合成回退页 noindex，并排除 sitemap。首页搜索只查当前文档，文档搜索区分历史版本。

## 版本化文档

使用固定版本 `starlight-versions` 0.10.1。当前入口 `/{locale}/docs/…` 对应 **0.0.19**；`/{locale}/v0.0.18/…` 保留修改前的 0.0.18 文档，版本菜单可切换；已有 0.0.17 归档继续保留。构建前检查归档元数据，缺失时直接报错，不自动用新内容重建旧快照。

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

## 打包与上线边界

用户自行部署。交付静态包包含完整 dist，源码包不含 node_modules、构建缓存、凭据或内部历史核查报告。线上更新需作为同一次构建完整替换 HTML、带哈希资源与 Pagefind 索引；回滚也恢复完整构建。静态深层页面应对应 index.html，不启用 SPA 回退，不存在的页面返回真实 404。

正式环境的根入口、缓存、地区识别、公共 HTTP 状态仍由实际托管环境验证。本地预览和历史上传进度都不代表已经上线。

版本数据继续保留 `languageVersion`、`releaseCommit`、`docsSourceRef`、`releaseUrl` 等扩展字段；当前标签 `v0.0.19` 的真实提交为 `73e9f2fef6861b0bc1ce441471726b926a8a174b`.
