---
title: 'کتابخانهٔ استاندارد'
description: 'ماژول‌های کتابخانهٔ استاندارد Ku، امضاهای اصلی و محدودیت‌های مسیرهای اجرا.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'src/stdlib/metadata.rs',
    'src/stdlib/mod.rs',
    'docs/syntax.md',
    'docs/protocol-foundation.md',
  ]
---

این مستندات محدودهٔ نهایی پیش‌انتشار آزمایشی v0.0.18 را شرح می‌دهند. وضعیت انتشار واقعی و فایل‌های دانلود در GitHub Releases ثبت می‌شوند. این نقاط ورود اصلی با کد منبع تطبیق داده شده‌اند؛ این صفحه مرجع جامع API نیست. [قرارداد نحو](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md) رفتار را تعریف می‌کند. محدودیت‌های مسیرهای اجرا را در [جدول پشتیبانی](/fa/docs/support/) ببینید.

## واردسازی ماژول‌ها

```ku
import fs from "std.fs"
import json from "std.json"
import { http, time } from "std"
```

`fs`، ‏`http`، ‏`config`، ‏`task`، ‏`pg`، ‏`redis`، ‏`mysql`، ‏`bytes` و `net` به واردسازی صریح نیاز دارند. برخی ماژول‌های قدیمی هنوز مستقیم فراخوانی می‌شوند؛ نمونه‌های جدید برای روشن بودن وابستگی‌ها، واردسازی صریح را ترجیح می‌دهند. `std.task` فضای نام مشاهدهٔ داخلی و آزمون فشار است، نه API هم‌زمانی برنامه‌ها.

| ماژول                                | کاربرد                                         | محدودیت‌های مهم                                                             |
| ------------------------------------ | ---------------------------------------------- | --------------------------------------------------------------------------- |
| `std.string` / متدهای رشته           | طول یونیکد، طول بایتی، جست‌وجو، برش            | `trim/lower/upper` بومی پیاده‌سازی یونیکد ندارند                            |
| `std.array` / متدهای آرایه           | طول، جست‌وجو، آرایهٔ جدید، map                 | شکست اندیس مستقیم با `try_get` قابل بازیابی متفاوت است                      |
| `std.object`                         | جست‌وجو با مقدار جایگزین صریح از طریق `get_or` | اندیس عادی ناموجود به‌طور پیش‌فرض null نمی‌دهد                              |
| [std.json](/fa/docs/stdlib/json/)    | کدگذاری و کدگشایی سخت‌گیرانهٔ JSON             | در Task بومی و LLVM پشتیبانی نمی‌شود                                        |
| `std.fs`                             | فایل، بررسی وجود، فهرست پوشه                   | مسیر نسبی از پوشهٔ فایل منبع فعلی `.ku` محاسبه می‌شود                       |
| `std.time`                           | زمان، تاریخ، بازه، خواب                        | انتظار همگام و انتظار Task مسیرهای اجرای متفاوت دارند                       |
| `std.http`                           | کارخواه HTTP، سرویس، پاسخ                      | تفاوت مسیر اجرا و چرخهٔ عمر باقی است؛ پیاده‌سازی کامل شبکهٔ عمومی Task نیست |
| `std.config`                         | محیط و پیکربندی                                | اطلاعات ورود واقعی را در کد منبع ثبت نکنید                                  |
| `std.pg` / `std.redis` / `std.mysql` | کارخواه‌های یکپارچه با مخزن محدود              | فقط بومی؛ بررسی سرور واقعی و محدودیت‌های TLS جداگانه لازم است               |
| `std.bytes` / `std.net`              | دادهٔ دودویی، کارخواه TCP، TLS اختیاری         | مسیر C بومی تولیدشده؛ TLS به بستهٔ متناسب با مقصد نیاز دارد                 |
| `std.lexer` / `std.parser`           | ابزار کمکی اشکال‌زدایی کامپایلر                | خلاصهٔ اشکال‌زدایی می‌دهند، نه ساختار عمومی کامل Token / AST                |

## امضاهای رایج

این امضاها برای مستندسازی‌اند. `&` حالت پارامتر فقط‌خواندنی پشتیبانی‌شده را نشان می‌دهد؛ `T!` نتیجهٔ قابل بازیابی را نشان می‌دهد:

```text
len(&value:any): int
str(&value:any): str
println(&value:any): null
string.len(&text:str): int
string.byte_len(&text:str): int
string.slice(text:str, start:int, end:int): str!
array.len(&values:[T]): int
array.try_get(values:[T], index:int): T!
fs.read(path:str): str!
fs.write(path:str, text:str): null!
fs.exists(path:str): bool
fs.read_dir(path:str): [str]!
```

مقدار `"中😀".len()` برابر 2 و `.byte_len()` برابر 7 است. طول، مقادیر اسکالر یونیکد را می‌شمارد؛ برش از اندیس نویسه به‌جای بایت UTF-8 استفاده می‌کند. `array.push` و `array.concat` بدون تغییر ورودی، آرایهٔ جدید می‌دهند.

## خواندن فایل

```ku
import fs from "std.fs"

fn main(): null! {
    text = fs.read("message.txt")?
    println(text)
    return ok(null)
}
```

ابتدا یک `message.txt` با کدگذاری UTF-8 کنار کد منبع بسازید. فایل ناموجود یا خواندن ناموفق، خطا می‌دهد. این نمونه به آن فایل ورودی وابسته است. `try_read` و `try_write` همچنان نام‌های مستعار سازگار با نوع نتیجهٔ یکسان هستند.

`read_dir` مسیرهای مرتب‌شده با مالکیت مستقل می‌دهد؛ سقف آن 10,000 ورودی و 1,000,000 بایت برای مجموع مسیرهاست. عملیات فایل در زیرمجموعهٔ محدود Task بومی از مخزن پس‌زمینهٔ ثابت استفاده می‌کند. این موضوع انتظار عمومی شبکه از همان سازوکار را اثبات نمی‌کند.

## نگهداری مرجع

فرادادهٔ رابط عمومی در [src/stdlib/metadata.rs](https://github.com/fengyanweb/ku/blob/v0.0.19/src/stdlib/metadata.rs) قرار دارد. این داده‌ها همراه پیاده‌سازی‌ها و قراردادها بررسی می‌شوند. این وب‌سایت فعلاً مرجع دستی گزینش‌شده‌ای را نگه می‌دارد؛ وجود فرمان `ku doc` تأیید نشده است.

## API reference (English)

Detailed module references and runnable examples are currently available in English:

- [http](/en/docs/stdlib/http/)
- [time](/en/docs/stdlib/time/)
- [fs](/en/docs/stdlib/fs/)
- [config](/en/docs/stdlib/config/)
- [string](/en/docs/stdlib/string/)
- [array](/en/docs/stdlib/array/)
- [object](/en/docs/stdlib/object/)
- [json](/en/docs/stdlib/json/)
- [bytes](/en/docs/stdlib/bytes/)
- [net](/en/docs/stdlib/net/)
- [database](/en/docs/stdlib/database/)
- [compiler](/en/docs/stdlib/compiler/)
