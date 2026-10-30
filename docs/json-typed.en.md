# Encoding named structs as JSON

With the native C backend, the existing `json.encode(value)` accepts named structs directly. No additional API is required.
This page describes native C usage; it does not extend that support to other execution backends or arbitrary Task value passing.

## Usage

Import `json` and define a struct. This declaration comes from the complete example:

```ku
struct Envelope {
    id: str
    counter: int
    data: KuValue
}
```

See [json_typed.ku](../examples/json_typed.ku) for the complete program, including Chinese text, emoji, nested arrays, and decoding the result.
After decoding the input and obtaining `counter` and `data`, the example uses:

```ku
    envelope = Envelope { id: "example", counter: counter + 1, data: data }
    text = json.encode(envelope)?
    println(text)
    // Encoding borrows the envelope; it can be encoded again.
    if (json.encode(envelope)? != text) { panic("typed JSON changed its input") }
```

## Fields and output

The basic fields shown here are `str`, `int`, and dynamic `KuValue`. Nested named structs can also be encoded; existing usage includes strings, integer arrays, and arrays of structs.
The compiler writes the struct's static field layout directly. The ordinary JSON implementation still encodes the complete `KuValue` subtree, preserving its objects, arrays, booleans, and null values.
Dynamic subtrees retain the ordinary JSON value-type rules, encoding errors, and nesting limits.
On success, the caller owns the returned `str` and can pass it to `json.decode()`. Ku releases the string through its normal lifetime rules.
The return convention remains Result: propagate errors with `?` or handle them with `try/catch`. There is no automatic unwrapping.

## Reading and ownership

Encoding only reads its input. It does not consume the struct or its dynamic subtree, so the same value can be encoded again.
Calling or repeating `encode` does not require `.clone()`. Struct construction, assignment, and other operations retain their existing ownership rules.
The example's `decoded.take("data")` explicitly extracts data and transfers ownership before encoding. `encode` itself does not perform `take`.
Writing static fields directly still requires an output buffer and string storage. It is not a zero-allocation guarantee; performance depends on the data and calling path.
