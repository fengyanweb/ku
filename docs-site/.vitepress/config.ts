import { defineConfig } from "vitepress";

export default defineConfig({
  title: "Ku",
  description: "Ku language documentation",
  cleanUrls: true,
  themeConfig: {
    nav: [
      { text: "Guide", link: "/guide/getting-started" },
      { text: "Syntax", link: "/guide/syntax" },
      { text: "History", link: "/reference/version-history" }
    ],
    sidebar: [
      {
        text: "Guide",
        items: [
          { text: "快速开始", link: "/guide/getting-started" },
          { text: "语法", link: "/guide/syntax" },
          { text: "命令行", link: "/guide/cli" },
          { text: "标准库", link: "/guide/stdlib" },
          { text: "Native C", link: "/guide/native-c" }
        ]
      },
      {
        text: "Reference",
        items: [{ text: "版本历史", link: "/reference/version-history" }]
      }
    ]
  }
});
