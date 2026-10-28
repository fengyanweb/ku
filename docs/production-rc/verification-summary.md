# RC验证结果

[最终production-gate.json](/Users/mac/Documents/Ku/验收/生产前测试_2026-10-28/RC-v0.0.18-rc.1/production-gate.json)为`rc_prepared`，七项RC检查identity/install/business/lifecycle/resource_limits/observability/rollback全true。真实生产检查仅identity_bound=true，其余包括业务SLO、目标预检、灰度授权/部署/观察均null；两层门禁不可混同。

Linux实测以6006505d… CLI（源码44f9733…）、9cc43c06…包、a2cda9d…应用及1e6b164…归档绑定，完整SHA见[范围](production-profile.md)和release manifest。以下结果均为已完成样例，不是生产认证。

| 检查 | 实际结果 | 原证据（相对新验收目录） |
|---|---|---|
| 包安装与正常用户入口 | 通过：干净目录解包/依赖、installed version/check/native-build/run hello；非root业务启动 | `RC-v0.0.18-rc.1/installation-check.json`、`Linux-evidence/install-preflight.log` |
| 正常业务与写入 | 320/320正确、0错误；64次新ID写入数量、唯一性及逐ID一致。8客户端各40次、每次间隔0.1s | `Linux-evidence/linux-installed-rehearsal.json`；小样本延迟仅原始记录，不认证性能/SLO/容量 |
| 摘流与SIGTERM | A置MAINT后旧keep-alive/fresh共20次正确且A请求数不增；hold在SIGTERM后完整200/held-ready，整单元停止、空cgroup | 同上；Stop约0.533s，仍沿原16s停止预算 |
| 原停止缺陷及修正 | stock control-group丢已接纳请求0B；mixed同条件完整200，最终RC预演再通过 | `systemd-stop-ordering.json`及上述rehearsal；旧失败不抹去 |
| native退出恢复 | 新代恢复0.302416s，不重放写入，database_outcome仍unknown | `Linux-evidence/linux-installed-rehearsal.json` |
| 样例回退与恢复 | rollback0.608668s响应rc-previous；恢复candidate0.438265s | 同上，previous仅样例，不是生产稳定后端；不证明数据回滚 |
| PG故障与新读恢复 | 压力12次为8 execution_unknown+4 acquire_timeout；撤压后新读正确。PG重启新读0.638395s恢复，期间connect_error/unknown及解析失败记录保留，无写入重放 | 同上；HTTP200包着error不算业务成功 |
| 安装CLI同步/异步PG | 两lane通过：字段/中文/null/10000行、确认丢失实际写入1行、断连/压力、新请求恢复、退出会话0，cleanup_errors=[] | `Linux-evidence/installed-pg-lifecycle.json`及各lane；sync确认取消5，async仍unknown7/确认取消0，不宣称回滚 |
| 新PG与Task指标 | 两活动实例metrics有效；PG active/waiter、坏连接丢弃、Task/buffer及队列有固定字段；故障后活量归还，停止后PG会话0 | rehearsal和installed PG原JSON；瞬时值不推出长期无泄漏 |
| OS资源域 | Ku每单元256MiB/200%/64tasks/FD8192；proxy64MiB/100%/16tasks/FD16384；PG256MiB/100%/48tasks，实际readback与最终inactive/空域 | rehearsal的render、units与unit_cleanup；整机和外部观察成本另计 |
| 有界监控 | 12×5s、0gap/0dropped、219307B、约55s；48 unavailable=spare计划缺席24+root sudo wrapper FD拒读24，核心服务FD/metrics可见 | `Linux-evidence/monitor/run.json`、`metrics.0.jsonl`；有限前台采集，不是长期/灰度观察 |
| setup失败 | 两次失败保留；实际原因proxy maxconn4096需8223FD而配置8192，仅proxy改16384 | `Linux-evidence/setup-failure-analysis.json`及两份失败JSON/log，无timeout放宽 |
| Mac辅助链路 | Release代理、同步/异步PG、combined30s均通过；combined3000正确、2968复用、601写入对账及会话/清理归零 | 新验收目录Mac原JSON；31b6…是不同CLI，不能当Linux新身份 |
| CI | 历史232三平台完整/实际安装PG成功可按输入复用；新增提交的 CI 状态单独保存为交付目录 `CI-RC-current.json`，历史成功不替代新检查 | `CI-232-final.json`为31 SUCCESS/9 SKIPPED，跳过不计通过 |

## 保留的未验收项

真实应用、目标机器、业务SLO、生产回退对象与切流授权未提供；canary未部署，观察未开始。100rps/250ms只是Mac内部样例，100ms只是任务书讨论草案。Linux小样本及有限监控不认证长期可靠性、生产容量、TLS公网入口或节点级高可用。

旧性能门禁仍not_accepted、五false、容量unknown。phase4有效短p99=25.761695ms超过20ms，phase1生成无效且96.375543ms；M只scoped关闭，不能声称全项目无泄漏。新好窗/功能预演不覆盖旧失败或证明宿主根因。

## 交付与收尾

RC state中的安装及五类真实收据SHA已匹配，归档保持原1e6b…；其中MD是准备快照。本目录最终MD与外部门禁补充实测，不回写归档。预演归属单元全部inactive、PG会话0、cleanup_errors=[]；独有runtime目录的保留/清理由协调者单独留收据，不据文档推断删除。

无生产数据库migration、公开发版、合并、网站部署或购买；JSON旧默认及Value/AST暂停项保持。真正业务条件落实后，按固定目标SLO/预算与授权补目标验收，再评估ready_for_canary，不凭RC直接生产放行。
