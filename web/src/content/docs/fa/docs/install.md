---
title: 'نصب و دریافت'
description: 'دریافت پیش‌انتشار آزمایشی Ku 0.0.19 و آشنایی با نیازمندی‌های پلتفرم و ساخت.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  [
    'Cargo.toml',
    'docs/v0.0.19.md',
    'docs/history.md',
    '.github/scripts/export-release.py',
  ]
---

**Ku 0.0.19** را برای رایانهٔ خود دانلود کنید. هر بستهٔ این پیش‌انتشار آزمایشی شامل ابزار خط فرمان، مفسر و افزونهٔ VS Code است.

## ۱. دانلود Ku

| سیستم   | معماری                | دانلود                                                                                                                          | حجم     |
| ------- | --------------------- | ------------------------------------------------------------------------------------------------------------------------------- | ------- |
| macOS   | Apple Silicon / ARM64 | [دانلود macOS (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-aarch64-apple-darwin.tar.gz)     | 39.8 MB |
| Windows | x64                   | [دانلود Windows (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-x86_64-pc-windows-msvc.tar.gz) | 44.1 MB |
| Linux   | x64 / GNU             | [دانلود Linux (.tar.gz)](https://github.com/fengyanweb/ku/releases/download/v0.0.19/ku-v0.0.19-x86_64-unknown-linux-gnu.tar.gz) | 48.3 MB |

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

باید `ku 0.0.19` نمایش داده شود. اکنون می‌توانید فایل‌های `.ku` را اجرا و بررسی کنید؛ این کارها به نصب Rust نیاز ندارند.

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

در بخش افزونه‌های VS Code گزینهٔ **… → Install from VSIX…** را انتخاب کنید، فایل `ku-language-0.0.19.vsix` را از پوشهٔ استخراج‌شده باز کنید و پنجره را دوباره بارگذاری کنید.

اگر Ku در `PATH` نیست، مقدار `ku.executablePath` را روی مسیر کامل `ku` یا در Windows روی `ku.exe` تنظیم کنید.

<details>
<summary>بررسی فایل دانلودشده</summary>

SHA-256 بسته را با [SHA256SUMS](https://github.com/fengyanweb/ku/releases/download/v0.0.19/SHA256SUMS) مقایسه کنید. [صفحهٔ انتشار](https://github.com/fengyanweb/ku/releases/tag/v0.0.19) برای هر بسته یک فایل `.manifest.json` شامل مجموع بررسی فایل‌ها نیز دارد. راهنمای بیشتر در فایل `README-INSTALL.md` داخل بسته است.

</details>

<details>
<summary>کامپایل برنامه‌های Ku خودتان</summary>

`ku run` و `ku check` فقط به CLI همان سیستم نیاز دارند. `ku build` پیش‌فرض علاوه بر آن به Rust **1.89.0** و پیونددهندهٔ سیستم نیاز دارد. `ku build --backend c` به کامپایلر C و پیونددهندهٔ مقصد نیاز دارد. برنامه‌های بومی پایگاه داده نیز به کتابخانه‌های کلاینت سازگار نیاز دارند که در بسته نیستند.

</details>

<details>
<summary>حذف نصب</summary>

ابتدا فایل‌های منبع و تنظیماتی را که خودتان ساخته‌اید نگه دارید، سپس پوشهٔ استخراج‌شده، ورودی اضافه‌شده به `PATH` و افزونهٔ VS Code را حذف کنید.

</details>

[گام بعد: اجرای اولین برنامه](/fa/docs/getting-started/)
