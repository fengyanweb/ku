# Declaration inventory

This tool reads `source.ku`, selects top-level functions whose names start with
the prefix in `prefix.txt`, and prints their locations in source order. It uses
the Ku bootstrap parser and reports its positioned errors for unsupported input.

Copy the bootstrap sources to a separate working directory and put the input
files beside the tool's `main.ku`. Build with the installed compiler.

```sh
cp -R /path/to/ku/bootstrap ./bootstrap
cd bootstrap/tools/declarations
printf 'fn Keep(): int { return 42 }\n' > source.ku
printf 'Keep\n' > prefix.txt
ku build --release main.ku -o declarations
./declarations
```

Expected output is `1:1 Keep`. The capturing callback returns a line number for
a matching function and `null` for other declarations. An empty prefix returns
`inventory/empty_prefix`.

For a native deployment, copy `declarations`, `source.ku` and `prefix.txt`
together. An explicit Runner build still needs its original module sources.
