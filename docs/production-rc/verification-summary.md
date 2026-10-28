# RC验证汇总初稿

当前状态`in_progress`，目标`rc_prepared`。本稿为已完成证据的只读汇总，未执行新负载、VM、构建或部署；缺项不默认通过。真实生产业务和目标机器缺失，未进入canary。

| 项目 | 当前结果 | 证据与限制 |
|---|---|---|
| Mac arm64候选CLI | 已构建 | 新验收目录`candidate-build-identity.json`，SHA31b6…；正常Release用户入口，未替换已安装6288… CLI |
| 独立Release代理生命周期 | 通过 | `release-service.json`，正常代理、单native退出替代、另实例10/10、样例滚动/回退通过，cleanup_errors=[]；complete_runtime_acceptance=false，不能提升为整链路/生产回退通过 |
| 私有PG同步lane | 通过 | `pg-lifecycle.sync.json`、`PG-concise-results.json`；Unicode/null/参数/10000行、确认丢失、断连、压力及正常退出，退出后PG连接0 |
| 私有PG异步lane | 通过 | `pg-lifecycle.async.json`；同类结果、unknown和预算归还分开，退出后连接0；未知不代表数据库回滚确认 |
| 最新232源码/安装CI | 通过并复用 | CI-232-final.json，31 SUCCESS/9 SKIPPED；三平台完整workspace和实际安装CLI→PG均成功。旧运行中快照不作为当前状态，skipped仍不计通过 |
| Linux ARM64候选身份 | 可复用旧绑定 | 9428… CLI→cf59… C→455… ELF；不是本轮安装收据 |
| Linux实际安装、依赖和同用户启动 | 待验证 | 目标为隔离样例，协调者串行补齐本轮包与安装验证；定向停止对照不等于安装通过 |
| systemd停止次序 | 旧配置失败；mixed定向对照通过 | systemd-stop-ordering.json：control-group在已接纳标记后停止，返回0B；mixed同条件返回HTTP200/109B并完成，cleanup_errors=[]。当前stock和RC模板已mixed；新包绑定、安装单元、有限重启及完整OS预算仍待验证 |
| 监控、告警与关闭/回退预演 | 待验证 | 需实际采集、端点、有限日志与动作结果；无私有指标时unknown |
| Mac combined 30s | 通过，production_ready=false | combined-short.json，3000正确/3000终态、零shed，2968正确复用；e2e p99上界6.8ms、max9.446083ms；gen上界5.0ms。601笔成功写入逐ID/UTF8对账、无重复，PG退出会话0、cleanup_errors=[]、临时目录已清理 |
| combined故障与恢复 | 指定Mac样例通过 | native退出后435.756ms新代正确读；标记版本滚动/回退通过；PG停止后返回execution_unknown、重启后原绝对5s内新读恢复约0.647s，不重放失败请求 |
| formal及Linux新包/PG指标 | 未执行/待验证 | 不把30s好窗或旧指标拼为持续运行、Linux实际安装或生产SLO通过 |
| 真实应用正确性/业务SLO/目标预检 | 未定义/未执行 | application、target、business SLO均null/tbd；100rps/250ms只限内部样例 |
| 真流量canary及观察 | 未执行 | 无机器、真实业务及切流条件；没有后台观察任务，不假称自动完成 |

## 旧性能结论保持

旧报告`final-gate.json`为`not_accepted`，五比较认证false、容量unknown。原phase4有效短p99=25.761695ms超过20ms；phase1生成无效且短p99=96.375543ms，恢复成功不能抵消。M只按已观察高水位解释scoped关闭，不宣称全项目无泄漏。最新控制无复现不证明宿主根因或所有本地路径穷尽。

本轮可采用独立、明确的新业务SLO，但任务书100ms草案尚未确认；不能把旧20ms失败改判。新样例成功也不认证Rust优势、多主机容灾、任意业务负载或任意平台。

## 收口条件

RC产物/安装、配置、监督、监控、隔离预演与关闭/回退能力均有身份对应实际收据后，协调者才能将新门禁置`rc_prepared`。若只剩真实应用/机器缺口，集中交付；真实应用、目标、SLO、预算和授权落实并通过目标验收后才评估`ready_for_canary`。灰度实际部署、观察完成另行记录，不能从RC准备跳到limited生产通过。

本轮无生产数据库migration、无公开部署/发版/合并、无购买；JSON旧默认和两个暂停项保持。最终链接、工具命令、manifest和门禁一致性随实际收据补核。
