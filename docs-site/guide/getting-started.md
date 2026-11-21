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

## 读取用户输入

从终端读一个值时，直接写提示：

```ku
fn main(): null! {
    name = input("请输入名字：")?
    println(name)
    return ok(null)
}
```

要在一句话中填写多个值，用反引号模板标记位置：

```ku
fn main(): null! {
    name, age = input(`我的名字是${}，今年${:int}岁。`)?
    println(name)
    println(age)
    return ok(null)
}
```

每个空位用回车提交。文本格 `${}` 返回字符串，整数格 `${:int}` 返回 `int`。`?` 传播输入结束或读取失败等错误。
