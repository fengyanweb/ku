# Native packages from SQLite and TinyXML-2

This example uses a Ku build that includes `ku i` and native package interop. You need Python 3 for the example downloader and Clang with C and C++ development libraries. The downloader checks the exact SQLite 3.50.4 archive and TinyXML-2 11.0.0 source bytes before writing the example. The upstream licenses travel with the corresponding package.

From this Ku source checkout, create a fresh example directory.

```sh
python3 examples/interop-native/prepare.py native-demo
cd native-demo
ku i ./vendor/sqlite_example
ku i ./vendor/tinyxml_example
```

PowerShell accepts the same relative paths. Invoke the script with `python` or `py -3` if your Python uses that name. For a specific Windows ABI, set `KU_INTEROP_CC` and `KU_INTEROP_CXX` to the absolute paths returned by `Get-Command` for your selected LLVM C and C++ drivers. They each name one executable. Check that `-dumpmachine` reports the intended Windows MSVC or GNU target before building the package.

The installer prints a separate import for each callable export. The example
groups imports from the same module in `src/main.ku`.

```ku
import { Query, Version } from "@sqlite_example/database"
import { Close, New, Parse, RaiseExampleError, RootName } from "@tinyxml_example/document"
```

Run the consumer and produce either executable directory.

```sh
ku run
ku build --release -o native-demo
./native-demo
ku build --backend runner --release -o runner-demo
./runner-demo
```

On Windows, use `-o native-demo.exe` and `-o runner-demo.exe` and run `.\native-demo.exe` and `.\runner-demo.exe`. Move the executable together with the adjacent `ku-runtime.d` directory. Its subdirectories contain the compiled companion libraries and their notices. Windows GNU builds link the compiler runtimes statically. Keep the toolchain notices in the delivery directory. If another compiler uses an external C++ runtime, keep its redistributable runtime available on the target as specified by that compiler.

Expected output

```text
3050004
call_error
你好，Ku
SQLite after error
0
example native exception
root
root
call_error
```

## The author side

The generated `vendor/sqlite_example/src/ku.interop` selects the real `sqlite3_libversion_number` declaration for direct binding. The author-owned `sqlite_adapter.c` handles SQLite's pointer lifetimes. It opens only an in-memory database, authorizes only a SELECT expression, binds the input as a parameter, copies the resulting text, finalizes the statement and closes the database. `return.Query = "owned_cstr:example_release"` directs the bridge to use the author's release routine once the host has copied the result. The first Ku string remains valid after the statement closes and after a second query.

`vendor/tinyxml_example/src/ku.interop` selects a TinyXML-2 document class and exact member names. The test subclass only adds a member that throws. The generated bridge catches C++ exceptions, checks the document handle on each call and copies the element name while the document owns the element. A closed handle yields a recoverable error. Close live handles as soon as you no longer need them. The companion releases remaining owners at normal process exit.

For a new flat C SDK with one header and one C file, run `ku i ./vendor/your-sdk`. Ku writes a package under the project's `ku-libraries/` directory with the discovered scalar exports. Review the generated `ku.mod`, `src/ku.interop` and Ku wrapper module. Commit them and the companion sources to share the package. Existing wrapper files are safe to edit, and an ordinary consumer can install the resulting Ku package from its path or from a pinned public Git repository.

The native descriptor accepts `source.N` and `include.N` paths underneath the package's import root, with one `header`. `export.PublicName` selects a C symbol from that header. `borrow.PublicName = "cstr"` declares call-scoped, NUL-terminated read-only text arguments. An embedded NUL returns an error in this form. For a read-only C buffer drawn from a Ku string, `buffer.PublicName = "0:1"` identifies the pointer and byte count at argument positions 0 and 1. The bridge validates the count, passes the UTF-8 bytes by reference and preserves embedded NUL bytes. A length of zero passes a null pointer. Use `value.byte_len()` for the count. Arbitrary non-UTF-8 buffers, general callbacks, cross-Task borrowed objects, unconstrained C++ overloads and by-value foreign structs need an explicit scalar or text adapter for this ABI.

For an explicitly installed private static SDK, `archive.N` selects a regular self-contained `.a` or `.lib` under the import root and `target` records the exact string from the selected compiler's `-dumpmachine`. Use an ABI-matched archive and retain its license and dependencies. A native compiler, rather than a path alias or the archive extension, validates that the member objects are suitable for linking. Publish only redistributable source and declared metadata through the Ku registry.
