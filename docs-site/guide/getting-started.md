# 快速开始

```powershell
cargo build --release
.\target\release\ku.exe -h
.\target\release\ku.exe run .\examples\hello.ku
.\target\release\ku.exe check .\examples\index.ku
```

Ku 不使用 `let` / `let mut`。首次赋值即声明变量：

```ku
name = "Ku"
age:int = 1
```
