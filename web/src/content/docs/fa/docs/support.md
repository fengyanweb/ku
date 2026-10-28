---
title: 'پشتیبانی از پلتفرم‌ها و مسیرهای اجرا'
description: 'تفاوت پلتفرم‌های انتشار Ku، مفسر، اجراکننده، C بومی، زیرمجموعهٔ Task و LLVM.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'docs/syntax.md',
    'docs/v0.0.19.md',
    'docs/v0.0.18-worklog.md',
    'docs/protocol-foundation.md',
    '.github/workflows/native-three-os.yml',
  ]
---

این مستندات محدودهٔ نهایی پیش‌انتشار آزمایشی v0.0.18 را شرح می‌دهند. وضعیت انتشار واقعی و فایل‌های دانلود در GitHub Releases ثبت می‌شوند.

## پلتفرم‌های CLI منتشرشده

پیش‌انتشار آزمایشی v0.0.18 شامل Windows x64 ‏(MSVC)، ‏Linux x64 ‏(GNU) و macOS arm64 ‏(Apple Silicon) است. هر پلتفرم بایگانی خود را دارد. فایل اجرایی واحدی برای هر سه سیستم وجود ندارد. خروجی پلتفرم‌های دیگر بررسی نشده است؛ [نصب](/fa/docs/install/) را ببینید.

## مسیرهای اجرا

| قابلیت                              | مفسر `ku run`                   | اجراکنندهٔ پیش‌فرض `ku build`            | C بومی همگام                                     | Task بومی در توسعه                               | نمونهٔ اولیهٔ متن LLVM |
| ----------------------------------- | ------------------------------- | ---------------------------------------- | ------------------------------------------------ | ------------------------------------------------ | ---------------------- |
| زبان پایه، توابع، جریان کنترل       | پیاده‌سازی شده                  | مسیر مفسر                                | زیرمجموعه پیاده‌سازی شده                         | زیرمجموعهٔ نوع‌دار محدود                         | نمونهٔ اولیهٔ محدود    |
| JSON                                | پشتیبانی می‌شود                 | پشتیبانی می‌شود                          | پشتیبانی می‌شود                                  | پشتیبانی نمی‌شود                                 | پشتیبانی نمی‌شود       |
| کل گراف واردسازی محلی مستقل از منبع | منبع را بارگذاری می‌کند         | پشتیبانی نمی‌شود؛ فقط ورودی تعبیه می‌شود | هنگام تولید گسترش می‌یابد                        | در محدودهٔ زیرمجموعهٔ پشتیبانی‌شده گسترش می‌یابد | تضمین استقرار ندارد    |
| async / await                       | با محدودیت منابع پیاده‌سازی شده | مسیر مفسر                                | زیرمجموعهٔ ویژه یا رد                            | جزئی، نه async کامل                              | پشتیبانی کامل ندارد    |
| کارخواه PostgreSQL / Redis / MySQL  | دسترسی پایگاه داده ندارد        | دسترسی تفسیری پایگاه داده ندارد          | فقط بومی                                         | انتظار عمومی Task نیست                           | پشتیبانی نمی‌شود       |
| کارخواه TCP / TLS                   | مسیر بومی متناظر ندارد          | مسیر بومی متناظر ندارد                   | پیاده‌سازی C تولیدشده؛ TLS بستهٔ متناظر می‌خواهد | ادعای انتظار عمومی کامل شبکه وجود ندارد          | پشتیبانی نمی‌شود       |

ساخت پیش‌فرض به زنجیره‌ابزار Rust و پیونددهندهٔ متناظر نیاز دارد. C بومی به زنجیره‌ابزار C مقصد نیاز دارد. موفقیت `ku check` کارکرد هیچ‌یک از این ساخت‌ها را اثبات نمی‌کند. وجود فرمان LLVM به معنای مسیر انتشار کامل نیست.

## زیرمجموعهٔ بومی Task

شاخهٔ توسعهٔ فعلی مسیر منبع محدود `async fn main(): null!` را دارد. این مسیر شامل انواع اولیهٔ صریحاً فهرست‌شده، رشته، آرایهٔ تخت، Result تک‌لایه، شروع/انتقال/await برای Task، بخشی از جریان کنترل و عملیات پس‌زمینهٔ محدود فایل/پوشه است. این یک مسیر async کامل در بستهٔ منتشرشدهٔ v0.0.18 نیست.

انتظار عمومی شبکهٔ Task، اجرای کامل M:N، ترکیب‌های دلخواه هیپ پویا، همهٔ ترکیب‌های capture/تابع ناهمگام و آزمون ماندگاری طولانی کامل هنوز تمام نشده‌اند. زیرمجموعهٔ دقیق پذیرفته و ردشده را در [قرارداد هم‌زمانی](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/concurrency.md) ببینید.

## محدودیت‌های پایگاه داده و شبکه

API پایگاه داده از `module.client(config)?` و سپس متدهای گیرنده استفاده می‌کند و مخزن محدودی داخل کارخواه دارد. بررسی سرور واقعی برای هر درایور، سیستم و commit جداست. ABI ساختگی یا پیوند موفق جایگزین پرس‌وجوی واقعی نیست.

Redis از TCP متن ساده استفاده می‌کند. MySQL در Oracle/MariaDB بررسی گواهی یکسانی که در صورت شکست اعتبارسنجی اتصال را رد کند ندارد. بنابراین اتصال مستقیم باید از loopback، شبکهٔ خصوصی کنترل‌شده یا تونل TLS بررسی‌شده استفاده کند. PostgreSQL راه‌دور به `sslmode=verify-full` صریح و گواهی ریشهٔ معتبر نیاز دارد. TLS بومی در `std.net` به معنای استفادهٔ HTTP، ‏Redis یا مفسر از همان مسیر نیست.

## خودمیزبانی و سازگاری

واژه‌کاو و تجزیه‌گر خودمیزبان Ku زیرمجموعه‌های محدود و بررسی‌های تفاضلی دارند. تجزیه‌گر، بررسی‌کننده، IR و مسیر اجرای کاملِ خودمیزبان هنوز تمام نشده‌اند؛ Rust همچنان زنجیره‌ابزار راه‌اندازی اولیه را فراهم می‌کند. نسخهٔ 0.0.x سازگاری کد منبع یا ABI مربوط به C/FFI تولیدشده را تضمین نمی‌کند.

گزارش‌های کار و نتایج پیشین CI فقط محدوده و commit ذکرشدهٔ خود را اثبات می‌کنند. برای تفکیک تغییرات منتشرشده از کار توسعه از [یادداشت‌های انتشار](/fa/releases/) استفاده کنید. این وب‌سایت ادعا نمی‌کند تمام CI شاخهٔ فعلی، بارهای عملیاتی یا بررسی پذیرش پایگاه داده موفق بوده‌اند.

> اطلاعات تازهٔ v0.0.18 در ادامه فعلاً به انگلیسی آمده است؛ بقیهٔ ترجمه حفظ می‌شود. [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

## v0.0.18 release scope and known limits

This release freezes already implemented and verified functionality. It remains an **experimental prerelease**, not a production-readiness claim. [The v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) is the authority for actual publication, assets and checksums; a skipped publishing job is not release evidence.

- JSON keeps the previous implementation as its default. `json.encode` / `json.decode` share the `stringify` / `parse` entry points. The float round-trip correction for `1e20` is included; yyjson replacement and performance exploration have ended.
- Task/native async support remains the documented bounded subset. Shutdown admission and cleanup fixes do not imply a complete M:N runtime or universal network suspension.
- HTTP uses bounded socket readiness, queues and response handling, with keep-alive support and optional bounded diagnostic records. These changes do not imply that all workload acceptance has passed.
- `ku supervise` controls whole-program workers, admission, queue bytes, output and cleanup. It is not one worker per HTTP request or per Task, and it is not automatically applied to ordinary `ku run`.

**Known HTTP issue:** HTTP 504 occurred historically, including a seven-request cluster. This round's 60-second and 180-second short tests did not reproduce it. The root cause is unconfirmed, and formal 30-minute stability acceptance has not passed. It is retained as a known issue for v0.0.18 and is not a separate release blocker. The earliest three historical 504s lack enough evidence for a unique reconstruction.

**Deferred, not completed:** complete M:N, general Task network scheduling, the complete database platform matrix, and further long-running workload acceptance belong to later versions.

### Process memory boundary

| Host    | `ku supervise` guarantee and prerequisite                                                                                                                                                                                                                                                                   |
| ------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux   | cgroup v2 kernel charged memory; a writable, delegated domain root with `memory` and `pids` enabled is required. Required interfaces include `memory.max`, `memory.swap.max`, `memory.oom.group`, `pids.max` and `cgroup.kill`. Missing controls or permissions cause refusal before application execution. |
| Windows | Nested Job Object limits for individual workers and the worker group, measured as committed virtual memory, not RSS. Initial process commit is checked before its suspended application thread is released. Host policy can reject Job assignment.                                                          |
| macOS   | This isolation mode is not supported natively and is explicitly rejected before starting the application. Use a suitably configured Linux host/VM when required; there is no automatic VM bridge.                                                                                                           |

Worker accounting covers applicable Ku and third-party library allocations in that worker's resource domain. Linux cgroup accounting and Windows commit are different metrics. Linux `memory.max` may permit transient overshoot; these mechanisms do not promise an absolute ceiling on all physical memory at every instant.

The coordinator itself has no OS memory hard limit. Its bounded admission/control reserve is an accounting policy, not a coordinator RSS limit. A single Task's entire heap is not precisely metered; shared connection pools, caches and library allocations cannot be exactly attributed to each Task. External databases, the host/VM and build compilers lie outside the worker domain.

When an OS limit terminates a worker, the coordinator reports failure and only returns reserved capacity after confirmed cleanup. Some allocation failures can be reported before allocation, but third-party exhaustion is not guaranteed to become an ordinary Result. **Database operations are never automatically retried**; an exited worker does not prove rollback, and side effects may be unknown.

See the [process execution contract](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/process-execution.md), [concurrency contract](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/concurrency.md) and [HTTP acceptance record](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/http-stability-acceptance.md). Cross-platform CI and installation checks establish only their reported targets and scope, not a completed real-database matrix.
