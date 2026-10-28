# 证据复用与补齐初稿

旧性能门禁与本轮生产RC门禁独立。旧结果保持`not_accepted`、五项比较认证全false；它不自动阻止RC准备，也不能被新样例好窗覆盖。

本机证据根目录：

- 旧性能证据：`/Users/mac/Documents/Ku/实施/性能稳定性收口_2026-10-26`。
- 本轮隔离验证：`/Users/mac/Documents/Ku/验收/生产前测试_2026-10-28`。

| 证据 | 采用方式 | 范围与需要补齐的内容 |
|---|---|---|
| 232提交最新三平台完整CI、实际安装CLI→PG | `reuse` | 本轮CI-232-final.json已核最新完成，31项SUCCESS/9项SKIPPED；[run40627240228](https://github.com/fengyanweb/ku/actions/runs/40627240228)及[run40627244831](https://github.com/fengyanweb/ku/actions/runs/40627244831)分别保留作业状态。新收据的Windows/Linux/macOS完整workspace和三个实际安装PG作业均SUCCESS；旧快照仍作为历史保留，不再据其运行中状态判当前pending |
| cbb定向四项、d07完整十二项及源码PG生命周期 | `reuse` | 按未变编译器/Cargo/build身份复用；跳过项不计通过。不能再笼统写数据库生命周期全skipped；新232安装PG成功也不能替代Linux ARM64本地安装预演 |
| 新Mac Release CLI构建 | `reuse` | 本轮`candidate-build-identity.json`绑定31b6…、正常Release构建和工具链；不要求因文档变化重编 |
| 新Mac Release代理生命周期 | `reuse` | `release-service.json` passed且cleanup_errors为空，退出替代280.614ms、滚动911.115ms、样例回退627.479ms。`complete_runtime_acceptance=false`；原fixture未记录每份native二进制SHA，只作功能链证据 |
| 新Mac同步/异步私有PG | `reuse` | `pg-lifecycle.sync.json`、`pg-lifecycle.async.json`均passed，退出后连接0、cleanup_errors为空。参数/中文/emoji/null、10000行、断连、池压力、确认丢失均有结果；确认丢失实际写入1行且不重放。异步未知结果不能写成取消或回滚已确认 |
| 正常CLI→C→Linux ARM64 ELF | `reuse`身份；`verify`安装缺口 | 旧9428…/cf59…/455…固定绑定。主样例选Linux ARM64后，仍需本轮真实安装、依赖、服务用户、启动及卸载/关闭收据 |
| 215个物理RST及恢复见证 | `reuse` | 204取消、5先完成、6无Task；恢复上界约1.835s/0.609s。限旧六线程样例，不证明任意FFI/PG可撤销或新部署RTO |
| M内存机制 | `reuse` | 当前已观察增长的合理高水位解释scoped关闭；normal精确分桶未知，不代表全项目无泄漏，不反复trim或重测同一事实 |
| 原7对与1800s长测 | `reuse`并保留失败 | phase4生成有效、短p99=25.761695ms超过20ms；phase1生成无效且短p99=96.375543ms。五比较false、容量未知；过载错误与未知结果不抹去，不把新业务SLO改写成原任务通过 |
| Mac combined代理→native→PG | `reuse`本轮实测 | combined-short.json passed：3000/3000正确、2968正确复用，e2e p99上界6.8ms、gen上界5.0ms；601行写入对账，native退出/版本标记切换/PG新读恢复、会话归零及清理通过。production_ready=false；formal未执行，不自动重开30分钟 |
| systemd停止次序 | `reuse`定向失败及修正对照 | systemd-stop-ordering.json：旧control-group已接纳请求返回0B；mixed同条件109B完整200、587.223ms停止，cleanup_errors=[]。当前模板已mixed；不能用unit Result=success掩盖旧请求失败，最终包和单元的绑定复验待补 |
| Linux安装/整链路监控/回退预演 | `new verification pending` | production_rc、monitor和renderer源码已可核对；新包、安装、新PG指标及完整运行结果待实测，静态工具不算通过 |
| 真实业务/目标预检/灰度观察 | `external inputs missing` | 用户目前没有业务和机器；rc_prepared可在其余准备完成后交付，ready_for_canary及limited生产状态不可置通过 |

只有源码、配置、产物、依赖或部署环境改变且影响结论时，才补对应检查。修改文档不触发重编；已有原件继续保留原机，以相对索引和SHA引用，不重复导出大trace或重开Rust/容量矩阵。

旧原件位置见旧证据根目录的`RAW_LOCATIONS.md`与`raw-evidence-location-index.json`。正式引用以原summary、选窗身份和before/after SHA为准；摘要不是重新采样的理由。
