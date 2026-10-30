# 具名结构体的 JSON 编码

在 native C 后端中，现有 `json.encode(value)` 可以直接接收具名结构体，无需新增接口。
本页描述 native C 的用法，不将此能力推广到其他执行后端或任意 Task 值传递。

## 使用方式

导入 `json` 后定义结构体。以下声明摘自完整示例：

```ku
struct Envelope {
    id: str
    counter: int
    data: KuValue
}
```

完整程序见 [json_typed.ku](../examples/json_typed.ku)，包含中文、emoji、嵌套数组和往返解码。
其中完成解码并取得 `counter`、`data` 后，使用以下调用：

```ku
    envelope = Envelope { id: "example", counter: counter + 1, data: data }
    text = json.encode(envelope)?
    println(text)
    // Encoding borrows the envelope; it can be encoded again.
    if (json.encode(envelope)? != text) { panic("typed JSON changed its input") }
```

## 字段与输出

这里的基本字段为 `str`、`int` 和动态 `KuValue`。嵌套具名结构体也可编码；已有用法包含字符串、整数数组和结构体数组。
编译器按结构体的静态字段布局写出 JSON；`KuValue` 子树仍由普通 JSON 实现完整编码，保留其中的对象、数组、布尔值和空值。
动态子树继续遵循普通 JSON 的值类型、编码错误和嵌套限制。
成功返回的 `str` 由调用方拥有，可交给 `json.decode()`；字符串按 Ku 正常生命周期释放。
返回方式仍为 Result，沿用 `?` 传播或 `try/catch` 处理错误，不会自动解包。

## 读取与所有权

编码只读取输入，不消费结构体或其中的动态子树；同一值可以再次编码。
只为调用或重复调用 `encode`，不需要 `.clone()`。构造结构体、赋值等操作仍遵循原有所有权规则。
示例中的 `decoded.take("data")` 是编码前的显式取值与所有权移交，`encode` 本身不执行 `take`。
直接写出静态字段仍需要输出缓冲与字符串存储，不代表零分配；具体性能取决于数据和调用路径。
