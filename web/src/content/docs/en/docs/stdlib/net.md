---
title: TCP and TLS clients
description: Bounded native TCP client configuration, binary I/O, TLS, and lifecycle.
kuVersion: '0.0.18'
verifiedAt: '2026-10-21'
sources:
  ['src/stdlib/metadata.rs', 'src/backend/c.rs', 'docs/protocol-foundation.md']
sidebar:
  order: 11
---

`std.net` opens a TCP client and exchanges [binary values](/en/docs/stdlib/bytes/). It is a native C API, not a TCP server, HTTP client, or interpreter networking API.

## Import and methods

```ku
import net from "std.net"
```

| Entry                        | Returns       | Meaning                                                     |
| ---------------------------- | ------------- | ----------------------------------------------------------- |
| `net.client(config)`         | client Result | Connect using the configuration below.                      |
| `client.write(data)`         | `null!`       | Write all bytes or return an error; does not consume data.  |
| `client.read(max_bytes:int)` | bytes Result  | Receive **up to** the requested size, possibly fewer bytes. |
| `client.close()`             | `null`        | Consume and close the client.                               |

A read is not a message or read-exact operation. Define framing in your protocol and bound any loop collecting a response. Empty EOF is an error rather than a successful empty message. Reads and writes borrow the client; after consuming `close`, using it again is a compile error. Scope exit also releases an owned client.

## Configuration

| Field                    | Default / range                          | Purpose                                                          |
| ------------------------ | ---------------------------------------- | ---------------------------------------------------------------- |
| `host:str`               | Required; visible ASCII, up to 253 bytes | Host name or IP; encode international domains as punycode first. |
| `port:int`               | Required, `1..65535`                     | Remote TCP port.                                                 |
| `connect_timeout_ms:int` | 5000; `1..300000`                        | Shared connection and TLS handshake budget.                      |
| `read_timeout_ms:int`    | 5000; `1..300000`                        | Per-read budget.                                                 |
| `write_timeout_ms:int`   | 5000; `1..300000`                        | Per-write budget.                                                |
| `max_read_bytes:int`     | 1 MiB; `1..16 MiB`                       | Maximum requested bytes in one read.                             |
| `tls:bool`               | false                                    | Enable TLS on the same client API.                               |
| `tls_server_name:str`    | host when TLS is enabled                 | Certificate host name.                                           |
| `tls_ca_pem:str`         | Built-in WebPKI roots when omitted       | Explicit PEM roots **replace** the built-in roots.               |

Unknown or invalid fields return `net/invalid_config`. TLS-only fields require `tls: true`. TLS requires a matching native target pack; certificate and host-name validation cannot be disabled. It does not read the operating system trust store. TLS reads have an additional 64 KiB runtime limit. This TLS transport is not yet used by native HTTP or Redis.

## Local echo example

This example requires a local peer. Save the following as `echo.py` and run `python3 echo.py` in a separate terminal. It accepts one connection and exits; accept and I/O waits are bounded.

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

Save the Ku program as `tcp.ku`, build with `ku build --backend c -o tcp tcp.ku`, and run `./tcp` while the peer is waiting:

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

## Errors and limits

Errors use domain `net`, including `connect_error`, `connect_timeout`, `read_timeout`, `write_timeout`, `end_of_stream`, `transport_error`, `client_closed`, and TLS errors. Transport failure, EOF, or a timeout after I/O starts makes the stream unusable; do not continue an application message on that stream. Waiting for an operation's gate without touching the stream may time out without invalidating it.

Deadlines bound the native transport path, but synchronous DNS resolution can only be checked after the resolver returns. This is not a guarantee that every operating system call can be interrupted at the deadline. For HTTP, use the [HTTP library](/en/docs/stdlib/http/) rather than sending arbitrary bytes to an HTTP port.
