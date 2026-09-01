# Ku Registry API v1

这份协议定义第三方 registry 与 Ku CLI 之间唯一的 v1 客户端接口。仓库同时提供可自行部署的有界参考服务 `ku-registry`；现有 `ku package publish`、`ku package yank`、依赖解析器和该服务使用同一套 v1 路径与包格式，不提供第二套发布或撤回语法。`ku` Rust crate 不公开“解析未签名 index 后直接下载”的低层 SDK 入口，第三方 registry 必须走这里记录的 HTTPS + pinned Ed25519 信任链。它不是官方托管服务，也尚未经过生产吞吐量或高并发基准测试。registry 基址必须是 HTTPS、没有凭据/fragment/query，并以 `/` 结尾，例如：

```txt
https://packages.example/v1/
```

项目在 `ku.mod` 固定 registry origin 与 Ed25519 公钥：

```txt
registry.url = "https://packages.example/v1/"
registry.public_key = "ed25519-<64 hex digits>"

dep.math = "^1.2.0"
```

registry 依赖的唯一写法是 `dep.math = "<exact 或 caret>"` 并省略 `dep.math.source`；本地开发覆盖才写绝对 URL，例如 `dep.math.source = "file://C:/work/math"`，Windows 路径也使用 `/`。消费者跨包 import 只使用 `import { Value } from "@math/path"`，不提供第二套 registry source 或 package import 别名。`ku.mod` 的每个 key 都只能声明一次，包含 `registry.*` 以及所有 `dep.*` 字段；重复字段直接拒绝，不使用 last-wins。

能修改项目 `ku.mod` 的人本来就能修改源码和依赖，因此项目配置本身属于受信输入。token 不得写入 `ku.mod`、`ku.lock`、命令行或日志；CLI 只读取 `KU_REGISTRY_TOKEN`。

## 包格式

媒体类型：

```txt
application/vnd.ku.package+tar.zstd
```

归档后缀固定为 `.tar.zst`，只包含一个 `<name>-<version>/` 根目录。`ku package pack` 固定条目顺序、mtime=0、uid/gid=0、目录 mode=0755、文件 mode=0644 和 zstd level 3。只打包：

```txt
ku.mod
src/
README / README.md
LICENSE / LICENSE.md
docs/
examples/
tests/
```

不会包含 `.ku`、`ku.lock`、`.git`、`.env`、构建产物或安装脚本。包只能提供 Ku 源码与文档；v1 不执行 preinstall/postinstall/build script，也不加载第三方 DLL。

## Signed index

读取一个包的 index：

```http
GET /v1/packages/{name}/index.toml
GET /v1/packages/{name}/index.toml.sig
```

`index.toml.sig` 是 UTF-8 文本：

```txt
ed25519-<128 hex digits>
```

签名覆盖 `index.toml` 的原始字节，不能重新排版后再验签。index 格式：

```txt
name = "math"

[[version]]
version = "1.2.3"
url = "../../artifacts/math-1.2.3-sha256-<64 hex digits>.tar.zst"
checksum = "sha256-<64 hex digits>"
dep.core = "^2.0.0"
```

每个版本的 `dep.<name>` 都在签名范围内。index 是当前可供 fresh/refresh 求解的完整版本集合；当一个包的所有物理版本都已 yank 时，合法的已签名 index 只有 `name = "<name>"`，没有 `[[version]]`。下载后 CLI 再解析归档中的 `ku.mod`，要求 name、version、`src` root 和依赖集合与签名 index 完全一致。

artifact URL 必须是稳定、无 query 的 HTTPS `.tar.zst` URL；v1 不把有过期时间或凭据的预签名 URL 写进 `ku.lock`。index 与 detached signature 是两个请求，客户端遇到签名不匹配时只会在同一个总 deadline 内重新获取，最多获取 3 个完整 pair；持续变化或第三次仍不匹配时 fail-closed。

参考服务对内容寻址 artifact 的 GET 使用最多 1024 项的有界校验缓存。同一路径的并发冷请求只执行一次完整 SHA-256，等待请求仍各自遵守自己的绝对 deadline；每次请求都会重新打开文件并比较文件身份、长度和修改时间（Unix 还比较 ctime），因此替换或修改会使缓存失效。只有完整读取且指纹未变化时确认的 checksum 不匹配才按相同文件指纹负缓存；临时 I/O 错误、文件变化、超时或校验 panic 都清除本次校验状态并唤醒等待者，后续请求可以重试。损坏 artifact 在发送成功响应头或 body 前返回 500。成功响应包含 checksum `ETag` 和 `Cache-Control: public, max-age=31536000, immutable`。index 响应不使用这些 immutable artifact headers。

index 最大 1 MB、4096 个版本、单行 8192 bytes；URL 最大 2048 bytes。版本只接受无前导零的 `major.minor.patch`，依赖只接受精确版本或 caret 范围。

## Publish

```http
PUT /v1/packages/{name}/{version}
Authorization: Bearer <KU_REGISTRY_TOKEN>
Content-Type: application/vnd.ku.package+tar.zstd
Content-Length: <bytes>
X-Ku-Checksum: sha256-<64 hex digits>
Idempotency-Key: <name>-<version>-sha256-<64 hex digits>

<raw .tar.zst bytes>
```

服务端响应：

| 状态 | 语义 |
| --- | --- |
| `201` | 首次发布成功，artifact 和已签名 index 已同时可读 |
| `200` | 相同 name、version、checksum 和依赖元数据的幂等重复提交 |
| `401` / `403` | token 无效或无包名权限 |
| `409` | 同一 name/version 已存在不同 checksum；永不覆盖旧版本 |
| `413` | 归档超过 32 MB |
| `429` | 同包已有 mutation、全局容量已满或提交时 OS 锁忙；本次未提交，调用者可稍后重试同一个幂等请求 |

CLI 不对 publish 自动重试，避免网络断开时制造不明确状态。成功响应后，CLI 必须重新获取并验签 index；只有新版本的 checksum 与依赖元数据都一致才报告成功。

## Yank

已经发布的问题版本只能单向 yank，不提供 delete、unyank 或以重复 publish 恢复可见性的第二套状态变更：

```http
PUT /v1/packages/{name}/{version}/yank
Authorization: Bearer <KU_REGISTRY_TOKEN>
Content-Length: 0
Idempotency-Key: yank-<name>-<version>

```

请求不得携带 body，ACL 与 publish 完全相同并精确绑定包名。服务端响应：

| 状态 | 语义 |
| --- | --- |
| `200` | 首次或重复 yank 成功；目标版本已经不在当前签名 index 中 |
| `400` | Content-Length 非 0、携带 body 或 Idempotency-Key 不精确匹配 |
| `401` / `403` | token 无效或无该包名权限 |
| `404` | 包或物理版本不存在 |
| `429` | publish/yank 共用的有界 mutation 槽已满；可稍后重试同一个幂等请求 |

`ku package yank [path]` 仍只从 `KU_REGISTRY_TOKEN` 读取 token。收到 200 后客户端重新获取 index 与 detached signature、验签并确认目标版本已省略；网络在响应前断开时结果可能不明确，调用者用同一个固定 Idempotency-Key 重试即可。

yank 只影响新的 fresh/refresh 求解，不删除或改写 artifact。既有 `ku.lock` 仍可在 `--locked` 下按已经固定的无 query URL 与 SHA-256 补 cache，`--offline` 可继续使用校验通过的本地 cache。这样能保持已有构建可重复，但也意味着 yank 不是强制阻止已锁定消费者执行该版本的安全撤销机制。

参考服务把单向 tombstone 保存为包目录下的空真实目录 `.yanks/<version>/`。启动审计拒绝 symlink、非空 marker、非法版本名和没有对应物理版本的 marker；tombstone 永不由正常 API 删除。clean startup 会在固定 deadline 和 4096 版本/65536 index item 边界内扫描 `versions/` 的小型 `entry.toml`，要求物理版本集合精确等于“当前签名 index 中可见版本 + tombstone”，并要求每个可见版本的 name/version/checksum/dependencies 与签名 index 完全一致。启动审计不会重新读取或哈希所有历史 artifact bytes；artifact 仍在 GET 时校验，带 pending marker 的待恢复版本仍按恢复流程校验。yank 不会释放发布容量。

v1 index 没有单调 revision，因此 Ed25519 签名本身不能识别一个历史但签名仍有效的 pre-yank index。refresh 从配置的 HTTPS origin 获取当前快照可以获得参考服务的单向语义，但能回放旧响应的 registry/CDN 仍可重新展示旧版本；需要抗回放的密码学撤销必须进入带 revision/transparency 的后续协议。参考服务一旦写入首个 tombstone，也不能回滚到不理解 `.yanks` 的旧服务版本，否则旧版本可能从物理 artifact 重建并重新暴露已 yank 版本。

## 启动自托管服务

`ku-registry` 只有一套配置入口，全部来自环境变量：

| 变量 | 含义 |
| --- | --- |
| `KU_REGISTRY_BIND` | 监听 IP 与端口；默认 `127.0.0.1:8443` |
| `KU_REGISTRY_DATA_DIR` | 服务独占写入的数据目录 |
| `KU_REGISTRY_CREDENTIALS_FILE` | publish/yank 的 schema 2 身份、团队、所有权、token hash 与审计链文件 |
| `KU_REGISTRY_SIGNING_KEY_FILE` | Ed25519 32-byte seed 文件 |
| `KU_REGISTRY_TLS_CERT_FILE` | PEM 证书链 |
| `KU_REGISTRY_TLS_KEY_FILE` | 与证书匹配的 PEM 私钥 |
| `KU_REGISTRY_WORKERS` | 固定 worker 数，默认 16、范围 1..64 |
| `KU_REGISTRY_QUEUE_CAPACITY` | 有界连接队列，默认 32、范围 1..256 |
| `KU_REGISTRY_REQUEST_TIMEOUT_MS` | 从 accept 时刻开始的 TLS/header/body/校验/提交总 deadline，默认 15000、范围 100..60000 |

签名 key 文件只接受一种格式：

```txt
ed25519-<64 hex digits>
```

新的自托管部署使用同一个 schema 2 文件保存开发者、团队成员关系、包所有权、带 scope 的 token hash 和审计链。它是有界 UTF-8 文本快照，不是 SQLite 数据库；因此没有 SQL migration，也不存在数据库锁表风险。服务端不保存明文 token，也不支持通配符包名。记录形态如下，实际应只由管理命令生成，不应手写 hash 或审计行：

```txt
schema 2
developer alice
team core
member core alice
owner math team:core
token sha256-<64 hex digits> alice all
audit <sequence> <previous-hash|root> <event-hash> <before-transition-hash> <after-transition-hash> <action> <subject>
```

`developer token-issue` 生成的 token scope 固定为 `all`，但“all”仍只表示该开发者直接拥有的包和其所在团队拥有的包。旧 ACL 迁移产生 `package:<exact-name>` scope，防止多个旧 token 和多个包被错误展开成笛卡尔积权限。运行时先按 developer/team 建索引，再展开最多 4096 条精确 `(token hash, package)` 授权；工作量接近输入记录数与实际有效授权数之和，不执行每枚 token 对每个 owner 的笛卡尔扫描。hash 存在于有效 ACL 但包名不匹配时返回 403，未知 hash 或当前没有任何有效包授权的开发者 token 返回 401。token hash 使用 SHA-256；SHA-256 不会把低熵 token 变成强凭据，管理命令始终从 OS CSPRNG 生成 32 个随机字节。

治理控制面只有一条离线运维路径。拥有凭据文件写权限的 registry 运维者执行命令；bearer token 只能 publish/yank，不能创建身份、加入团队、认领或转移包。全新部署先执行 `ku-registry governance init <first-developer>`，该命令只接受不存在或没有有效记录的凭据文件，重复初始化 fail-closed。随后用以下命令签发首枚 token、认领首个包，再启动服务；服务会拒绝没有任何有效 token-to-package 授权的中间快照：

```txt
ku-registry developer create <developer>
ku-registry developer token-issue <developer>
ku-registry team create <team>
ku-registry team member-add <team> <developer>
ku-registry team member-remove <team> <developer>
ku-registry package claim <package> <developer:name|team:name>
ku-registry package transfer <package> <developer:name|team:name>
ku-registry developer token-revoke <developer>
ku-registry audit verify
```

`developer token-issue` 输出一个 `ku_` token；明文只在文件原子提交成功后向 stdout 输出一次，应直接进入受控 secret store。`developer token-revoke` 不接受 token 参数，只读取 `KU_REGISTRY_TOKEN`，并删除该 hash 属于指定开发者的全部 scope；若这会让任一包失去最后一个有效发布者则事务拒绝且文件不变。轮换顺序固定为先 issue 并分发新 token，再 revoke 旧 token。团队成员拥有团队全部包的发布权；`member-remove` 会在同一事务中检查团队包仍有至少一个有效成员 token，否则拒绝，必须先加入并为替代成员签发 token。`claim` 只接受未被认领的包；已有包只能 `transfer`，且目标开发者或团队必须已存在并对该包拥有至少一个有效 token。

正常开发者 token 生命周期只有上述 `token-issue` 与读取 `KU_REGISTRY_TOKEN` 的 `token-revoke` 两条命令。唯一的例外是离线 operator 恢复：若 `developer token-issue` 返回“凭据已提交但 stdout 写入/flush 失败”，明文已不可恢复，诊断会给出非秘密 hash 和一条精确命令：

```txt
ku-registry developer token-revoke-hash <developer> <sha256-64-hex-digits>
```

该命令只能由本来就拥有凭据文件写权限的运维者用于清理这枚孤儿 token，不读取也不允许复用 `KU_REGISTRY_TOKEN`，不是 bearer API 或普通撤销的第二种业务写法。它只删除 hash 与 developer 同时精确匹配的 token，仍执行“每个包保留至少一个有效发布者”的不变量检查，并把 hash 恢复动作追加到同一个审计链。开发者不匹配、hash 不存在或格式不是精确 `sha256-` 加 64 位十六进制时都在写入前失败。若不变量阻止清理，先按正常路径安全签发并保存替代 token，再执行该离线恢复命令。

已有两字段 ACL 文件必须显式迁移，不能由服务启动时静默转换：

```txt
# 停止服务并先建立权限同样严格的离线备份
ku-registry governance migrate <developer>
ku-registry audit verify
# 重启服务并验证启动成功
```

迁移在同一把跨进程锁内读取旧快照并原子替换；每条旧授权变为同一开发者的 `package:<name>` scoped token，因此迁移前后的有效 ACL 相等。审计重放用独立的 `migration-init`、`migration-token-import`、`migration-complete` 阶段封闭迁移：普通 token issue 只能产生 `all` scope，package scope 只能在未完成的迁移阶段导入，迁移缺少 owner、token 或 complete 事件都会拒绝。失败发生在替换前时旧文件不变；若报告 `registry_credentials_commit_uncertain`，必须先检查文件首个有效记录与审计链，不能盲目重试。回滚只允许在尚未执行任何后续 schema 2 变更时，停止服务后用迁移前的安全备份做同目录原子恢复；一旦发生 token、成员或所有权变更，恢复旧备份会回滚安全状态，禁止作为常规回滚方案，应在 schema 2 上执行补偿操作。旧 `token issue/revoke` 只服务尚未迁移的旧 ACL，schema 2 会显式拒绝它们。

管理操作持有 `<credentials-file>.lock` 跨进程 OS 独占锁，锁等待使用 10 秒绝对截止和有界轮询。锁内重新读取最多 8 MiB 的 UTF-8 文件；单行最多 8192 bytes，developer/team/member/owner/token 各最多 4096 条，审计最多 32768 条，展开后的有效 ACL 最多 4096 条。8 MiB 是为最坏 4096 条旧 ACL 的逐事件可重放迁移保留的固定启动/离线管理上限；服务只在启动时解析并展开凭据，publish/yank 热路径使用内存中的精确 ACL，不逐请求重读治理文件。修改、审计追加和不变量检查在内存中的一个新快照完成，随后写入同目录固定 staging、同步并原子替换，不存在“状态已改但审计未写”的两个文件窗口。既有凭据权限先复制到尚未写入内容的 staging 并读回验证：Unix 保留 uid/gid/mode；Windows 同时保留 OWNER、GROUP、DACL 及 DACL 继承保护位，并从 staging handle 回读逐字节复核。Windows 若当前 operator 无权恢复原 owner/group，或任一安全描述符字段无法读取、设置、复核，会在写入凭据内容及 replace 前 fail-closed，而不会让新 staging 的 operator 身份静默成为最终 owner。Unix 首次新建凭据、锁和 staging 请求 `0600`；Windows 没有 `0600` 等价保证，首次新建仍继承父目录权限，必须预先将凭据父目录的写权限限制为受信 registry 服务账号和管理账号。凭据、锁、staging 的 symlink 与 Windows reparse point 都拒绝，已打开锁还会和当前路径比较 OS file identity。所有正常修改者必须共享该命令和 lock；持有父目录写权限的非协作本机进程仍可在 staging/rename 边界竞争或重写文件，这不在当前威胁模型内，因此不能把路径检查或同文件 hash 链视为对恶意本机 writer 的防护。底层同步 I/O 不能由用户态 deadline 强行中断，目录必须位于可信本地文件系统。

Unix 扩展文件 ACL 不会被静默丢弃。管理命令在原凭据的安全打开 fd、以及尚未写入内容的 staging fd 上分别检查，首次 issue 也检查 staging，避免父目录默认/继承 ACL 绕过 `0600`：Linux 只接受无 POSIX access ACL 或完全等同普通权限位的三个基础项；macOS 只接受无 extended ACL 或不带任何条目和 flags 的显式空 ACL，延迟继承、禁止继承等 flags 也会拒绝。扩展/未知 ACL、无法可靠查询的文件系统或未实现 ACL 检查的 Unix 平台返回 `registry_credentials_acl_unsupported`，其他 ACL 查询 I/O/权限错误返回 `registry_credentials_permissions_failed`，都在写入和替换前停止并保留旧凭据；`ENOTSUP` 不被误当作“无 ACL”。当前不会自动移除 ACL，也不提供绕过检查的另一种命令；凭据目录应预先配置为可可靠检查的普通 Unix 权限环境。

上述 Linux/macOS ACL 行为目前是源码合同与平台条件测试，不能由 Windows 本机结果代替真实 Unix syscall 验收；对应 workspace workflow 在 Linux/macOS 实际跑绿前，不应把它写成三系统部署已验证。

运行中的 `ku-registry` 在启动时静态加载凭据，不热加载。每次治理变更后必须由服务管理器重启并确认启动成功；transfer、member-remove 和 token-revoke 等收窄权限的操作若要求立即生效，应先停止服务，再修改并重启，不能把磁盘提交时刻误认为运行中实例的授权切换时刻。每个审计 event hash 同时绑定前一事件、动作、主体以及确定性的 before/after rolling transition hash；该 transition hash 从前一值、动作和主体增量计算，避免对每个历史事件重新哈希完整状态。验证从空状态开始，只接受固定动作集合，按 subject 重放每个 developer、team、member、owner、token 和 bootstrap/migration phase 变更；before 必须连续、after 必须等于本事件的增量结果、每个事件必须真实改变治理状态或受限控制阶段，最终阶段必须正常且重放状态还必须逐项等于文件当前状态。因此未知 action、追加同状态事件、虚假的 create/transfer/revoke、未同步修改审计的状态篡改都会失败。它仍不是 keyed log，且审计与治理状态位于同一文件，所以拥有文件写权限的恶意管理员仍可重写状态并重新构造一条语义自洽的完整历史，不能称为不可抵赖日志或外部透明账本。Unix 替换后的目录同步失败会报告 `registry_credentials_commit_uncertain`；token issue 的此类错误及 stdout 写入/flush 失败只附带非秘密 hash，不输出明文 token。所有有状态治理命令都检查并 flush 成功确认；若状态已经原子提交但 stdout 随后失败，进程返回明确的 committed-but-output-failed 错误，磁盘状态仍生效，盲目重试可能得到 already-exists、already-applied 或 already-revoked，应先检查审计链。`audit verify` 是纯读取，验证成功但输出失败只表示无法交付确认，不表示发生过状态提交。当前闭环是自托管、离线 operator 管理的开发者名、团队成员、包所有权、转移、token 和可验证审计；它仍不提供在线注册/登录、邮箱或组织验证、团队角色、token 到期、身份/团队删除、跨节点一致性、不可变外部审计或官方托管服务，不能称为完整公共开发者身份平台。

构建后直接启动：

```txt
ku-registry
```

进程启动时会输出实际 HTTPS 地址和需要写入消费者 `registry.public_key` 的公钥。服务会在 canonical data dir 内持有生命周期级 `.instance.lock` OS 独占锁；同一目录的第二个实例结构化失败，原实例退出后才可重启。生产部署仍应使用正式证书、限制数据目录和 secret 文件 ACL，并由服务管理器负责进程重启。data dir 必须位于受信的本地文件系统；服务不声称能抵御拥有该目录写权限的本地恶意进程。不要把明文 publish token 写进命令行、`ku.mod` 或日志。

## 参考服务的原子性与并发

参考服务实际执行以下约束；其他 v1 实现也必须保持相同的客户端可观察语义：

1. 对 `(name, version)` 建唯一约束或 CAS；同内容幂等，不同内容冲突。
2. 边接收边计算 SHA-256，不能把整个包无界读入内存。
3. 使用 Ku 相同的受限解包规则校验路径、链接、文件数和大小。
4. 校验归档 `ku.mod` 的 name/version/dependencies。
5. artifact 写入不可变内容寻址存储。
6. 先生成新 index、对 exact bytes 签名并写入不可变 generation，再用一个原子 pointer 切换 index 与 signature 的同一快照；generation identity 同时绑定验证公钥与 index bytes。
7. 只有 artifact、index、signature 都可读后才返回成功。
8. yank 与 publish 共用一个最多 4 个包名的 mutation admission 集合，在认证及请求头校验后、读取上传体或创建 staging 前取得 RAII guard。同一包名（包括不同版本）的重复请求、全局容量已满都立即返回 429；没有同包排队者持有全局槽。提交时仍使用每包 OS 锁保护存储，但 mutation 只尝试一次，锁忙也返回 429；startup 与 GET recovery 继续使用原有绝对 deadline 内的锁等待。guard 在失败清理和 OS unlock 后释放。yank 先持久化候选签名 generation 与 pending marker，再持久化 tombstone，最后原子切换 pointer；崩溃恢复在 tombstone 尚未建立时保留版本可见性，一旦 tombstone 已建立就从重建 index 中省略该版本。
9. 对上传体、并发数、请求时间和临时文件设置硬上限；失败后清理 staging。

参考服务使用每包 OS 文件锁和不可变版本目录，不依赖数据库。数据库实现可用一条唯一约束实现发布竞争：

```sql
UNIQUE (package_name, version)
```

事务中先比较已有 checksum；相同则返回幂等成功，不同则返回 409。不要用“先查再插”的非原子流程。

HTTP parser 只接受 HTTP/1.1 origin-form 的 GET/PUT，并要求 Host；不提供第二套 HTTP/1.0 行为。重复 header、obs-fold、Transfer-Encoding、Expect、绝对 URI、百分号路径和反斜杠路径均拒绝。HTTP/1.1 默认复用连接；客户端显式 `Connection: close`、任意解析/业务/响应错误以及未安全消费请求体时都立即关闭。不会复用的错误 PUT 不发起额外 socket read，也不等待未受信请求体；只有完整且不超过 8 KiB 的 body 已随 header 解密进现有缓冲区时才直接丢弃它，避免 Windows 在关闭带未读数据的 socket 时用 reset 吞掉错误响应，同时不让未认证慢上传占住 worker。一个 TLS 连接最多处理 8 个成功请求；第一个响应之后，等待并完整读取下一请求头的时间最多 1 秒，随后 body、处理和响应仍只能使用 accept 时刻开始的同一个绝对 deadline 的剩余时间。固定 worker 因此最多被一个完全空闲或慢请求头的复用连接额外占用 1 秒，已收到完整请求头的请求也不能越过连接总 deadline，不存在无界连接循环；这仍是简单同步线程池，不代表异步服务器的并发规模。

accept 后的连接携带绝对 deadline 进入有界队列，过期排队连接直接关闭；队列满直接关闭，不在 accept loop 做 TLS handshake。该 deadline 不能中断已经进入内核的文件 `sync_all`，也不涵盖客户端 accept 前的 DNS/网络工作。publish 与 yank 合计最多 4 个不同包进入 mutation 区，每包同时最多一个；已活跃包的重复请求在上传前拒绝，不能用同包等待者占满全部 mutation 槽。所有早期 429 沿用已缓冲小 body 的无 socket read 丢弃规则，不等待缺失的 body。429 不代表幂等成功；客户端稍后显式重试，仍需得到正常 200/201 或内容冲突 409，并完成原有签名校验。

publish 的单包压缩体最大 32 MB、解包最大 128 MB、zstd window 最大约 128 MiB。因而 4 个不同包同时执行归档校验时，仅 zstd window 理论最坏上界仍约为 512 MiB，实际进程还需要 tar、文件缓冲、TLS 和索引内存；部署者应在容器或服务管理器中另设进程级内存限制。同包 admission 不等于按用户公平调度，也不消除多个不同包或慢 TLS 连接占满线程池的风险，不能据此推导生产高并发或整体抗 DoS 能力。

发布在锁内先完成候选 index 全量预算检查、签名和 staged artifact 校验；正常新增版本从已经验签的 immutable index 读取历史元数据，只校验本次新 artifact，不随版本数增长反复 hash 全部历史 artifact。成功完成版本目录 rename、signed-generation pointer 原子替换以及相应目录 sync 才返回 201 durable commit。重复发布同一不可变版本只重验 artifact 和当前 index，不删除 tombstone，因此不能隐式 unyank。pointer replace 已对并发 GET 可见但随后目录 sync 失败时，请求可能返回 500；客户端必须用同一个 Idempotency-Key 重试，服务会重验并补做 sync。durable commit 之后的旧 generation 清理失败只记录并留待启动恢复，不把已经提交的版本再改写成失败响应。启动时 clean package 验证 pointer、generation digest、签名及 tombstone/index 不相交，不重新 hash 全部 artifact；dirty package 在每包 60 秒、全启动 300 秒硬 deadline 内严格扫描全部 bounded metadata。存在 pending marker 时只额外校验 marker 指定且已 rename 的版本 artifact，损坏则启动 fail-closed；随后用全部 metadata 减去 tombstone 重建 index，所以已建 marker 的版本不会因崩溃重新暴露。仅 cache/pointer 损坏而无 pending 时同样按 metadata 与 tombstone 重建 index，历史 artifact 在各自 GET 时从同一打开的 file handle 复验 SHA-256，篡改请求返回 500。这样恢复成本不会随全部历史 artifact 字节数线性放大，同时 pending commit 仍先验证后签名。

Unix 上文件和目录均执行 `sync_all`；首次创建 `packages/<name>/` 后还会同步它的 `packages/` 父目录，重试已有根时再次同步，以补齐创建目录项的持久提交。Windows 上文件使用 `sync_all`，原子替换使用 `MoveFileExW(..., WRITE_THROUGH)`，但 Rust 当前没有为该实现提供可移植的目录 fsync；`packages/` 父目录同步在 Windows 只能采用现有 best effort。启动审计可以修复进程崩溃留下的 pending 状态，却不能诚实保证突然断电时目录元数据与 Unix fsync 完全等价。需要严格掉电持久性的 Windows 部署应使用具备写入日志/快照的外部存储层或在部署环境额外验证文件系统语义。

当前没有官方托管 registry、跨节点复制、在线 key rotation、撤销服务或生产性能数字；这些不能从参考服务的本地并发测试推导出来。

开发者可显式运行一个默认忽略的本机有界负载门禁：

```txt
cargo test registry_tls_concurrency_keep_alive_and_overload_load_gate -- --ignored --nocapture --test-threads=1
```

该门禁固定使用 4 个 worker、8 个排队连接、8 个并发 keep-alive TLS 客户端（每连接 4 次 GET）和 24 个过载 TLS 客户端；整个负载阶段最多 20 秒，各连接共享绝对 deadline。输出包含吞吐、成功、拒绝、内部错误和 accept 数。结束时必须 join 服务及 worker、确认 staging 为空，并能删除完整测试目录。它用于防止连接复用、过载拒绝和资源退出发生回归，不是生产吞吐数字，也不替代固定硬件上的 P99、RSS、FD 和长时间 soak 测试。

`ku package pack/publish` 读取的是开发者本机 workspace。三种受支持桌面系统都不会跟随枚举后替换的文件链接：Unix 从固定根目录句柄逐组件 `openat(O_NOFOLLOW)`，中间组件还要求目录，最终打开使用 `O_NONBLOCK`，所以 FIFO 替换不会卡住；Windows 用 `OPEN_REPARSE_POINT`、最终句柄路径的根边界以及 volume/file identity 拦截文件 symlink 和中间 junction/reparse 逃逸，源文件句柄打开后也只共享读取，禁止新的写入或替换。读取前后还比较 identity、大小和 mtime/ctime（Windows last-write），不一致立即失败且不重试，因此不会从替换到 package 根外的路径读取文件内容。仍需让 workspace 在打包期间保持静止并只由受信账号写入：能预先持有同一 inode/共享写句柄、在时间戳粒度内原地改写等长内容并恢复元数据的本地恶意写者，不存在可由跨平台 `std` 提供的原子只读快照或强制锁来完全排除；需要该威胁模型时应在只读 snapshot/worktree 中打包。

## 客户端 cache 与 lock

registry artifact 安装到：

```txt
.ku/cache/packages/<name>/<name>-<exact-version>-sha256-<digest>/
```

本地 file override 使用独立的 FNV 内容寻址 root：

```txt
.ku/cache/packages/<name>/<name>-<actual-version>-fnv64-<16 hex>/
```

registry `ku.lock` 记录精确版本、registry、无 query 的 artifact URL、SHA-256 和派生 cache key，不把机器上的绝对 cache 路径当作权威。file lock 记录 requirement、实际版本、绝对 source、`ku.mod`/`src` 运行快照 checksum 与派生 cache key；绝对 `file://` source 是本地开发覆盖的显式例外，迁移时必须更新或移除。两类 lock 都按目标机器的当前 cache 根重新定位；registry cache 同时校验 artifact SHA-256、archive 文件树摘要和已解包文件树摘要，file cache 重新校验固定的 FNV 快照，内容篡改都会 fail-closed。

同一个 cache key 使用系统文件锁，同一时刻只允许一个安装者下载和解包；成功安装后等待者复用结果，首个安装者最终失败时后续等待者仍可在剩余预算内接管并重试，因此不能把它解释成“失败时也严格只有一次网络请求”。进程退出后锁由操作系统释放，其他进程有界等待。同一 package cache 根提供 8 个跨进程共享的全局下载槽，因此不同包合计最多并行下载 8 个；依赖图最多 256 个包、求解最多 20000 步，单包和整图预算都在写入下一批内容前检查。从一次 package 联网操作开始，cache usage/install/download 锁等待、求解、获取、重试、分块校验与安装共享一个 300 秒绝对预算；等待已经消耗预算时，客户端会为后续 HTTP 获取采用缩短后的连接和读取超时，同时正常预算下继续复用会话连接池。该预算不涵盖之后的整个编译或程序运行阶段，也不是硬实时保证：同步 DNS，以及已经进入内核的单次 file read、sync 或 rename 不能在绝对时刻由 Rust 层强行取消，但调用返回后会再次检查。若 immutable cache 的 rename 在截止前进入内核、却在截止后才成功返回，本次操作可能报告 timeout 并留下已经完整校验的 immutable cache；后续重试会重新校验并复用它，不会覆盖该内容寻址目录。超时后不会再开始 cache quarantine 或新的安装步骤。

源码 import 展开同样有硬边界：单文件 1 MB、最多 4096 个源码模块（包含入口）、递归深度 32、累计源码 32 MB、展开后 65536 个顶层 item，以及 32 MB 的 source-equivalent AST 克隆预算。超限会在继续递归或复制前失败，不依赖增加 timeout 控制资源。

`ku package resolve . --offline` 以及 `ku check/run/build --offline` 禁止 registry 网络访问，也不读取绝对 `file://` source；它们只接受完整、与 lock 固定 cache key 匹配且重新校验通过的本地 cache，并且不改写 `ku.lock`。`--locked` 同样不改写 lock，但允许按 lock 中固定的 exact HTTPS URL/checksum 补 registry cache。cache 缺失、lock 漂移或内容篡改都会失败。

## 信任与密钥轮换

v1 使用项目显式固定的单个 Ed25519 registry 公钥，不存在“跳过签名”模式。轮换时由项目维护者在审核新 key 后修改 `registry.public_key`；旧 lock 仍由其精确 SHA-256 保证可重复安装。

自动 signed-roots、在线吊销和透明 key rotation 不属于 v1 协议，不能假装已经提供。需要这些能力的 registry 应在前置网关停止旧 key 服务，并通过正常代码审查更新项目 pin。
