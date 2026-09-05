//! Current normative entry points must not drift back to historical syntax.

#[test]
fn documentation_separates_current_contract_from_implementation_evidence() {
    let readme = include_str!("../README.md");
    let syntax = include_str!("../docs/syntax.md");
    let semantics = include_str!("../docs/semantics.md");
    let worklog = include_str!("../docs/v0.0.18-worklog.md");
    assert!(readme.contains("docs/semantics.md"));
    assert!(syntax.contains("[语义合同](semantics.md)"));
    assert!(semantics.contains("[syntax.md](syntax.md)"));
    assert!(semantics.contains("v0.0.18-worklog.md"));
    assert!(worklog.contains("不是发行说明"));
    assert!(!readme.contains("## 0.0.15 支持的核心语法"));
    assert!(!syntax.contains("Ku 0.0.15 的基础类型"));
    assert!(!syntax.contains("0.0.15 的历史边界是"));
    assert!(syntax.contains("默认 runner 当前仍"));
    for required in [
        "&name: T",
        "fn(&T): R",
        "/user/{id}",
        "read_header_timeout_ms",
        "max_active_requests",
        "`del`",
        "fn(req, res)",
        "module.client(config)?",
        "task.spawn",
        "Task.new",
        "runtime.schedule",
        "万级同时 keep-alive",
    ] {
        assert!(
            semantics.contains(required),
            "missing semantic invariant: {required}"
        );
    }
}

#[test]
fn protocol_status_binds_tls_evidence_to_published_commit() {
    let readme = include_str!("../README.md");
    let protocol = include_str!("../docs/protocol-foundation.md");
    assert!(protocol.contains("c668283"));
    assert!(protocol.contains("https://github.com/fengyanweb/ku/actions/runs/33969256015"));
    assert!(!protocol.contains("三系统最终消费者 CI 仍待跑绿"));
    assert!(!protocol.contains("下一步先跑绿精确 target pack"));
    assert!(protocol.contains("RESP3、registry v2、官方托管与高可用不在本轮范围"));
    assert!(readme.contains("c66828390eb3124750bca9a9c7e789dd2df70267"));
    assert!(!readme.contains("三系统最终消费者 CI 仍是发布阻断项"));
}
