# Declaration inventory

This tool reads `source.ku`, selects top-level functions whose names start with
the prefix in `prefix.txt`, and prints their locations in source order. It uses
the Ku bootstrap parser and reports its positioned errors for unsupported input.

From a separate working directory, create the two input files and build the tool
with the installed compiler.

```sh
printf 'fn Keep(): int { return 42 }\n' > source.ku
printf 'Keep\n' > prefix.txt
ku build --release /path/to/ku/bootstrap/tools/declarations/main.ku -o declarations
./declarations
```

Expected output is `1:1 Keep`. An empty prefix returns `inventory/empty_prefix`
when the first declaration is visited.
