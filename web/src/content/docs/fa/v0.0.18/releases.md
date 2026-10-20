---
title: یادداشت‌های انتشار
description: 'Ku v0.0.18 experimental prerelease scope, downloads and known limitations.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  - docs/v0.0.18.md
  - docs/v0.0.18-worklog.md
  - docs/history.md
  - Cargo.toml
slug: fa/v0.0.18/releases
---

این مستندات محدودهٔ نهایی پیش‌انتشار آزمایشی v0.0.18 را شرح می‌دهند. وضعیت انتشار واقعی و فایل‌های دانلود در GitHub Releases ثبت می‌شوند.

> اطلاعات تازهٔ v0.0.18 در ادامه فعلاً به انگلیسی آمده است؛ بقیهٔ ترجمه حفظ می‌شود. [English](/en/v0.0.18/docs/support/) · [简体中文](/zh-cn/v0.0.18/docs/support/)

## v0.0.18 release scope and known limits

This release freezes already implemented and verified functionality. It remains an **experimental prerelease**, not a production-readiness claim. [The v0.0.18 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.18) is the authority for actual publication, assets and checksums; a skipped publishing job is not release evidence.

- JSON keeps the previous implementation as its default. `json.encode` / `json.decode` share the `stringify` / `parse` entry points. The float round-trip correction for `1e20` is included; yyjson replacement and performance exploration have ended.
- Task/native async support remains the documented bounded subset. Shutdown admission and cleanup fixes do not imply a complete M:N runtime or universal network suspension.
- HTTP uses bounded socket readiness, queues and response handling, with keep-alive support and optional bounded diagnostic records. These changes do not imply that all workload acceptance has passed.
- `ku supervise` controls whole-program workers, admission, queue bytes, output and cleanup. It is not one worker per HTTP request or per Task, and it is not automatically applied to ordinary `ku run`.

**Known HTTP issue:** HTTP 504 occurred historically, including a seven-request cluster. This round's 60-second and 180-second short tests did not reproduce it. The root cause is unconfirmed, and formal 30-minute stability acceptance has not passed. It is retained as a known issue for v0.0.18 and is not a separate release blocker. The earliest three historical 504s lack enough evidence for a unique reconstruction. Do not describe the issue as repaired or long-term stability as accepted.

**Deferred, not completed:** complete M:N, general Task network scheduling, the complete database platform matrix, and further long-running workload acceptance belong to later versions. General value copy/release follow-up and AST analysis remain paused. No unrelated memory-limit validation is being repeated.

### Process memory boundary

| Host | `ku supervise` guarantee and prerequisite |
| --- | --- |
| Linux | cgroup v2 kernel charged memory; a writable, delegated domain root with `memory` and `pids` enabled is required. Required interfaces include `memory.max`, `memory.swap.max`, `memory.oom.group`, `pids.max` and `cgroup.kill`. Missing controls or permissions cause refusal before application execution. |
| Windows | Nested Job Object limits for individual workers and the worker group, measured as committed virtual memory, not RSS. Initial process commit is checked before its suspended application thread is released. Host policy can reject Job assignment. |
| macOS | This isolation mode is not supported natively and is explicitly rejected before starting the application. Use a suitably configured Linux host/VM when required; there is no automatic VM bridge. |

Worker accounting covers applicable Ku and third-party library allocations in that worker's resource domain. Linux cgroup accounting and Windows commit are different metrics. Linux `memory.max` may permit transient overshoot; these mechanisms do not promise an absolute ceiling on all physical memory at every instant.

The coordinator itself has no OS memory hard limit. Its bounded admission/control reserve is an accounting policy, not a coordinator RSS limit. A single Task's entire heap is not precisely metered; shared connection pools, caches and library allocations cannot be exactly attributed to each Task. External databases, the host/VM and build compilers lie outside the worker domain.

When an OS limit terminates a worker, the coordinator reports failure and only returns reserved capacity after confirmed cleanup. Some allocation failures can be reported before allocation, but third-party exhaustion is not guaranteed to become an ordinary Result. **Database operations are never automatically retried**; an exited worker does not prove rollback, and side effects may be unknown.

See the [process execution contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/process-execution.md), [concurrency contract](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/concurrency.md) and [HTTP acceptance record](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/http-stability-acceptance.md). Cross-platform CI and installation checks establish only their reported targets and scope, not a completed real-database matrix.


## v0.0.17 · پیش‌انتشار آزمایشی

در **2026-10-18 (America/Los_Angeles)** منتشر شد؛ زمان UTC برابر `2026-10-19T03:51:10Z` است. انتشار دارای `prerelease=true` و `draft=false` است. در حال حاضر انتشار پایدار تأیید نشده است.

تغییرهای اصلی:

* پارامترهای امانت‌گرفتهٔ همگام و فقط‌خواندنی از `&name: T` و آرگومان معمولی در فراخوانی استفاده می‌کنند. پارامترهای عادیِ مالک‌شده، معنای انتقال مالکیت را حفظ می‌کنند.
* زیرمجموعهٔ تجزیه‌گر خودمیزبان، `while` آکولاددار را با سقف تودرتویی و تعداد دستور اضافه می‌کند.
* آزمون‌های چندسکویی، بررسی قطع خروجی استاندارد را اصلاح می‌کنند. API تجاری اضافه نمی‌شود.
* بایگانی‌ها شامل وابستگی‌های Rust متناظر با اجراکنندهٔ پیش‌فرض و بستهٔ TLS مقصد هستند. همچنان ابزارهای خارجی مناسب لازم‌اند.

دریافت‌ها شامل Windows x64، ‏Linux x64 و macOS arm64 هستند. هفت فایل انتشار عبارت‌اند از سه بایگانی پلتفرم، سه manifest و `SHA256SUMS`. [نصب](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) و [یادداشت کامل انتشار](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) را ببینید.

### سازگاری و محدودیت‌ها

Ku همچنان آزمایشی است و برای محیط عملیاتی آماده نیست. نسخهٔ 0.0.x سازگاری کد منبع یا ABI مربوط به C/FFI تولیدشده را تضمین نمی‌کند. هنگام ارتقا، خروجی‌های بومی مربوط را دوباره کامپایل کنید. پارامتر امانت‌گرفتهٔ ناهمگام، خودمیزبانی کامل و async بومی کامل در محدودهٔ این انتشار نیستند.

اجراکنندهٔ پیش‌فرض فقط کد ورودی را تعبیه می‌کند؛ ماژول‌های واردشده هنوز به محل اصلی نیاز دارند. کارخواه‌های پایگاه داده فقط بومی‌اند. محدودیت سرور واقعی، TLS و وضعیت نشست هر درایور را بررسی کنید. کد خود Ku همچنان مجوز متن‌باز ندارد.
