---
title: TCP 与 TLS 客户端
description: 有界 native TCP 客户端配置、二进制读写、TLS 与生命周期。
kuVersion: '0.0.19'
verifiedAt: '2026-10-21'
sources:
  ['src/stdlib/metadata.rs', 'src/backend/c.rs', 'docs/protocol-foundation.md']
sidebar:
  order: 11
---

`std.net` 连接 TCP 服务端，读写[二进制数据](/zh-cn/docs/stdlib/bytes/)。这是 native C 接口，不提供 TCP 服务端，也不是 HTTP 客户端或解释器网络接口。

## 导入与方法

```ku
import net from "std.net"
```

| 入口                         | 返回             | 含义                                       |
| ---------------------------- | ---------------- | ------------------------------------------ |
| `net.client(config)`         | client 的 Result | 按下表配置建连。                           |
| `client.write(data)`         | `null!`          | 写完全部 bytes 或返回错误，不消费原 data。 |
| `client.read(max_bytes:int)` | bytes 的 Result  | 读取**最多**指定字节数，可能少于该数量。   |
| `client.close()`             | `null`           | 消费并关闭 client。                        |

一次读取不代表完整消息，也不是 read-exact。应用协议需要自行定义分帧，并限制累计读取循环。EOF 返回错误，不当作成功的空消息。read/write 借用 client；`close` 消费后再次使用会报编译错误。拥有 client 的作用域结束也会释放连接。

## 配置

| 字段                     | 默认值 / 范围                   | 作用                               |
| ------------------------ | ------------------------------- | ---------------------------------- |
| `host:str`               | 必填；可见 ASCII，最多 253 字节 | 域名或 IP；国际域名先转 punycode。 |
| `port:int`               | 必填，`1..65535`                | 远端 TCP 端口。                    |
| `connect_timeout_ms:int` | 5000；`1..300000`               | 建连及 TLS 握手共用预算。          |
| `read_timeout_ms:int`    | 5000；`1..300000`               | 每次读取预算。                     |
| `write_timeout_ms:int`   | 5000；`1..300000`               | 每次写入预算。                     |
| `max_read_bytes:int`     | 1 MiB；`1..16 MiB`              | 单次读取请求上限。                 |
| `tls:bool`               | false                           | 使用同一 API 启用 TLS。            |
| `tls_server_name:str`    | TLS 开启时默认为 host           | 证书主机名。                       |
| `tls_ca_pem:str`         | 不填使用内置 WebPKI 根          | 显式 PEM 根会**替换**内置根。      |

未知或非法字段返回 `net/invalid_config`。TLS 专属字段要求 `tls: true`。TLS 构建需要匹配的 native target pack；证书和主机名验证不能关闭，不读取系统信任根。TLS 单次读取另受 64 KiB runtime 上限约束。native HTTP、Redis 尚未复用这条 TLS transport。

## 本机回显示例

本例依赖本地服务。将以下代码保存为 `echo.py`，在另一个终端执行 `python3 echo.py`。它只接受一个连接后退出，等待和 I/O 均设置了期限。

```python
import socket

with socket.socket() as server:
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", 8765))
    server.listen(1)
    server.settimeout(30)
    connection, _ = server.accept()
    with connection:
        connection.settimeout(5)
        received = b""
        while len(received) < 3:
            part = connection.recv(3 - len(received))
            if not part:
                raise RuntimeError("client closed before three bytes")
            received += part
        if received != bytes([65, 0, 255]):
            raise RuntimeError("unexpected payload")
        connection.sendall(received)
```

将 Ku 程序保存为 `tcp.ku`，使用 `ku build --backend c -o tcp tcp.ku` 构建，在回显服务等待时运行 `./tcp`：

```ku
import bytes from "std.bytes"
import net from "std.net"

fn main(): null! {
    payload = bytes.from_array([65, 0, 255])?
    client = net.client({
        host: "127.0.0.1",
        port: 8765,
        connect_timeout_ms: 1000,
        read_timeout_ms: 1000,
        write_timeout_ms: 1000,
        max_read_bytes: 1024
    })?
    client.write(payload)?
    println(payload.get(2)?)
    reply = client.read(1)?
    println(reply.get(0)?)
    client.close()
    return ok(null)
}
```

```text
255
65
```

## 错误与边界

错误 domain 为 `net`，包括 `connect_error`、`connect_timeout`、`read_timeout`、`write_timeout`、`end_of_stream`、`transport_error`、`client_closed` 和 TLS 错误。传输错误、EOF 或开始 I/O 后发生的超时会使 stream 不可继续使用，不要在同一连接继续拼接应用消息。尚未触碰 stream、仅等待内部操作 gate 时超时，不一定使连接失效。

native transport 路径使用 deadline，但同步 DNS 只能在解析返回后复核；不承诺所有系统调用均能在期限到达时被中断。HTTP 业务请使用 [HTTP 库](/zh-cn/docs/stdlib/http/)，不要向 HTTP 端口发送任意二进制数据。
