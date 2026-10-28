# 证据复用与本轮实测

当前RC门禁`rc_prepared`与旧性能门禁独立；旧`not_accepted`、五比较false及有效失败不改。新证据根目录为`/Users/mac/Documents/Ku/验收/生产前测试_2026-10-28`，以下路径均相对该目录；旧性能证据位于`/Users/mac/Documents/Ku/实施/性能稳定性收口_2026-10-26`。

| 证据 | 采用方式 | 已确认范围与限制 |
|---|---|---|
| 232三平台完整CI/实际安装CLI→PG | reuse未受影响范围 | `CI-232-final.json`为31 SUCCESS/9 SKIPPED，Windows/Linux/macOS完整workspace与三个installed PG成功。[40627240228](https://github.com/fengyanweb/ku/actions/runs/40627240228)、[40627244831](https://github.com/fengyanweb/ku/actions/runs/40627244831)分别保存。旧运行中/跳过快照不代表当前全缺失；新增提交CI独立记录在`CI-RC-current.json`，不宣称这些作业覆盖新增指标/工具 |
| cbb/d07原CI、物理取消及M | reuse原身份 | 定向/完整源码回归、215 RST、约1.835s/0.609s恢复和scoped高水位解释保留；不证明任意FFI/PG可撤销、全项目无泄漏或新产物性能 |
| 新Mac CLI及独立Release代理 | reuse辅助证据 | `candidate-build-identity.json`、`release-service.json`绑定31b6…；代理正常/退出替代/标记版本切换通过、清理空。complete_runtime_acceptance=false，不能当新Linux身份或生产后端回退 |
| Mac同步/异步私有PG | reuse辅助证据 | `PG-concise-results.json`及两lane通过；中文/null/10000行、断连/压力、退出会话归零。确认丢失实际行存在且未重放，异步unknown不等于已确认回滚 |
| Mac combined 30s | reuse本轮独立链路 | `combined-short.json`：3000正确/3000终态、2968正确连接复用、601笔写入逐ID/UTF8对账；gen p99上界5.0ms、e2e上界6.8ms；新读恢复及清理通过。production_ready=false，不替代正式长测或真实业务 |
| 新Linux包与安装链 | 新执行已通过 | `RC-v0.0.18-rc.1/release-manifest.json`、`Linux-evidence/install-preflight.log`绑定6006…/a2cd…/归档1e6b…；实际解包、ldd、installed CLI version/check/native-build/run hello及非root业务启动通过 |
| systemd停止次序缺陷 | 保留失败与修正 | `systemd-stop-ordering.json`：旧control-group丢admitted请求，mixed同条件200。修正已进入本轮RC；`Linux-evidence/linux-installed-rehearsal.json`再验证完整hold响应及单元清理，不能以unit success掩盖旧业务失败 |
| Linux业务、生命周期与回退 | 新执行已通过 | 同一预演：320正确、64写入逐ID相同；MAINT后旧连接/fresh共20次不再发A；SIGTERM完成hold200；native恢复0.302416s，样例rollback0.608668s、恢复candidate0.438265s。previous/new只是样例版本标记 |
| Linux安装CLI→PG两lane/新指标 | 新执行已通过 | `Linux-evidence/installed-pg-lifecycle.json`，sync/async通过，实际metrics有效、坏连接丢弃、退出连接0。预演压力8 unknown+4 acquire；独立sync压力4 unknown+8 acquire、async为4 unknown+6 acquire+2 pool_busy，分别保存，不合并为成功请求 |
| Linux资源与有界监控 | 新执行已通过，范围有限 | `Linux-evidence/monitor/run.json`及metrics.0.jsonl：12×5s、0gap/0dropped、219307B、约55s；48项不可用含计划spare缺席24和root sudo wrapper FD拒读24。核心服务metrics/FD可见，非长期观察 |
| 两次预演setup失败 | 原样保留 | `Linux-evidence/setup-failure-analysis.json`：HAProxy maxconn4096需8223FD，原proxy8192不足；仅proxy改16384，Ku仍8192，无延长timeout。先疑startup race的原失败也保留 |
| 原7对/1800s性能结果 | reuse并保留未通过 | 原phase4生成有效、短p99=25.761695ms超过20ms；phase1生成无效且96.375543ms。五比较false、容量unknown；恢复成功、新RC功能通过均不抵消 |
| 真实业务/目标/灰度观察 | 外部输入缺失 | application、target、SLO均null；没有真实切流、已验证生产后端或长期观察，不能放行ready_for_canary |

[最终门禁](/Users/mac/Documents/Ku/验收/生产前测试_2026-10-28/RC-v0.0.18-rc.1/production-gate.json)已绑定安装和五类实际收据，六份收据SHA本轮逐一匹配。包内准备时MD保持原样；外部最终MD及门禁补充实测结论，不重打原包或更改1e6b…SHA。

仅当输入变化影响结论时补对应检查。没有新问题不重复负载等偶然好窗，不重开Rust七对/容量/原30分钟。大原件留原机，使用原目录的RAW_LOCATIONS与SHA索引；不能因包未收大raw要求重新测量。
