# 在 Ku 包中调用 Python

Ku 在首次调用时嵌入选定的 CPython，同一应用内使用匹配的解释器和相容的 wheel。桥接接口支持 `str`、`int`、`bool` 和 `null`。安装读取 Python 声明时使用语法树；包代码在应用执行时才导入。没有足够类型信息的普通函数使用显式 JSON 边界，接收包含位置参数的 JSON 数组字符串，返回 JSON 字符串。Python 异常作为 Ku 的 `Result` 错误传播。

## 安装本地 Python 文件

在有 `ku.mod` 的项目内创建 `tools/subtitles.py`。

```python
def Clean(text: str) -> str:
    return text.strip()
```

```sh
ku i ./tools/subtitles.py --module text
```

把安装输出的导入复制到 `src/main.ku` 中。

```ku
import { Clean } from "@subtitles/text"

fn main(): null! {
    println(Clean("  你好，Ku  ")?)
    return ok(null)
}
```

```sh
ku run src/main.ku
ku build --release src/main.ku -o clean
./clean
```

生成的 `vendor/ku-python-subtitles` 是可提交的包源码。分享时带上该目录、`ku.mod` 和 `ku.lock`。安装不会覆盖作者编辑过的适配器。Windows PowerShell 把上面的构建输出改为 `-o clean.exe`，然后运行 `.\clean.exe`。

`--python` 选择这次安装使用的 Python 可执行文件，`KU_PYTHON` 可显式选择后续 CLI 调用使用的本机解释器。解释器须是 CPython 3.9–3.14 的常规 GIL 版本，带配套的公共开发头文件和共享库；PyPI wheel 匹配还使用该解释器安装的 pip 所带的 packaging。需要可用的主机 C 编译器，`KU_INTEROP_CC` 可选择它。解释器路径属于本机准备环境，不写进项目或包锁定内容。

## 连接 PyPI 发行包

`pypi:` 明确以 PyPI 为来源。`pypi:pillow==11.3.0` 安装名为 Pillow 的发行包。可调用的 Python 模块由作者选择，例如 `PIL.Image`。省略 `--module` 只准备轮子，不推断导入名。纯 Python 包和平台 wheel 都记录完整工件名和 SHA-256。安装不运行源码构建后端。再次还原用已选工件，`--offline` 仅使用已有的已验证缓存。

```sh
ku i pypi:pillow==11.3.0 --module PIL.Image
```

对于返回 Pillow 对象等需要转换的函数，作者可以提供简单的 `vendor/ku-python-pillow/src/bridge.py`。

```python
from PIL import Image

def Pixel() -> int:
    with Image.new("RGB", (2, 1), (17, 31, 47)) as image:
        return image.getpixel((1, 0))[1]
```

将该包的 `src/ku.python.json` 中 `local` 改为 `true`，保留锁定的 wheel 和目标记录。将已有的 Ku 模块内容替换为这个接口，也可以新增 `src/text.ku`。

```ku
fn Pixel(): int! {
    return __ku_interop_int("self", "Pixel")
}
```

`ku i` 更新项目锁并列出适配器的真实导入。若文件名为 `text.ku`，调用方就使用 `import { Pixel } from "@pillow/text"`，运行结果为 `31`。换到另一 Python ABI 时，用原来的 PyPI 安装命令和该目标的解释器增加目标；已有版本保留。如果该版本的匹配 wheel 缺失，安装失败并说明目标。

## 交付应用

复制可执行文件和同目录的 `ku-runtime.d`。默认 Python 构建要求部署端有与构建相同的 CPython 安装，旁置目录包含已选 wheel 和库桥。Ku 原生和 Runner 产物都可以使用旁置目录，解释执行则还需要 Ku CLI。桥接不保留用户传入的字符串引用；异步任务和 HTTP 处理器中的阻塞桥接调用在检查阶段拒绝。

macOS ARM64 和 Linux GNU 的可再分发布局可以将 libpython 和标准库随应用一起交付。开发者选择自己有权再分发的独立 CPython，提供这份发行物的许可证原文目录。下面是 macOS ARM64 的固定输入示例，使用 Python Build Standalone 的同一版本运行时和许可证。 [Python Build Standalone 许可说明](https://gregoryszorc.com/docs/python-build-standalone/main/running.html#licensing)

```sh
python_dist="$PWD/cpython312"
mkdir -p "$python_dist/notices"
release=https://github.com/astral-sh/python-build-standalone/releases/download/20261110
archive=cpython-3.12.16%2B20261110-aarch64-apple-darwin
curl -fsSL "$release/$archive-install_only.tar.gz" -o "$python_dist/runtime.tar.gz"
curl -fsSL "$release/$archive-pgo%2Blto-full.tar.zst" -o "$python_dist/full.tar.zst"
printf '%s  %s\n' e956e60bc9465d577ef8062059aa05cd3a2665b4b8678a2ed24e432368330b8f "$python_dist/runtime.tar.gz" | shasum -a 256 -c -
printf '%s  %s\n' 4da072af9dbbc2557d3aabf51db553a19694f6ac61b5b7169749e4acfa5a052d "$python_dist/full.tar.zst" | shasum -a 256 -c -
tar -xzf "$python_dist/runtime.tar.gz" -C "$python_dist"
bsdtar -xf "$python_dist/full.tar.zst" -C "$python_dist/notices" python/licenses
export KU_PYTHON="$python_dist/python/bin/python3.12"
export KU_PYTHON_REDISTRIBUTION_NOTICES="$python_dist/notices/python/licenses"
ku i
ku build --release src/main.ku -o clean
```

随应用目录保留 `python/lib/python3.12/LICENSE.txt`、`python/notices` 和 wheel 中的许可证。使用 CPython 3.12 的 PyPI 包时，包的锁定目标也须覆盖 CPython 3.12。Windows 使用匹配的外部 CPython 运行时；MSVC 与 GNU 构建分别验证，不交换 C++ 工具链产物。
