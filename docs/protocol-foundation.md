# Ku 第一层协议地基状态

本文记录可由当前源码和自动化测试证明的能力。状态只按生产可用性标记，避免把“能跑一次”写成“协议已完整实现”。

## 当前状态

| 能力 | 当前状态 | 已有保证 | 明确边界 |
| --- | --- | --- | --- |
| HTTP/1.1 server | 三平台 CI 已验证 | 解释器与 native 均有限额、总读写 deadline、并发背压；严格拒绝歧义 Content-Length、Transfer-Encoding、非法 CRLF/Host/version/target；路由不会截断；native 在循环回边和 Ku 调用返回点检查 handler deadline，超时展开给 `finally` 一秒清理窗口，之后的 safepoint 可终止不退出的 Ku 循环；Windows 使用 Winsock worker/queue，Linux/macOS 使用 POSIX socket/poll/pthread，并显式抑制 SIGPIPE；三系统完整 workspace 与 target-native fixture 已跑绿 | 每连接关闭；暂不接收 chunked；native 超时是协作式而非线程抢占，不能中断阻塞中的系统/FFI 调用；native 仅 IPv4，且只支持直接 `listen`；CI 通过不等于生产负载 soak |
| TLS client | native `std.net` 已接入，v0.0.17 三系统消费者已验证 | 解释器的 `http.get/post/request` 和 registry HTTPS 使用 rustls；`ku-registry` 有专用 rustls TLS listener；libpq 可使用 PostgreSQL TLS。socket-free `ku-native-tls` ABI 已由 generated C 的同一 `net.client(config)` 驱动，支持 TLS 1.2/1.3、WebPKI 根或完全替换它的显式 PEM CA；证书与主机名验证不可关闭，错配或 runtime 不可用时 fail closed，不明文回退；握手、record staging、I/O 和 no-progress 都有显式上限 | v0.0.17 的精确 pack 证据绑定 c668283，不能外推未来 ABI/build-id；`ku run` stream parity、native HTTP/Redis TLS 仍未完成；不读系统根，尚无 TLS server/mTLS，不能称为通用 native HTTPS |
| PostgreSQL | 统一 client 实验实现 | 只公开 `pg.client(config)` 与 receiver API；内部有界池、服务端参数绑定、单行增量聚合、NULL/空串区分、严格 UTF-8/结果上限；发送后模糊失败为 `execution_unknown`，成功终态后结果失败为 `execution_completed_without_result`；明确 session-control 与执行后非 IDLE 返回 `session_state_unsupported`，后置固定消息明确不可重试，正常归还前在原预算内 `DISCARD ALL`；conninfo 内部副本销毁前清零；三系统精确 libpq 链接/启动和 workspace CI 已通过 | Windows loopback PostgreSQL 新 API 已跑通，Linux/macOS 实库查询仍待验；SQL 文本预检不能证明任意过程/函数没有状态或外部副作用；同步 DNS/libpq/SSL 内部调用不能硬抢占；不支持 transaction/COPY/exclusive；每次安全 reset 多一次往返；结果 cap 不是进程峰值硬上限；自动静态链接 libpq 不支持 |
| owned bytes / TCP | native 三平台基础实现 | `bytes.from_str/from_array` 是唯一构造入口，`len/get/to_str` 是唯一读取入口；深 clone、move/drop、array/Result/closure 路径使用同一 ABI；`net.client(config)` 只公开 `read/write/close`，默认明文，同一入口可写 `tls: true`；单连接 gate、connect/read/write 绝对 deadline、单次读取与 16 MiB 配置硬上限均有生成 C 合同测试，明文 loopback 已在三系统 workspace CI 跑绿 | 当前仅 native C，`ku run` 尚不支持；`read(n)` 是一次最多 n bytes 的接收，不是 read-exact/message API；TLS 单次读取还会收窄到 64 KiB runtime 上限；同步 `getaddrinfo` 不能硬取消；CI 通过不等于生产弱网 soak |
| Redis RESP2 | 统一 client 实验实现 | 只公开 `redis.client(config)` 与 `ping/get/set/exists/del/close`；内部有界懒连接池、逐连接自动 AUTH、三层 deadline、严格 RESP/UTF-8；GET nil 唯一映射为 `redis/key_not_found`；AUTH/transport/OOM/timeout 分类不折叠；服务端文本不进入诊断；坏连接只淘汰单槽；凭据副本销毁前清零；fake RESP、并发和错误恢复已在三系统 workspace CI 跑绿 | 新 client 仍需真实 Redis 复验；同步 DNS 不能硬取消；尚未把 RESP payload 切换到公共 `bytes/std.net` transport；未支持 RESP array/RESP3、TLS、cluster/sentinel 或通用 command API |
| MySQL | 统一 client 实验实现 | `mysql.client(config)` 内部有界池；query/execute 全部使用 `MYSQL_STMT`，结果脱离连接后读取；有列结果在 `MYSQL_NO_DATA` 前的失败为 `execution_unknown`，确认终态后结果失败为 `execution_completed_without_result`；顶层 `CALL` 和明确 session-control 前置拒绝，执行后事务/autocommit 污染返回同一 `session_state_unsupported` 但后置固定消息明确不可重试；普通路径执行 statement cleanup 与 `mysql_reset_connection()`；从不启用或调用已弃用的自动 reconnect 选项，依赖所支持 Oracle/MariaDB client 的 `mysql_init` 默认关闭状态，LOCAL INFILE 显式关闭；编译期把精确 header 路径及 header family 绑定到已选动态库 family，library init 后再核对 runtime major；最终产物必须动态导入本次选择的 MySQL/MariaDB family；三系统精确 client library 链接/启动与 fake 专项已通过 | 仍需真实 MySQL/MariaDB 查询；SQL 文本预检不能证明任意过程/函数没有状态或外部副作用；同步 libmysqlclient/DNS 是软 deadline，不能硬抢占；未提供强制验证 TLS；参数仅 `[str]`，不支持 SQL NULL/typed bind；不支持 transaction/exclusive/CALL；产物不内嵌 client runtime，显式 non-host target 仍不自动链接 |
| Package registry v1 | 自托管参考实现 | 唯一 manifest 形式为项目 pin `registry.url`/`registry.public_key` 且 registry dependency 省略 `source`，跨包 import 只使用 `@name/path`；`ku-registry` 以真实 TLS listener 覆盖 pack/publish/resolve/check/run/native、locked/offline、exact-package ACL、幂等、冲突、并发竞争和重启恢复；Ed25519 index、SHA-256、受限 `.tar.zst`、portable registry lock、offline cache 校验、256 包图上限、20000 求解步骤、同一 cache 根跨进程 8 个全局下载槽和联网解析/重试/分块校验共用的 300 秒绝对预算；file override 以 `ku.mod`/`src` 快照 checksum 使用 immutable 内容寻址 root | 不是官方托管服务，也未做生产吞吐量或高并发基准；同步 DNS 与已进入内核的文件操作不能硬取消；Windows 目录 fsync/突然断电边界、服务资源上限和部署要求见 registry-api；artifact 必须是稳定、无 query 的 HTTPS `.tar.zst` URL；v1 没有自动 signed-roots、在线吊销或透明 key rotation |

## 生产部署底线

- PG 的单行接收不增加第二套流式 API：`result.rows()/cols()/value()/is_null()` 读取与连接脱钩的 owned 结果。SQL 已发送后、成功终态前的 timeout/断连/OOM 返回 `pg/execution_unknown`；成功终态后本地格式、UTF-8 或上限失败返回 `pg/execution_completed_without_result`。两者都禁止自动重试。libpq 仍会先缓冲单个完整行，所以结果 cap 不是进程峰值硬上限。

- 公网 HTTP 服务由 Caddy、Nginx 或 Envoy 终止 TLS，并让 Ku native server 显式监听回环地址。native 只实现 `service.listen(address)`；`bind` / `listener.run` / `listener.close` 当前是解释器能力，native 编译会提前给出明确错误。`listen` 消费 service 句柄，调用后不能继续注册路由或复用。
- PostgreSQL 远程连接把 `sslmode=verify-full` 和可信 `sslrootcert` 放在 `pg.client` 的 `conninfo` 中，并用 `connect_timeout_ms` 设置整个 host 列表共享预算。同步 DNS 仍可能越过预算后才返回；`sslmode=require` 只加密、不验证主机身份。
- 两个 SQL 驱动只提供参数化查询：PG 使用 `$1..$N`，MySQL 使用 `?`，无参数也显式传 `[]`。数据库侧 timeout 仍不可省略；HTTP handler deadline 不等于服务端取消/回滚。共享 client 会前置拒绝明确的 session-control，MySQL 还前置拒绝顶层 `CALL`，并在归还前分别使用 `DISCARD ALL` / `mysql_reset_connection()` 清理 session。执行后检测到明确事务/session 污染会丢弃成功 payload 并淘汰连接；detached payload 已完成后的 reset/cleanup 失败只淘汰连接，仍把该 payload 交给调用方。有限 SQL 预检、后置状态位与 reset 都不能证明任意过程、函数或表达式无全局/外部副作用，当前又没有 transaction/exclusive API，因此业务不能依赖跨查询 session 状态，也不能把成功 reset 当作撤销已执行 SQL。共享 client 不接受不受信任方提供的任意 SQL；生产数据库账号必须使用最小权限，禁止 PG superuser，并避免向 MySQL/MariaDB 业务账号授予 `SYSTEM_VARIABLES_ADMIN`、`RELOAD`、`FILE` 等管理权限。
- `execution_unknown` 和 `execution_completed_without_result` 都不是自动重试信号；`session_state_unsupported` 也不能仅凭 code 自动重试。该 session code 的前置固定消息表示 SQL 尚未发送，后置固定消息表示语句已经或可能已经执行且 payload 已丢弃。驱动不会重放 SQL，业务也不得据此重试非幂等写入。需要重试时必须使用业务幂等键或数据库唯一约束，并在外部确认最终状态。
- native `handler_timeout_ms` 能终止会到达编译器 safepoint 的 Ku 计算循环并返回 504，但不是线程抢占。超时展开给 `finally` 固定一秒清理窗口，窗口结束后的 safepoint 会转向超时出口；同步 OS/FFI 阻塞仍须由下游 timeout 限制，因此 `finally` 仍应保持有限、非阻塞。
- `std.pg` 不实现 COPY；COPY/BAD_RESPONSE、超时、协议失步或非 IDLE 会话都直接淘汰该槽位，不做无 deadline 的隐藏排空/ROLLBACK。
- MySQL prepared result 默认不能把 `mysql_stmt_execute()==0` 当作有列结果终态；当前结果只有在 `mysql_stmt_fetch()` 返回 `MYSQL_NO_DATA` 后才确认完整。在此之前的 deadline、metadata、分配、bind、fetch、上限或 UTF-8 失败均返回 `mysql/execution_unknown` 并淘汰连接。顶层 `CALL` 在借连接前拒绝，直到实现有界的 `mysql_stmt_next_result()` 全结果循环。
- 三个数据库 client 都由 Ku move-only/checker 管理：`close()` 消耗并清空唯一 owner，只读 handler 不能关闭捕获的外层 client，所以正常 Ku 路径不允许新 use 与 consuming close 竞争。生成的数据库 helper 是 translation-unit 内部 `static` 实现而非第三方 C ABI。绕过 checker 直接调用生成 C 时，调用方必须外部串行化同一 client 的 close/use，close 不得与 query/execute/release 并发。内部 `finalizing` 只仲裁仍存活 allocation 上已进入的 close/release 路径，确保最多一个销毁者；它不支持重复 close、close 返回后使用裸指针，或 operation 尚未登记时与 consuming close 同时开始。
- `std.net` 同样只支持安全 Ku 生命周期：`close()` 消耗绑定，close 后再次 read/write 由 checker 拒绝；read/write 借用 client，`write(bytes)` 也不消耗 bytes。生成 helper 是 translation-unit 内部实现，不是可供第三方自由并发调用的稳定 C ABI；raw C 调用方必须外部串行化同一 client 的 close/use，close 不得与 read/write 并发。read/write 本身由一个带 deadline 的 gate 串行，但多个 `write` + `read` 组合不构成协议事务，应用层协议仍须自行定义消息边界。
- move-only native handle 可以放入 array 并随 array 一起 move/drop，但不能调用需要复制元素的 `first/last/try_get/push/concat/map`；checker 会直接拒绝，不会进入 backend 的 forbidden-clone trap。捕获到共享 cell 的数据库/net receiver 也不能带有副作用的参数；应先把参数求值到普通局部变量，再调用 receiver，避免参数回调把 A client 替换为 B client 后错误地操作 B。
- `net.client` 的 host 只接受最多 253 bytes 的可见 ASCII；国际域名先转 punycode。resolver 最多尝试 64 个地址，全部地址共享同一个 connect deadline；同步 `getaddrinfo` 仍只能在返回后复核 deadline。TLS 只新增 `tls: bool`、`tls_server_name: str`、`tls_ca_pem: str`；TLS-only 字段在 `tls: false` 或缺省 TLS 时会被拒绝，`tls_server_name` 缺省使用 `host`，显式 `tls_ca_pem` 完全替换内置 WebPKI roots，不提供 skip-verify/insecure 开关。`read(n)` 只执行一次有界应用数据接收，可能少于 n；EOF（TLS 下包括缺失 close-notify 的截断）、已经开始 socket/TLS I/O 后的 read/write timeout、transport failure 和 gate synchronization failure 会把 stream 标记为不可再用，避免继续消费失步数据。仅等待并发 gate、尚未触碰 transport 就到期的操作返回对应 timeout，但不会污染 stream。

- native TLS 构建只从匹配目标/compiler ABI 的 v1 target pack 读取 archive/header/manifest。CLI 有界校验尺寸、SHA-256、对象格式、必需 ABI symbols 与 build id，并从固定句柄复制到私有 staging 后才链接。manifest 中的 hash 只是 pack 内部完整性值，不是发布者签名；pack 来源、compiler 和构建环境仍是信任边界。archive 静态链入最终产物，因而没有 `ku-native-tls` shared-library sidecar；目标系统 CRT/系统网络库及 loader 依赖仍存在，不应将它描述为完全静态产物。FFI panic fence 能阻止 unwind 穿过 C ABI，但默认 panic hook 仍可能向 stderr 写诊断；显式 TLS buffer 使用 fallible reserve，rustls/标准库内部遇到进程级 allocator OOM 仍可能终止进程，不能宣称所有 TLS OOM 都会转换成 Ku `Result`。

## 本轮驱动错误恢复与资源验收

- Redis 建连、借用、逐连接 AUTH 和命令共享绝对 deadline；同步 DNS 只能返回后复核。三个驱动的构造前配置校验统一使用 `invalid_config`；client/池层统一使用 `client_closed`、`pool_busy`、`acquire_timeout`、`connect_timeout`、`connect_error`、`sync_error`、`out_of_memory`，命令层的 `timeout` / `redis_error` 不混入池合同。分配失败返回静态 `redis/out_of_memory`；未完整消费的响应会 poison 该连接，完整消费后的值/UTF-8 错误不会污染其他槽位。
- PG/MySQL 的 `result.value(): str!` 是唯一读取写法：NULL、越界和复制 OOM 都可 catch，原结果仍可再次读取或释放；`is_null()` 使用同一边界检查。
- 新 client 的专项测试覆盖 OOM、waiter 上限、防未排队 newcomer 直接夺槽、退避定时责任转交、single-flight 懒建、close 与 borrowed 并发、session reset、清理失败、坏连接单槽淘汰、密码不进诊断和延迟销毁。失败退避窗口从 25ms 指数增长并封顶 1000ms，实际 equal-jitter 是 `ceil(window/2)..window`（首次 13～25ms），健康空闲连接不受影响；已入等待集合的请求由平台 condition variable 无序选择，不保证 FIFO 或无饥饿。旧单连接/旧 pool 的实库结果只作底层回归证据，不能替代新 API 验收；三系统 workspace CI 已通过，当前实库缺口仍是 Redis/MySQL 新 client 与 PG 的 Linux/macOS 查询。
- 三个驱动的结果 cap 都是单结果限制，不是进程总 retained-memory 预算；多个并发查询和长期持有的 detached result 会叠加，生产部署仍需进程级内存与并发上限。

## 通用 TLS 的落地决策

本轮没有增加一个只收发 `str` 的“伪通用 TLS”接口。native owned `bytes` ABI 与 socket-free TLS runtime ABI 已由 generated C 接到同一 `net.client(config)`；设置 `tls: true` 仍使用同一 `read/write/close` 接口。runtime 使用 rustls/ring、TLS 1.2/1.3，只允许内置 WebPKI 根或完全替换它的显式 PEM CA，不读取系统根，也不能关闭证书或主机名验证。尚未完成的是解释器 parity、HTTP/Redis 复用和 TLS server/mTLS；不公开只返回 `str` 的第二套 TLS API。

后续按以下顺序落地：

1. 已固定 native owned `bytes` 与 move-only `std.net` transport ABI；解释器 parity 仍未完成。
2. 已固定 Ku 自有的 socket-free rustls C ABI、WebPKI/显式 PEM CA 二选一信任模型、握手/缓冲上限和不可关闭的证书与主机名验证；它本身不做 socket I/O 或等待。
3. 已在同一个 `net.client(config)` 中接入 TLS，不增加 `tls.connect` 第二套入口；socket connect 与握手共享 connect absolute deadline，各次读写使用各自的 absolute deadline。
4. 让 HTTP/Redis 与 TLS 复用 bytes/net transport；是否增加 TLS server 和 mTLS 必须另行固定合同，不能从 client runtime 推导为已有能力。
5. runtime 与 generated-C 合同已覆盖 TLS 1.2/1.3、自签 CA、错主机、不受信 CA、截断 close-notify、分片/慢握手和资源上限；v0.0.17 精确 pack 的三系统消费者、打包及完整门禁已由 [c668283 的 tag CI](https://github.com/fengyanweb/ku/actions/runs/33969256015) 通过。未来 runtime/pack 变更必须重新验收，不能沿用这个提交的结果。

## 下一段协议与生态工作

native `bytes/std.net` 明文 transport、socket-free TLS runtime ABI 和 generated-C TLS 驱动已落地，v0.0.17 精确 pack 的三系统最终消费者门槛已通过。v0.0.18 先收口语义、native 泛型与 stackless async、事件 runtime 和 HTTP 资源合同，再验收真实数据库与性能；RESP3、registry v2、官方托管与高可用不在本轮范围。当前 Redis 仍保留私有明文 transport，不能把 bytes/net 基础 ABI 写成内部复用已经完成。package registry 已补自托管离线 operator 管理的开发者、团队成员增删、包名认领/转移、token 与 hash-chain 审计；在线注册/登录、团队角色、外部不可抵赖审计、跨节点一致性和官方托管仍未完成，不能从本地 E2E 推导生产并发能力。
