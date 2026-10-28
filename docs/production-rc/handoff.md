# Ku v0.0.18-rc.1 交接

已达到`rc_prepared`，可用于Linux ARM64隔离native部署样例的安装与受控预演。真实应用、目标机器、业务SLO、生产回退对象及切流授权均未提供；没有canary部署或持续生产观察，不是ready_for_canary或limited_production_accepted。

## 从哪里接续

本机证据根：`/Users/mac/Documents/Ku/验收/生产前测试_2026-10-28`。包内相对state为`RC-v0.0.18-rc.1/`；[门禁](/Users/mac/Documents/Ku/验收/生产前测试_2026-10-28/RC-v0.0.18-rc.1/production-gate.json)七项RC检查全true，[manifest](/Users/mac/Documents/Ku/验收/生产前测试_2026-10-28/RC-v0.0.18-rc.1/release-manifest.json)绑定CLI6006505d…（源44f9733…）、包提交9cc43c06…、应用a2cda9d…、归档1e6b164…。原机安装前缀`/opt/ku-production-rc-20261028/install`，预演配置为独有runtime/cluster.json，不是可直接投产的业务配置。

在仓库根目录只读检查保存状态：

```sh
python3 deploy/production_rc.py status --state '/Users/mac/Documents/Ku/验收/生产前测试_2026-10-28/RC-v0.0.18-rc.1'
```

此命令不确认原机仍live、不访问生产，也不重新安装/压测。若证据根被移动，只改state实际路径并先核manifest SHA。操作语义、Linux安装和监督命令见[runbook](production-runbook.md)。

## 实际完成与边界

- Linux正常320次全正确，64笔写入逐ID一致；摘流后旧连接/fresh20次不再流向A，SIGTERM完成已接纳hold200。
- native恢复0.302416s、样例回退0.608668s、恢复candidate0.438265s；PG重启新读0.638395s。故障中的unknown/拒绝保留，无盲重试。previous仅样例标记，不是旧生产后端。
- 新包实际安装/依赖/normal CLI入口、sync/async PG及新指标、systemd整单元限制与清理通过；48个监控不可用项有确切范围。12次/约55s采集不等于长期观察。
- 原control-group停止丢请求已复现并以mixed修正；两次proxy FD setup失败保留，仅proxy8192→16384，Ku仍8192。
- Mac辅助链路和232历史CI可按范围复用；新RC CI另见交付目录 `CI-RC-current.json`。旧五性能认证false、容量unknown、20ms有效失败照常保留。

归档中MD为准备初稿；外部这五份最终MD及最终门禁说明实际结果，未更改原归档SHA。大原件留原机/已有索引，不为重新打包重测。

## 唯一外部决策清单

后续一次补齐：实际程序与业务、目标机器/访问方式、数据库和副作用、流量与SLO、回退或关闭方案、真实切流授权。新服务没有稳定后端时采用关闭/降级，不虚构旧生产。target、SLO与预算冻结并通过目标验收后才评估ready_for_canary；没有实际流量和观察记录不能置生产通过。当前没有自动后台观察任务。

JSON旧默认、Value/AST暂停、用户源码与独有失败保留；不公开发布/合并/改DNS/部署网站/购买，不触生产schema/data。不要因换轮次重开旧Rust/容量/30分钟矩阵；输入变更影响哪项，只补对应检查。详情见[范围](production-profile.md)、[复用](evidence-reuse.md)、[验证汇总](verification-summary.md)。
