# Native C

`ku build --native` 当前仍是 prototype C 后端，但已经支持非递归 `struct`、带长度 `array` 与越界检查、`enum tag/payload`、match lowering、统一 Error ABI、复杂 Result payload、`?` 传播以及 `try/catch/finally` 的普通完成/错误/return 路由。

仍明确拒绝的部分包括 native closure、动态 object ABI、正式 owned string ABI、递归值布局和 native async lowering。遇到这些能力时后端必须给出清晰错误，不允许静默生成错误 C。
