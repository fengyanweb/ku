# Ku v0.0.18-rc.1 交付范围

当前达到`rc_prepared`，仅限已安装并预演的Linux ARM64隔离样例。实际[production-gate.json](/Users/mac/Documents/Ku/验收/生产前测试_2026-10-28/RC-v0.0.18-rc.1/production-gate.json)的七项`rc_checks`为true；真实业务门禁只有`identity_bound=true`，其余为null。不是`ready_for_canary`，没有实际生产或灰度流量。

## 业务与外部条件

| 字段 | 当前事实 |
|---|---|
| 真实应用/业务入口 | `null`；交付的是独立PG参考样例 |
| 真实部署目标/访问方式/域名 | `null`；本地Linux虚拟机仅用于隔离预演 |
| 业务流量、SLO、风险 | `null/tbd`；真实只读/幂等/非幂等及外部副作用待确认 |
| 生产回退对象 | `null`；没有已验证的旧生产后端，新服务应约定关闭或降级 |
| 真实切流与观察授权 | 未落实；未部署canary、未开始真实观察 |

Mac内部100请求/秒、250ms门槛是本轮独立30s样例契约；Linux320次请求是有间隔的小型功能预演。两者均不是生产负载上限或业务SLO。任务书普通API p99≤100ms只是讨论草案，原20ms失败仍保留。

## 固定身份

以[release-manifest.json](/Users/mac/Documents/Ku/验收/生产前测试_2026-10-28/RC-v0.0.18-rc.1/release-manifest.json)为准：

| 身份 | 值 |
|---|---|
| RC包提交 | `9cc43c06e7b61c64ade4f6c4a3260078a9c8ff96` |
| CLI源码提交 | `44f973397c55cdef4dbce13c62c2ea02702df213` |
| CLI SHA256 | `6006505dabac0a0d42208ea99169761c79424c1f4ae7f5e656787948bb1e7306` |
| 应用ELF SHA256 | `a2cda9d1274858727bc00045aa0acd16db6a09b07b7220eae19ca23c6abea0be` |
| 归档SHA256 | `1e6b164dfcd31d6d68bab2949dee5cb8ab01faa0feecf378193f69b198c0350f` |
| 配置模板SHA256 | `61e23174f543f492f7447978e6b8149e0cd8318054a5b223a66e5a3b59a42f5d` |

包名`ku-v0.0.18-rc.1-aarch64-unknown-linux-gnu-native-deployment.tar.gz`，5,488,124B。它包含native部署所需CLI/样例及工具，不是完整Rust runner SDK、VSIX或native-TLS pack；未公开发布。CLI使用rustc/cargo1.89.0，正常Release C后端O2；包提交增加部署工具/文档，CLI源码另行冻结。

本机最终MD和新门禁是在实测后形成的说明。已封存归档中的MD是准备时初稿，归档及其SHA保持不变；两者不能相互冒充。

## 已预演环境与边界

Linux aarch64、内核6.8.0-147-generic、glibc2.39、PostgreSQL/libpq16.15、HAProxy2.8.30、Python3.12.3；依赖解析见安装收据。以非root账户`mac`运行，实际验证installed CLI版本/check/native hello、样例服务、systemd、监控及回退。

参考链为HAProxy→native→私有PG，Ku service只负责监督。双活动实例与备用位共享宿主，不是节点级高可用；loopback验证不认证公网TLS或异地容灾。每个Ku整单元实测MemoryMax256MiB、CPUQuota200%、TasksMax64、FD8192、Stop16s；代理64MiB/100%/16任务、FD16384；PG256MiB/100%/48任务。均是本次样例预算，不可直接套到业务；supervisor/launcher/native、代理、PG、driver/观察器分别核算。

正常320请求全正确、64笔写入ID/数量一致；摘流后旧keep-alive与fresh共20次未再流向A，已接纳hold请求在SIGTERM后完整200。退出替代、PG故障恢复和样例版本回退均有收据。监控仅12次/约55s，保留spare缺席及wrapper FD权限缺口，不认证长期无增长。

历史Mac CLI31b6…/说明提交232…和旧性能CLI9428…/Ccf59…/ELF455…分别保留，不能充当新6006…的产物SHA。旧五项性能认证false、容量unknown；新RC代码的CI另见交付目录 `CI-RC-current.json`，历史CI仅按未受影响范围复用。

JSON旧默认、暂停的Value复制/释放与AST分析保持；无生产schema/data改动、无盲重试、无网站部署/公开发版/合并/购买。参见[复用表](evidence-reuse.md)、[操作手册](production-runbook.md)、[验证汇总](verification-summary.md)、[交接](handoff.md)。
