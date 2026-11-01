# libpq LDAP 按需装载候选

仅供独立开发分支的 `libpq-loader-build` 门禁使用，不修改 Ku 默认 SDK。

- 官方 PostgreSQL 18.6 源码在 runner 私有目录下载并核对固定 SHA；仓库仅保留小补丁、身份清单与夹具。
- Make 的 shared 候选通过 `DYNAMIC_LDAP=1` 启用；static、macOS、Windows 保持原路径。
- 门禁分别构建同源、同参数的 eager A 和 lazy B，并验证静态消费、真实原库初始化及有限装载失败。
- LDAP 用例只连接未提供服务的本机端口，不启动服务器，不发 SQL，不运行性能测试。
- Meson 的编译依赖修订尚未实际构建；成功 LDAP 查询、TLS 握手与服务内存收益也不在本门禁的通过范围。
- 首次 LDAP 使用前的隐式全局符号可见性以及缺库的失败时机会改变；显式装载原库保留完整初始化。

# libpq LDAP loader candidate

This development-only gate does not change Ku's default SDK. The runner verifies
the pinned official PostgreSQL 18.6 source archive, then builds an identical-source
eager/lazy Make pair. Only the Linux shared candidate enables `DYNAMIC_LDAP=1`.
Static and other-platform paths remain eager. Fixtures exercise actual library
initialization, static consumers and bounded loader failures; they start no server,
issue no SQL and measure no performance. Meson builds, successful LDAP searches,
TLS handshakes and whole-service memory savings remain unverified. Ambient symbol
visibility before first use and missing-library failure timing are explicit changes.
