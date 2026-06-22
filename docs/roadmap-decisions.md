# Ku 待决策问题

这份文档只保留仍需要决定的设计。已经确定并完成的内容写入 `docs/syntax.md`、`docs/package.md`、`docs/ir.md` 和版本记录，不再在这里重复堆积。

当前已完成但不再需要决定的主线：

- native C：默认 move、显式 `clone()`、自动 drop；array/struct/enum/Result 已有 copy/move/drop 路径和回归测试。
- native C：统一 `KuError { domain, code, message }`，支持 `try/catch/finally`、return-through-finally、跨 payload 错误传播和复杂 Result payload。
- registry：HTTPS-only 请求、SHA-256 流式校验、有界重试/超时/大小、唯一临时目录、内容寻址 cache、安装锁和 GC 隔离已实现。
- async：取消、超时、状态、runtime snapshot、blocking shutdown drain 和百万并发需求压力测试已完成；native C/LLVM 继续明确拒绝 async。
- LLVM：继续保持清晰的小子集，只按真实项目需求扩展。

## 决策 1：native 闭包 ABI 与捕获语义

完整 native closure 需要一次把下面规则定齐，否则很容易出现解释器和 native 语义不一致。

推荐整组选项：

1. 分阶段交付：先做无捕获同步函数值和精确间接调用，再做 Copy 捕获，最后做 Owned 捕获。
2. 保持现有共享绑定捕获：closure 读取外层最新值；允许写回的前提仍是外层 binding 可变。
3. 函数值属于 Owned：默认 move、显式 `clone()`、作用域结束 drop；调用函数值本身不消耗它。
4. 只允许 closure env / captured binding cell 使用局部非原子引用计数，不把全局值模型改成 RC。
5. 捕获动作本身不 move；共享槽中的 Owned 值不能直接 move-out，传参或返回时必须 `.clone()`；重赋值先 drop 旧值。
6. 增加结构化函数类型语法：`fn(int, str): bool`。类型身份只包含参数类型、返回类型和 async 标志，不包含参数名或函数体。
7. 0.x closure ABI 只保证编译器内部使用：`{ typed invoke pointer, env pointer }`，隐藏 env 作为首参；暂不承诺外部 C callback ABI。
8. 第一阶段支持同步 closure 逃逸和局部函数自递归；互递归 closure graph、循环 env 和 async closure 暂时明确拒绝，不引入 tracing GC。
9. native closure 默认线程封闭，env 引用计数非原子；native async 仍拒绝。
10. native 直接/间接递归是否增加 Ku 自己的调用深度守卫：推荐增加，避免只依赖 C 栈崩溃。

我选择：

## 决策 2：registry 签名、信任根与包归档

HTTPS、SHA-256 和 cache 执行层已经完成，但生产 CLI 必须 fail-closed；签名算法、信任根和解包格式没定之前，不能把下载到的归档直接当成可导入 package。

推荐整组选项：

1. registry index 使用 Ed25519 detached signature。
2. 签名输入是规范化后的原始 index 字节，不对解析后的对象重新序列化。
3. 官方 registry 根公钥随 Ku 工具链内置；自定义 registry 的公钥由用户配置显式提供，不允许静默信任首次连接。
4. key rotation 使用“旧 key 签新 key”的过渡记录；撤销列表由仍受信任的 key 签名，并设置单调版本号，防止回滚。
5. package 归档第一版统一使用 `.tar.zst`；解包时拒绝绝对路径、`..`、设备文件、硬链接和逃逸 symlink，并继续执行文件数、总字节数和单文件大小上限。
6. 归档根必须只有一个 package 目录，且必须包含 `ku.mod`；manifest 的 name/version 必须与 index/lockfile 一致。
7. cache 以 `name + exact version + SHA-256` 内容寻址；已验证目录不可覆盖，只能新增或 GC。
8. 默认 registry 地址由工具链内置，同时允许 `KU_REGISTRY` 和用户配置覆盖；lockfile 永远记录最终 resolved URL 和 SHA-256。
9. 第一版允许 registry 标记 yanked，但已有 lockfile 仍可重现安装；新解析不再选择 yanked 版本。
10. 发布者签名放到第二阶段，不阻塞第一版 registry index 签名。

我选择：

## 决策 3：native `str` 与动态 `object` 的正式内存 ABI

语言层已经把 `str` 和 `object` 定为 Owned，但当前 native C 的 `str` 仍是只读 `const char*`，动态 object 也还没有 native hash map。要完成全类型所有权，必须固定正式 ABI。

推荐整组选项：

1. `str` 使用 UTF-8 `KuString { ptr, len, capacity, storage }`，不把 NUL 结尾当成长度来源。
2. `storage` 区分 static/owned；字符串字面量零分配、drop no-op，运行时拼接结果持有 heap allocation。
3. `clone()` 深拷贝 owned string；move 清空源；drop 只释放 owned storage。
4. 与 C API 交互时提供临时 NUL 结尾 view/copy，不把内部字符串 ABI退化为裸 C 字符串。
5. 动态 `object` 第一版使用开放寻址 hash table，key 为 KuString，value 为 tagged KuValue。
6. object 默认 move、显式深 clone、自动 drop；缺键严格报错，只有 `object[key]?` 返回 `null`。
7. 第一版禁止 object 自引用和循环图，不引入 tracing GC。
8. OOM、array 越界和内部 invariant 失败默认终止当前进程；普通缺键、解析错误和 I/O 错误继续走 Result。是否接受这个边界？

我选择：

## 决策 4：下一阶段优先级

当前建议顺序：

1. 根据决策 1 完成 typed callable IR、无捕获 native closure、捕获 env 和闭包所有权。
2. 根据决策 2 完成 registry 签名验证、受限解包、CLI resolver/download/import 全链路。
3. 根据决策 3 替换 native `const char*` 原型 ABI并实现动态 object。
4. 用两个真实 Ku 项目做 native C/LLVM 编译验证；LLVM 只补项目实际需要的 array/enum。
5. 同步 native ABI 稳定后，再单独设计状态机式 native async runtime；不使用 OS 线程冒充小协程。

我选择：
