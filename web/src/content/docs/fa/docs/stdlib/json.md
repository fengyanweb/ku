---
title: 'مرجع JSON'
description: 'کدگذاری و کدگشایی JSON در Ku، تبدیل KuValue، کدهای خطا و پشتیبانی از مسیرهای اجرا.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'src/stdlib/json.rs',
    'src/stdlib/metadata.rs',
    'docs/syntax.md',
    'examples/json.ku',
    'docs/v0.0.19.md',
  ]
---

> اطلاعات تازهٔ v0.0.18 در ادامه فعلاً به انگلیسی آمده است؛ بقیهٔ ترجمه حفظ می‌شود. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

`json.encode` / `json.decode` are available in v0.0.18 and share the `stringify` / `parse` implementations. Compatibility names remain available. The previous JSON implementation remains the default; yyjson is not used.

## واردسازی و رابط‌ها

```ku
import json from "std.json"
```

| رابط                         | نوع بازگشتی | معنا                                                |
| ---------------------------- | ----------- | --------------------------------------------------- |
| `json.stringify(&value:any)` | `str!`      | کدگذاری فقط‌خواندنی به متن JSON فشردهٔ جدید         |
| `json.parse(text:str)`       | `KuValue!`  | تجزیهٔ سخت‌گیرانه به مقدار پویای دارای مالکیت مستقل |
| `json.try_parse(text:str)`   | `KuValue!`  | نام مستعار سازگار برای `parse`                      |
| `json.encode(&value:any)`    | `str!`      | نام مستعار `stringify` در شاخهٔ توسعه               |
| `json.decode(text:str)`      | `KuValue!`  | نام مستعار `parse` در شاخهٔ توسعه                   |

`!` نشان‌دهندهٔ Result است. از `?` برای انتقال خطا یا `try/catch` برای رسیدگی استفاده کنید. کدگذاری ورودی را مصرف یا تغییر نمی‌دهد.

## کدگذاری و خواندن مقدارها

این یک نمونهٔ پایهٔ کامل است. شاخهٔ توسعه در [examples/json.ku](https://github.com/fengyanweb/ku/blob/v0.0.19/examples/json.ku) نمونهٔ بازگشت‌سنجی گسترده‌تری دارد.

```ku
import json from "std.json"

fn main(): null! {
    value = { name: "Ku", count: 7 }
    text = json.stringify(value)?
    println(text)

    decoded = json.parse(text)?
    println(decoded["name"]?.as_str()?)
    println(decoded["count"]?.as_int()? + 1)
    return ok(null)
}
```

کلیدهای شیء به‌طور قطعی مرتب می‌شوند. خروجی مورد انتظار:

```text
{"count":7,"name":"Ku"}
Ku
8
```

`KuValue` تجزیه‌شده نمی‌تواند مستقیم در محاسبات صحیح شرکت کند. ابتدا با `.as_int()?` یا `.as_str()?` تبدیل کنید. ناسازگاری نوع، خطا برمی‌گرداند. کلیدهای ناموجود شیء نیز به‌جای مقدار پیش‌فرض خودکار، خطا می‌دهند.

## داده‌ها و مرزها

ورودی‌های پشتیبانی‌شده شامل رشته، مقدار بولی، عدد صحیح، ممیز شناور متناهی، null، آرایهٔ تودرتو، شیء با کلید رشته‌ای و ساختار قابل نمایش هستند. تابع، Task، دستگیرهٔ منبع، Result، enum و ارجاع چرخه‌ای قابل کدگذاری نیستند.

| قاعده              | پیاده‌سازی فعلی                                                                     |
| ------------------ | ----------------------------------------------------------------------------------- |
| نحو JSON           | سخت‌گیرانه؛ نظرها، ویرگول پایانی، ورودی اضافی انتهایی و یونیکد نامعتبر را رد می‌کند |
| ورودی / خروجی      | هرکدام تا 1,000,000 بایت                                                            |
| بودجهٔ عمق         | عمق ریشه 0 است؛ `depth > 32` رد می‌شود؛ 33 لایهٔ ظرف خالی پذیرفته و 34 رد می‌شود    |
| اعداد صحیح         | ۶۴ بیتی علامت‌دار؛ سرریز خطاست و تبدیل خودکار به float انجام نمی‌شود                |
| ممیز شناور         | binary64 متناهی؛ نگارش اصلی، برچسب float و صفر منفی حفظ نمی‌شود                     |
| یونیکد             | چینی، ایموجی، گریزها و NUL تعبیه‌شده                                                |
| کلیدهای تکراری شیء | تجزیه آخرین مقدار را نگه می‌دارد                                                    |
| ترتیب کلید         | کدگذاری بر اساس ترتیب بایت UTF-8 مرتب می‌کند                                        |

A float such as `1.0` may encode as `1`. When an integer-looking float is outside int64, the encoder adds `.0` to keep decoding on the float path: `1e20` encodes as `100000000000000000000.0` and decodes again. Finite binary64 values do not preserve original spelling, a float tag or the sign of negative zero.

## خطاها

خطاهای قابل بازیابی از دامنهٔ `json` استفاده می‌کنند. کدهای رایج `parse_error`، ‏`stringify_error` و `out_of_memory` هستند. `byte N` در پیام تجزیه، فاصلهٔ بایتی UTF-8 از صفر است. برای رسیدگی حداقلی به ورودی نامعتبر، [خطاها و عیب‌یابی](/fa/docs/guide/errors/) را ببینید.

## پشتیبانی از مسیرهای اجرا

مفسر، اجراکنندهٔ بسته‌بندی‌شدهٔ پیش‌فرض و مسیر C بومی همگام، پیاده‌سازی JSON دارند. **زیرمجموعهٔ محدود Task بومی و LLVM از JSON پشتیبانی نمی‌کنند.** واردسازی در اجراکنندهٔ پیش‌فرض همچنان به محل کد منبع وابسته است؛ پشتیبانی JSON به معنای بستهٔ کاملاً مستقل نیست.

امضاها و محدودیت‌های این صفحه با کد منبع و قراردادها تطبیق داده شده‌اند. بررسی اجرا باید نسخه، commit و مسیر اجرا را مشخص کند. [قرارداد کامل نحو JSON](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md#127-json) را ببینید.
