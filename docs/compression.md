# 可选原生压缩模块

`std.zstd`、`std.gzip` 和 `std.deflate` 使用现有 `bytes` 类型，供同步 native C 程序调用。它们不改变 HTTP 的压缩策略、JSON 引擎或默认分配器。未使用这些模块的程序不会链接压缩库，也不会创建压缩上下文、线程或缓冲区。

| 模块      | 压缩格式                                        | 实现                            |
| --------- | ----------------------------------------------- | ------------------------------- |
| `zstd`    | Zstandard frame；编码启用内容校验和             | Zstandard 1.5.7，默认压缩级别 3 |
| `gzip`    | RFC 1952 gzip member，内部使用 DEFLATE          | zlib 1.4，默认压缩级别及策略    |
| `deflate` | RFC 1951 裸 DEFLATE stream，不带 gzip/zlib 包装 | 同一 zlib 1.4                   |

每个模块均提供两个方法：

```text
compress(data, max_output_bytes)   -> bytes!
decompress(data, max_output_bytes) -> bytes!
```

`data` 是只读输入，不会被消费、修改或深复制；调用后原值仍可使用。成功结果是独立 owned `bytes`，与输入及已经销毁的库上下文无关，按正常生命周期释放。失败以 Ku Result 返回，不返回不完整的解码结果。

```ku
import { bytes, zstd, gzip, deflate } from "std"

fn main(): null! {
    source = bytes.from_str("中文😀")?
    encoded = zstd.compress(source, 1024)?
    decoded = zstd.decompress(encoded, 1024)?
    println(decoded.to_str()?)
    println(source.to_str()?)
    return ok(null)
}
```

## 上限与格式规则

- 必须显式指定输出上限，为 `0..67108864` 字节。输入仍受现有 `bytes` 64 MiB 上限约束。
- 输出渐进分配，容量不超过调用者给出的上限；不会根据流中声称的解压长度直接分配整块内存。内部使用 32 KiB 暂存块，在达到输出上限时只允许多探测一个字节以区分完整尾部与超限。
- 零输出上限允许解码合法的空 frame/member；非空输出返回 `output_limit`。
- Zstandard 保留 1.5.7 默认的解码 window 上限，即 128 MiB。它与输出上限不同；库上下文、window、熵表、分配器开销和输入仍会占用内存。输出上限不是进程整体内存上限。
- gzip/裸 DEFLATE 使用默认的 32 KiB window 和 `memLevel=8`。本次库分配通过匹配的 `zalloc/zfree` 管理，含本模块分配头在内最多 2 MiB；该值覆盖上游固定默认配置约 268 KiB 的需求，不缩小 window 或改变压缩策略。
- Zstandard 解码完整的串联 frames（包括标准 skippable frames）；gzip 解码完整的串联 members。统一累计输出上限，截断或尾部垃圾均返回错误。
- 裸 DEFLATE 只接受单一完整 stream，拒绝额外尾部数据。裸格式没有校验和，合法但内容改变的裸流不一定能检测出来。
- 不支持外部字典。库报告需要字典时返回明确错误。

常见错误码为 `invalid_argument`、`output_limit`、`truncated_input`、`invalid_data`、`out_of_memory`；还可能返回 `window_limit`、`workspace_limit`、`dictionary_required` 或 `library_version_mismatch`。错误 domain 与模块一致。校验失败不会被转成空值或成功。

## 构建及执行范围

三个模块目前是同步 native C 能力，不公开流式状态句柄。runner/解释器不具备对应二进制 `bytes` 实现；构建这些后端时必须提前拒绝。Task 路径在接入相应预算、取消和期限适配前同样不能接受这些调用。

使用模块时需要对应目标平台的官方头文件和库，编译头与实际链接库的版本必须匹配。Zstandard 保留 BSD-3-Clause 许可，zlib 保留其 zlib 许可；原生依赖包应包含原许可证。不能用系统旧库或错误架构的库冒充匹配版本，也不应通过全局替换系统压缩库来安装这些可选模块。

CLI 默认从自身安装目录的 `native-compression/v1/<target>/` 读取可选依赖包。每个 `zstd/` 或 `zlib/` 目录包含 `lib/`、`include/`、`LICENSE` 与 `manifest.json`；`<target>` 为完整目标 triple。配套包就位后使用普通 `ku build --backend c --release main.ku`，无需额外压缩专用构建参数。

需要选择其他包时，`KU_ZSTD_LIB`、`KU_ZLIB_LIB` 必须指向该包的绝对 `lib/` 路径；头文件默认取同级 `include/`，也可用对应的 `KU_ZSTD_INCLUDE`、`KU_ZLIB_INCLUDE` 绝对路径覆盖。文件大小、SHA256、目标对象格式、ABI、CRT及版本都必须匹配 manifest。显式配置为空、相对路径、缺失文件或校验失败时直接报错，不再搜索系统旧库。静态库与头文件通过私有副本交给 C 编译器，产物不依赖开发机的压缩动态库路径。

压缩库静态链接与整个程序的 `--static` 是两层设置。Linux 的 `--static` 继续由所选 C 工具链及其静态系统库决定；macOS 保留现有系统链接限制。MSVC 依赖包使用 `/MD`，不接受要求 `/MT` 的 `--static` 组合。CLI 按实际 C 编译器 ABI 选择依赖包，不能将 MinGW 与 MSVC 库混用。路径以独立参数传给编译器，可包含空格和 Unicode；不写入 shell 命令或要求移动到 ASCII 路径。

这里不承诺不同压缩算法、平台或上游版本的编码字节完全相同，也不把上游单库速度作为 Ku 的整服务成绩。

实现选择 stock zlib：它直接提供这里使用的稳定 `z_stream`、gzip/raw window 和 `zalloc/zfree` 接口。zlib-ng 也提供兼容模式及架构优化，但需要另行冻结兼容构建和 CPU 分派配置；目前没有 Ku 完整调用收益证据，因此保留单一实现。这个选择不宣称 zlib 更快，也不改变 `ureq` 的传递依赖后端。

官方资料：[Zstandard 1.5.7](https://github.com/meta/zstd/releases/tag/v1.5.7)、[zlib 1.4](https://www.zlib.net/)、[zlib 内存配置](https://github.com/madler/zlib/blob/v1.4/zconf.h.in)、[zlib-ng 构建与兼容模式](https://github.com/zlib-ng/zlib-ng)。
