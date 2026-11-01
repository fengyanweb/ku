# PG 调用链采集 / PG callchain capture

仅复用 run 41280471018 的普通5b ELF（234300…）与默认发行版 libpq；完整生成C（830536…）已与当前f85默认逐字节对齐。固定200请求/秒、helpers/text/async/int、原SQL和驱动，三窗15/60/15秒，每窗预热10秒。仅显式db-gpr改为cpu-clock 999Hz、16KiB用户栈和原33个ARM64通用寄存器；原始记录96MiB、日志1MiB有界。无产品重编译、无库候选、无自动补窗。

Only the immutable ordinary5b ELF from run41280471018 is reused, with the default distro libpq and the recorded current-default complete-C equality proof. The original workload, ABI, driver, SQL, deadlines, cancellation, cleanup and quality gates remain unchanged. Explicit db-gpr uses999Hz/16KiB/33 ARM64 GPRs; controls remain unsampled. No product rebuild, library candidate or automatic replacement window.

SELF只描述运行态采样。未解析帧和未归因部分保留未知；inclusive调用链不能相加。ARM64展开祖先可能是LR-1标记，必须根据本次实际ELF指令还原对齐的返回PC和BL位置，不能当作SELF执行PC。控制max/min超过1.10或采样CPU超出原1.10互反包络时停止归因，不补窗。

SELF describes sampled on-CPU instructions, not off-CPU I/O waiting. Keep unresolved attribution unknown and never sum overlapping inclusive frames. Decode ARM64 LR-1 ancestor markers against the actual sampled ELF before assigning query/reset callers. A repeated invalid control sequence stops this item without replacement or a performance claim.
