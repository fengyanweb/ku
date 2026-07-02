# Native C

`ku build --native` 当前仍是 prototype C 后端，但已经支持非递归 `struct`、带长度 `array` 与越界检查、`enum tag/payload`、match lowering、统一 Error ABI、复杂 Result payload、`?` 传播以及 `try/catch/finally` 的普通完成/错误/return 路由。

仍明确拒绝的部分包括 native closure、动态 object ABI、正式 owned string ABI、递归值布局和 native async lowering。遇到这些能力时后端必须给出清晰错误，不允许静默生成错误 C。

后续 native ABI 和优化顺序已经固定：先做无捕获同步函数值和间接调用，再推进 `KuString`、dynamic object、registry fail-closed、最终不依赖 Ku 源码文件的 native binary。IR 优化至少覆盖常量折叠、死代码删除、简单内联、临时变量消除、drop/clone 消除、escape analysis、stack allocation、monomorphization 和 bounds check 优化。
