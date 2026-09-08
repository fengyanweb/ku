//! Current normative entry points must not drift back to historical syntax.

#[test]
fn documentation_separates_current_contract_from_implementation_evidence() {
    let readme = include_str!("../README.md");
    let syntax = include_str!("../docs/syntax.md");
    let semantics = include_str!("../docs/semantics.md");
    let worklog = include_str!("../docs/v0.0.18-worklog.md");
    let ir = include_str!("../docs/ir.md");
    assert!(readme.contains("docs/semantics.md"));
    assert!(syntax.contains("[语义合同](semantics.md)"));
    assert!(semantics.contains("[syntax.md](syntax.md)"));
    assert!(semantics.contains("v0.0.18-worklog.md"));
    assert!(worklog.contains("不是发行说明"));
    assert!(ir.contains("取消语义已确定"));
    assert!(ir.contains("[语义合同](semantics.md)"));
    assert!(ir.contains("其余 async native lowering 继续拒绝"));
    assert!(!ir.contains("取消语义单独决策"));
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
fn native_task_docs_limit_source_support_and_separate_error_layers() {
    let readme = include_str!("../README.md");
    let syntax = include_str!("../docs/syntax.md");
    let concurrency = include_str!("../docs/concurrency.md");
    let ir = include_str!("../docs/ir.md");
    for document in [readme, syntax, concurrency, ir] {
        for required in [
            "单 worker",
            "async fn main(): null!",
            "单层 Result",
            "try/catch/finally",
            "M:N",
            "netpoll",
            "RSS",
            "soak",
        ] {
            assert!(
                document.contains(required),
                "native Task scope missing: {required}"
            );
        }
        assert!(document.contains("三系统 CI"));
        assert!(!document.contains("native C 明确拒绝 async。"));
    }
    assert!(ir.contains("Frame ABI 3、Control ABI 1、Driver ABI 6"));
    for boundary in [
        "R5h 内部正常作用域 session（尚未接入源码）",
        "`scope_end` 不接受",
        "替换集合或子集",
        "Pending end 只是观察，不登记唤醒",
        "不阻止外部 control CAS",
        "父 D 缩短不扫描 manifest",
        "取消后向最终退出提升、外层 child 合并",
        "`ku_task_driver_scope_promote_final`",
        "只 OR 完整 expected",
        "不读新时钟、不续期、不清 fired/failure",
        "typed 整函数退出见证授权",
        "promotion 的 OK 仅表示元数据登记成功",
        "现有 wrapper 会跳过 scope 期限刷新",
        "R5h.3a 内部 typed Exit 清理桥接（尚未接入源码）",
        "源码 lower 当前继续使用 Complete",
        "KU_TASK_FRAME_EXIT_STAGED",
        "finish_exit_values",
        "实际 Task 位全空",
    ] {
        assert!(ir.contains(boundary), "scope session boundary: {boundary}");
    }
    assert!(ir.contains("KuTaskAdapterOutcomeV1"));
    assert!(ir.contains("KuTaskAdapterTakeRequestV1"));
    assert!(ir.contains("不兼容变更"));
    assert!(concurrency.contains("外层运行时 `task/shutdown_timeout`"));
    assert!(concurrency.contains("不同于业务 Result.err"));
    assert!(concurrency.contains("绝对清理期限只收紧、不续期"));
    assert!(syntax.contains("用户 cleanup 仍不能 Await"));
    assert!(ir.contains("成功 hosted take"));
    assert!(ir.contains("`ku ir` / `--emit-ir` / LLVM 仍拒绝 async"));
}

#[test]
fn native_sync_docs_separate_checked_cleanup_from_all_fatal_and_task_support() {
    let ir = include_str!("../docs/ir.md");
    for required in [
        "SyncGuard { continue_block, cleanup_block }",
        "普通算术 fatal 不可 catch",
        "Panic/index/OOM",
        "foreign callback 重入",
        "Task 用户 finally",
        "LLVM 仅保留 SyncGuard",
        "不提供本片 native C 的 checked 算术与 Owned 清理保证",
        "原绝对 D、不续期",
        "精确提交三系统 CI/sanitizer",
    ] {
        assert!(
            ir.contains(required),
            "missing synchronous boundary: {required}"
        );
    }
    assert!(!ir.contains("该路径仍存在独立的\n溢出/除零 UB 缺陷"));
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
