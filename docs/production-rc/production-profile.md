# Ku v0.0.18 RC 范围初稿

当前为`in_progress`，本轮目标是`rc_prepared`。用户确认没有真实业务和目标机器；本文件不能据此宣布`ready_for_canary`或生产验收通过。RC安装、Linux监督与监控、部署/关闭预演仍须有实际收据后才能关闭准备项。

## 范围与缺口

| 字段 | 当前事实 | 放行前需要 |
|---|---|---|
| 真实应用、业务入口 | `null`；仅使用仓库HTTP/PG样例 | 确定程序、路由、依赖及写入语义 |
| 主样例平台 | 拟选Linux ARM64；是本地隔离预演目标 | 固定实际内核、glibc、libpq/TLS依赖和服务用户 |
| 真实部署目标、域名、入口 | `null` | 目标机器/访问方式、已有入口与授权 |
| 业务负载与SLO | `null/tbd` | 峰值、并发、连接模式、请求大小、成功率、延迟、RTO |
| 数据风险 | 真实业务未知；样例只用独有临时PG | 明确只读/幂等/非幂等及结果未知对账方式 |
| 生产回退对象 | `null`；不存在已验证的旧生产后端 | 有旧服务则绑定产物/配置；新服务则约定关闭或降级 |
| 授权 | 本地准备、隔离验证与预演 | 真实切流、DNS、公开发布、合并和购买不在本次自动授权内 |

内部样例拟采用100请求/秒、70%读/20%新ID写/10%健康、端到端p99≤250ms。这只是独立的内部演练契约，已通过Mac 30s combined链路，不是生产负载上限或业务SLO。任务书的普通API p99≤100ms同样只是待确认讨论草案。原20ms失败继续保留。

## 候选身份

- 说明基点：`23229134d150cb5dcadab870f9ab2f5c6b81caac`；生产代码基点`cbb14afcff9b864bd0c41953f53684f0fa8a9456`。
- 新Mac arm64 Release CLI：`31b6b9ce45bff064912aff2a1646d3fc73fccb86ce34c51db28a1707111a5e84`，来源为本轮`candidate-build-identity.json`。缓存目录名含1.89，实际编译器为rustc1.99.0，不能用目录名推断工具链。
- 旧Linux ARM64正常CLI：`94283691e87f3d4133d970b67fc1e79460e9d9c3e824fb6b9a24507e023be2d6`；原C`cf59ceaa…`、ELF`455b47c7…`。它们是旧样例身份，不能冒称本轮新安装或新应用产物。
- 本轮RC提交、应用二进制、配置及依赖SHA以协调者最终release manifest为准；本稿不把临时Mac CLI路径当永久安装位置。

Mac实际环境为Darwin25.5.0 arm64/8GiB/8逻辑CPU，HAProxy3.4.7与PostgreSQL17.11。独立Release代理、同步/异步PG及Mac 30s combined均已通过。combined为3000/3000正确、正确连接复用2968次，端到端p99上界6.8ms、发生器上界5.0ms；601笔成功写入逐ID/UTF8对账，PG会话退出归零。它不是持续运行或生产SLO通过。

Linux已定向复现stock systemd的KillMode=control-group先终止native、丢失已接纳请求；同条件mixed返回完整200。当前stock与RC模板已改mixed，但最终候选单元/安装包仍须绑定修正后的SHA并复验。Linux新包、安装、OS预算、监控及新PG指标结果待补。Mac没有本轮逐服务OS CPU/内存硬限额，采样RSS不等同硬限制。

## 运行与资源边界

参考链路为既有入口/HAProxy→native应用→实际依赖，Ku service负责进程监督，不是每请求代理。双活动实例和滚动备用位仍共享宿主，不能声称节点级高可用；本轮无公网TLS验证或第二主机。应用内部预算、supervisor、代理、PG和观察器分别核算，按PID去重，RSS含共享页不能冒充PSS。

当前JSON旧默认、暂停的通用Value复制/释放与AST分析保持；不改生产schema/data，不加数据库盲重试。后续真实应用确定后，只补受影响的检查，不能把样例配置直接套到生产。

参见[复用表](evidence-reuse.md)、[操作手册](production-runbook.md)、[验证汇总](verification-summary.md)及[交接](handoff.md)。
