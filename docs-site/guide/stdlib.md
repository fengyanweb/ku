# 标准库

`fs` 和 `http` 需要显式导入：

```ku
import "std.fs"
import "std.http"
```

```ku
fs.write("out.txt", "hello")
res = http.get("https://example.com")?
print(res.status)
print(res.body)
```
