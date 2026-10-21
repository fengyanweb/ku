---
title: Files and directories
description: Read and write UTF-8 files, check paths, and list directories with std.fs.
kuVersion: '0.0.18'
verifiedAt: '2026-10-21'
sources: ['src/stdlib/fs.rs', 'src/stdlib/metadata.rs', 'docs/syntax.md']
sidebar:
  order: 5
---

Import with `import fs from "std.fs"`. Relative paths resolve from the directory of the current `.ku` source file, rather than the shell's working directory. Absolute paths remain absolute.

## Write, read, and list

Save as `files.ku` in a writable directory and run `ku run files.ku`. It creates or **overwrites** `message.txt` beside the source; remove that file when finished. The directory listing also includes the source and any other files already present.

```ku
import fs from "std.fs"

fn main(): null! {
    fs.write("message.txt", "你好 Ku\n")?
    text = fs.read("message.txt")?
    println(text)
    println(fs.exists("message.txt"))

    paths = fs.read_dir(".")?
    for path in paths { println(path) }

    try {
        fs.read("missing.txt")?
    } catch (err) {
        println(err.domain)
        println(err.code)
    }
    return ok(null)
}
```

The program prints `你好 Ku`, `true`, the sorted entry paths, then `fs` and `read_failed` for the deliberately missing file. The extra line after the greeting comes from its newline plus `println`.

## Interface

`T!` returns a Result. Inputs are read-only; a read produces a new owned string and a directory listing produces a new owned array.

| Call                               | Return   | Behavior                                                                                |
| ---------------------------------- | -------- | --------------------------------------------------------------------------------------- |
| `fs.read(path:str)`                | `str!`   | Read a UTF-8 file                                                                       |
| `fs.try_read(path:str)`            | `str!`   | Compatibility alias for `read`                                                          |
| `fs.write(path:str, text:str)`     | `null!`  | Create or truncate a file and write UTF-8 text                                          |
| `fs.try_write(path:str, text:str)` | `null!`  | Compatibility alias for `write`                                                         |
| `fs.exists(path:str)`              | bool     | True when filesystem metadata can be read for a file/directory or a valid followed link |
| `fs.read_dir(path:str)`            | `[str]!` | List one directory level, returning sorted entry paths                                  |

`write` does not create missing parent directories, append, or promise an atomic replacement. It can leave partial external effects if execution is interrupted. `read_dir` is not recursive and includes files and directories. Returned values are paths formed from the resolved directory and the entry name, not just basenames; no canonicalization or symlink containment is implied.

`exists` returns false for a missing path, invalid/overlong path, or metadata/permission failure. False does not uniquely mean the file does not exist, and a true result does not guarantee a later open will succeed.

## Limits and errors

| Operation                     | Limit                                                             |
| ----------------------------- | ----------------------------------------------------------------- |
| `read` / `try_read`           | 1,000,000 UTF-8 bytes                                             |
| `write` / `try_write`         | 1,000,000 UTF-8 bytes of text                                     |
| `read_dir`                    | 10,000 entries and 1,000,000 bytes of returned path text in total |
| Directory and existence paths | 32,768 bytes, including the resolved directory prefix             |

Filesystem errors use domain `fs`. Typical codes are `read_failed`, `file_too_large`, `write_failed`, `content_too_large`, and `read_dir_failed`; allocation/resource failures can add path-specific errors. Invalid UTF-8 input is an error, not a lossy text conversion. `try_read` and `try_write` do not change the return type or turn failures into defaults.

Handle errors with `?` or the `try/catch` shown above. There is no public binary-file, remove, rename, mkdir, or atomic-write operation in this module. Binary buffers are covered by [the standard-library index](/en/docs/stdlib/).

## Execution support

The six calls are implemented for the interpreter, default packaged runner, and synchronous native C. The restricted native Task path also supports these file/directory operations through its bounded background pool; the call itself can suspend without an additional `await`. A running OS operation is not forcibly rolled back by cancellation, and the native Task path rejects submissions from `finally`. LLVM does not implement this complete module.
