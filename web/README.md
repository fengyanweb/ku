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
- 归档版本保持自己的正文和导航，不能用当前文档重新生成历史快照。
- 首页和文档有16种语言；完整模块参考首先维护简体中文与英文。变更示例须用对应版本 Ku 检查并运行。
- 安装页直接链接已发布的三平台包；不要填写不存在的安装器或下载地址。
- 微信赞赏图为 `public/images/wechat-donation.png`，仅在“支持 Ku”页展示；反馈渠道为 GitHub Issues 和 `kagura.gc@qq.com`。

`dist/`、`.astro/`、`node_modules/` 与打包产物均为生成文件，不提交。依赖按锁文件安装；`postinstall` 中的版本插件补丁用于保护现有多语言归档，不能跳过。

## 部署说明

当前任务不部署官网。未来部署时使用完整 ZIP，包括 Pagefind 搜索索引与静态资源。不要启用 SPA 回退；深层页面对应自己的 `index.html`，不存在的路径返回404。

根路径的语言选择、Cookie 与可信地区信息接入见 [Cloudflare 接入说明](deploy/cloudflare/CONNECTION.md)。本地预览不能证明线上地区分流已生效。

源码使用 Apache-2.0，第三方组件遵循各自许可。
