---
title: 'نصب و دریافت'
description: 'Ku 0.0.20 را برای رایانهٔ خود دانلود کنید. هر بسته شامل ابزار خط فرمان، مفسر و افزونهٔ VS Code است.'
kuVersion: '0.0.20'
verifiedAt: '2026-11-09'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.20.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

**Ku 0.0.20** را برای رایانهٔ خود دانلود کنید. هر بسته شامل ابزار خط فرمان، مفسر و افزونهٔ VS Code است.

## ۱. دانلود Ku

| سیستم   | معماری                | دانلود                                                                                                                          |
| ------- | --------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| macOS   | Apple Silicon / ARM64 | [دانلود macOS (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-aarch64-apple-darwin.tar.gz)     |
| Windows | x64                   | [دانلود Windows (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-x86_64-pc-windows-msvc.tar.gz) |
| Linux   | x64 / GNU             | [دانلود Linux (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.20/ku-v0.0.20-x86_64-unknown-linux-gnu.tar.gz) |

## ۲. استخراج و اجرا

**تمام بسته** را در پوشه‌ای خالی، مثلاً پوشهٔ `ku` در پوشهٔ کاربری خود، استخراج کنید. همهٔ فایل‌ها را کنار هم نگه دارید و فقط فایل اجرایی را جابه‌جا نکنید. در همان پوشه یک ترمینال باز کنید.

macOS / Linux:

```sh
./ku --version
```

Windows PowerShell:

```powershell
.\ku.exe --version
```

باید `ku 0.0.20` نمایش داده شود. اکنون می‌توانید فایل‌های `.ku` را اجرا و بررسی کنید؛ این کارها به نصب Rust نیاز ندارند.

<details>
<summary>اختیاری: اجرای ku از هر پوشه</summary>

اگر بسته را در `~/ku` استخراج کرده‌اید، این خط را به فایل راه‌اندازی پوستهٔ خود، مانند `~/.zshrc` یا `~/.bashrc`، اضافه کنید:

```sh
export PATH="$HOME/ku:$PATH"
```

در Windows، تنظیمات متغیرهای محیطی حساب کاربری را باز کنید و مسیر کامل پوشهٔ استخراج‌شده، مثلاً `%USERPROFILE%\ku`، را به متغیر کاربری `Path` اضافه کنید. ورودی‌های موجود را نگه دارید.

ترمینال تازه‌ای باز کنید و `ku --version` را اجرا کنید.

</details>

## ۳. نصب افزونهٔ VS Code

در بخش افزونه‌های VS Code گزینهٔ **… → Install from VSIX…** را انتخاب کنید، فایل `ku-language-0.0.20.vsix` را از پوشهٔ استخراج‌شده باز کنید و پنجره را دوباره بارگذاری کنید.

اگر Ku در `PATH` نیست، مقدار `ku.executablePath` را روی مسیر کامل `ku` یا در Windows روی `ku.exe` تنظیم کنید.

<details>
<summary>بررسی فایل دانلودشده</summary>

SHA-256 بسته را با [SHA256SUMS](https://github.com/fengyanweb/ku/releases/download/v0.0.20/SHA256SUMS) مقایسه کنید. [صفحهٔ انتشار](https://github.com/fengyanweb/ku/releases/tag/v0.0.20) برای هر بسته یک فایل `.manifest.json` شامل مجموع بررسی فایل‌ها نیز دارد. راهنمای بیشتر در فایل `README-INSTALL.md` داخل بسته است.

</details>

<details>
<summary>کامپایل برنامه‌های Ku خودتان</summary>

`ku run` و `ku check` به Rust نیاز ندارند. `ku build` پیش‌فرض به پیونددهندهٔ سیستم و هویت دقیق کامپایلر Rust ثبت‌شده در SDK نصب‌شده نیاز دارد؛ بسته‌های v0.0.20 از **Rust 1.99.1** استفاده می‌کنند. `ku build --backend c` به کامپایلر C و پیونددهندهٔ پلتفرم مقصد نیاز دارد. برنامه‌های بومی پایگاه داده به کتابخانه‌های سازگار کلاینت نیز نیاز دارند که همراه بسته نیستند.

</details>

<details>
<summary>حذف نصب</summary>

ابتدا فایل‌های منبع و تنظیماتی را که خودتان ساخته‌اید نگه دارید، سپس پوشهٔ استخراج‌شده، ورودی اضافه‌شده به `PATH` و افزونهٔ VS Code را حذف کنید.

</details>

[گام بعد: اجرای اولین برنامه](/fa/docs/getting-started/)
