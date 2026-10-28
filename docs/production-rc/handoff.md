# Ku v0.0.18 RC交接初稿

当前`in_progress`，本轮交付目标`rc_prepared`；尚未达到该状态。用户明确没有真实业务和目标机器，application/deployment target/business SLO为null/tbd。不能写ready_for_canary、生产运行或limited_production_accepted。

## 已有可复用成果

新Mac正常Release CLI31b6…已构建；Release代理生命周期和私有PG同步/异步均实际通过并清理。新CI-232-final.json已核三平台完整/安装PG成功，31 SUCCESS/9 SKIPPED，旧运行中快照不再作当前pending。Linux旧正常CLI9428…/Ccf59…/ELF455…身份可定位，源码等价证据和缓存保留，不能把它们伪装成本轮新安装。

旧正式7对、长测、物理取消与scoped M证据照常复用，旧五认证false、长测有效20ms失败和容量unknown不改。Rust比较尚未通过不阻止独立RC准备；新内部样例100rps/250ms又不构成真实生产承诺。

## 当前独立可做项

由协调者串行安排Linux ARM64隔离安装、依赖和同用户启动、systemd/资源/监控验证、部署及关闭/回退预演；Mac combined 30s已实际通过：3000/3000正确、2968正确复用、601笔写入逐ID对账、PG会话归零及清理；formal未执行，不认证持续运行或生产SLO。systemd旧control-group已复现丢失admitted请求，mixed同条件200完成；当前模板已改mixed，最终包与安装单元绑定复验仍待收据。新Linux包、新PG指标/监控及安装预演仍pending。

现有RC工具为[production_rc.py](../../deploy/production_rc.py)，status/resume检查保存状态；preflight会实际安装并运行hello，verify/rehearse仅核验并附加实际收据，不自动重放动态操作。调用参数和边界见runbook，最终产物/配置SHA以manifest为准。

现有部署入口：[deploy/service/manage.py](../../deploy/service/manage.py)，操作说明：[production-runbook.md](production-runbook.md)。真实执行前先读最终门禁、manifest和当前进程，不因交接重复安装、创建数据库对象或重放写入。

## 外部条件与后续决策

真实部署前一次补齐：程序/业务、目标机器和访问方式、数据库及副作用、预计流量与SLO、回退或关闭方案、切流授权。没有稳定后端时使用新服务关闭/降级，现有sample previous/new仅版本标记演练。数据结果unknown必须对账，不当回滚、不盲重试。

完成其余RC准备后可交付`rc_prepared`并保留外部缺口；没有真实部署和持续观察就不进入canary。当前没有实际后台灰度观察器或自动后续任务，不能宣称会自行完成。

不改JSON旧默认、暂停的Value/AST、生产schema/data；不公开发版、合并、部署网站或购买。临时探针与重复产物按归属清理，用户源码、原失败和独有证据保留。

详情：[范围](production-profile.md)、[证据复用](evidence-reuse.md)、[验证汇总](verification-summary.md)。本机新证据在`/Users/mac/Documents/Ku/验收/生产前测试_2026-10-28`，旧性能证据在`/Users/mac/Documents/Ku/实施/性能稳定性收口_2026-10-26`；大raw仍留原机，复用索引，不为打包重测。
