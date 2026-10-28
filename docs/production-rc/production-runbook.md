# RC运行手册初稿

当前仅用于隔离准备；真实应用、目标机器、SLO和生产回退对象尚未确定。正式入口为仓库Python工具[production_rc.py](../../deploy/production_rc.py)，不是Ku内置子命令。Linux包/配置SHA及安装收据仍pending；下列命令是已核源码的调用契约，未填入最终manifest实际值前不能执行部署。

## 执行入口

现有且已核源码的部署工具为[manage.py](../../deploy/service/manage.py)，支持`check/render/run/start/status/stop/roll/rollback`。以下命令在仓库根目录执行，`RC_CONFIG`必须先指向本轮manifest绑定的私有配置，不能直接使用未填写的example或生产配置：

```sh
python3 deploy/service/manage.py check "$RC_CONFIG"
python3 deploy/service/manage.py status "$RC_CONFIG"
```

已有Ku正常编译入口为`ku build --backend c --profile release … -o …`，应用路径与产物SHA必须来自manifest，不使用旧临时路径。直接参数和原监督行为见[现有部署说明](../../deploy/service/README.md)。

以下变量均须由实际构建记录/manifest填入，不是已有生产配置。prepare当前限定Linux ARM64 native部署包，不包含完整Rust runner SDK、VSIX或TLS pack；核对源码、CLI/app SHA、提交一致及许可资料，要求打包的部署源已提交。

```sh
python3 deploy/production_rc.py prepare \
  --ku "$RC_KU" --app "$RC_APP" --source "$RC_APP_SOURCE" \
  --config "$RC_CONFIG_TEMPLATE" --build-record "$RC_BUILD_RECORD" \
  --cargo-metadata "$RC_CARGO_METADATA" --rust-sysroot "$RC_RUST_SYSROOT" \
  --runbook "$RC_RUNBOOK" --output "$RC_OUTPUT" --repo "$RC_REPO" \
  --name "$RC_NAME" --candidate-commit "$RC_COMMIT" \
  --ku-sha256 "$RC_KU_SHA256" --app-sha256 "$RC_APP_SHA256"
RC_STATE="$RC_OUTPUT"
python3 deploy/production_rc.py status --state "$RC_STATE"
python3 deploy/production_rc.py resume --state "$RC_STATE"
python3 deploy/production_rc.py preflight --state "$RC_STATE" --install-root "$RC_INSTALL_ROOT"
```

prepare输出必须新建；preflight只在原生Linux ARM64执行，拒绝覆盖已有安装目录，校验归档/动态依赖并实际运行installed CLI/version/check/native hello。失败留存；修正安装原因后才可另建目录并用`--correction-reason`记录。status只核保存证据，不证明目标仍存活；resume会保存重新派生状态，不重跑业务。

```sh
python3 deploy/production_rc.py verify --state "$RC_STATE" --receipt "$RC_EXECUTION_RECEIPT"
python3 deploy/production_rc.py rehearse --state "$RC_STATE" --receipt "$RC_REHEARSAL_RECEIPT"
```

两命令核验候选/config binding和原证据SHA；receipt必须来自实际运行，rehearse只接收lifecycle/rollback。不能手写passed替代实测，失败冻结候选不能靠再选好窗改通过。真实target_preflight另需`--profile`绑定应用、目标、SLO和回退对象，目前均缺失。

## 每次有状态操作前

核对候选CLI/应用/配置SHA、工作目录、目标服务用户、独有目录和实际端口。原PID加启动身份/argv或持有的Popen句柄确定归属；不得只按进程名杀进程。接续中断状态时先读当前门禁、部署锁及实际进程，不抢不明锁、不重建未知数据库对象、不重放业务写入。

确认上一执行的HTTP连接、归属进程、PG会话及预算归还；不把请求完成当连接已关闭。凭据由权限受控文件注入，管理口仅loopback/受控网络，token不进入普通日志、状态或指标标签。新启用的OS限制必须覆盖预期完整进程树，supervisor域外成本另列。

## 日常观察与告警

分别记录代理、supervisor、launcher、native、PG、driver/观察器的CPU、RSS、FD、线程及重启；同PID只记一次，RSS和cgroup不能重复相加。端到端延迟包含排队/连接/写回，service内部值另列；driver生成无效时不能据其认证服务容量。

按业务确认的分母记录计划、发送、接纳、正确、错误、拒绝、取消、未确定和未发送。正确性错误/重复副作用立即隔离候选；失联、OOM、重启风暴或持续超过事前预算/SLO停止扩量。阈值和最小样本量未确认前不启用自动生产切流。无告警不能替代采集完整性；私有Task/DB预算指标不可见时写unknown，不伪造0。新PG指标在Linux尚待实测，不能因monitor能解析字段就写已验证。

[monitor.py](../../deploy/service/monitor.py)是前台只观察工具，只输出告警，不自动摘流/重启；固定PID/cgroup/输出上界、按PID去重、状态不可用为unknown。下面是12次有限观察，日志至多3个各1MiB，输出目录须新建；不是持续灰度观察。

```sh
python3 deploy/service/monitor.py --config "$RC_CONFIG" --output-dir "$RC_MONITOR_DIR" \
  --state "$RC_STATE" --interval 5 --samples 12 --max-log-bytes 1048576 --max-files 3
```

只有显式`--samples 0`才运行至中断；持续观察还需实际托管、live PID/start/argv及新鲜observer.json，不能据文件存在或有限采样称后台持续运行。策略中的FD增长等阈值是草案，业务成功率/延迟需应用或负载证据，不能由低频进程采样推算。

健康采用实际runtime startup/ready/live及真实只读业务请求；`/health`只证明HTTP样例可响应。不能由代理固定200代替应用健康，不因数据库短故障同时反复重启所有实例。日志使用已有有界设施并脱敏，慢接收端不得阻塞请求。

## 摘流、停止与恢复

1. 保存候选、配置、异常时间线及请求ID，停止扩量；核对当前操作归属。
2. 在入口摘除候选，确认不再分配新请求，覆盖keep-alive和传播窗口；再停止接纳。
3. 已接纳请求按原业务期限排空，随后关闭PG/句柄。管理重试只重试幂等控制动作，沿用同一绝对截止点，不续期、不重放HTTP业务或SQL。
4. 期限内无法回收时保留fail-stop/实际退出码及预算，不能假报清理成功。外部监督有限退避，强度上限后保持失败并告警，避免两套native重启器。
5. 新代必须有真实ready、入口UP和新业务正确响应；`systemctl active`或进程存在不够。

`_Exit(80)`为进程退出策略，不是Result恢复。Linux已复现停止次序缺陷：旧KillMode=control-group使已接纳请求0B结束，尽管unit Result=success；mixed同条件完成200。当前stock与RC模板已改mixed，TERM先给supervisor排空，最终超时仍由systemd清整个cgroup。该对照不等于最终安装单元、有限重启和OS预算已验收，必须绑定修正后模板/配置/包SHA。未实现通知协议不启用Watchdog。

[renderer](../../deploy/service/render-systemd.py)只渲染，不安装/enable/start，必须在实际Linux账户和文件权限下使用；拒绝root服务用户、未填写模板和与whole-unit限制冲突的Ku子域硬限额。预算变量由本轮范围确定，不能照搬256MiB或2CPU作为生产承诺。

```sh
python3 deploy/service/render-systemd.py --config "$RC_CONFIG" \
  --output "$RC_UNIT_DIR" --user "$RC_USER" --unit-prefix "$RC_UNIT_PREFIX" \
  --manage "$RC_INSTALLED_MANAGE" \
  --memory-max-bytes "$RC_MEMORY_BYTES" --cpu-quota-percent "$RC_CPU_PERCENT" \
  --tasks-max "$RC_TASKS_MAX" --nofile "$RC_NOFILE" \
  --timeout-stop-seconds "$RC_STOP_SECONDS"
systemd-analyze verify "$RC_UNIT_DIR/$RC_UNIT_NAME"
```

输出目录必须新建、服务账户已有且非root；token归属该账户且拒绝group/other读。停止预算至少覆盖原drain加5s清理，不修改业务deadline。安装及start/stop由协调者只对本轮绑定单元执行，实际读取MainPID、cgroup、限额、请求结果和退出状态；源码模板渲染成功不可直接置install/lifecycle/resource通过。

## 回退与写入对账

Release样例的previous/new只是同源版本标记，不是生产稳定后端。真实新服务没有旧后端时，异常动作应为关闭/降级；在恢复对象未确定前禁止对外宣称可切回旧生产。现有工具的canary/rollback必须同时具备真实profile、目标machine-id、完整target门禁、明确动作授权及action审批SHA；canary目前只支持整个slot替换，不实现百分比流量。rollback仅支持另行绑定的same-CLI旧应用，不支持凭空退回旧CLI或不存在的后端。当前输入缺失，不运行这两命令；新服务关闭/降级需按已约定实例控制另行实测，不能把sample版本标记切换写成生产回退。

应用退回不能撤销已提交数据。全部已尝试写入ID按成功、失败、结果未知分别与最终数据库核对；超时、断连或发出cancel都不证明回滚。确认丢失样例已观察到HTTP报unknown但实际行存在，不自动重发。生产schema/data不得用于破坏性试验。

## 中断与清理

先停负载和观察器，再逐层处理本轮实例/代理/PG；一个失败不得跳过其余清理。核对进程树、监听、PG会话与域归零，只删除有明确归属的临时文件；未确认项保留标识和错误，门禁不通过。独有失败证据、源码历史与有用缓存保留，临时探针/测试副本按约定清理。最终Linux包、安装、PG指标和清理结果由实际收据补齐，工具源码存在不等于这些门禁通过。
