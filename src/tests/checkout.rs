use std::fs;
use std::path::Path;
use crate::commit;
use crate::git_paths::branch_ref_path;
use crate::object::CommitObject;
use crate::reference::update_ref;
use super::{make_case_dir, run_git, git_stdout, test_git_dir, test_commit_identity};

/// 流程：git init → 写 a.txt(v1) + old.txt → add → gift commit c1
///       → 修改 a.txt(v2)、删除 old.txt、新增 new.txt → add → gift commit c2
///       → gift checkout_commit(c1)（detached 模式）
///       → 断言：a.txt 内容恢复为 v1、old.txt 重新出现、new.txt 被删除
///       → 断言：HEAD 为裸 OID（detached）且等于 c1 的 hex
///       → git write-tree 取当前索引的 tree OID，断言等于 c1 的 tree OID
///
/// 验证 checkout 到历史 commit 时工作区和 index 都正确还原，HEAD 进入 detached 状态。
#[test]
fn checkout_commit_detaches_and_restores_worktree_without_symlinks() {
    let case_dir = make_case_dir("checkout_detached_basic");
    let id = test_commit_identity();

    run_git(&case_dir, &["init"]);
    let git_bare = fs::canonicalize(case_dir.join(".git")).unwrap();

    // c1：a.txt(v1) + old.txt
    fs::write(case_dir.join("a.txt"), "v1\n").unwrap();
    fs::write(case_dir.join("old.txt"), "old\n").unwrap();
    run_git(&case_dir, &["add", "-A"]);
    let c1 = commit::commit(&case_dir, &git_bare, id.clone(), id.clone(), "first".into())
        .expect("commit c1");

    // c2：a.txt(v2)、删 old.txt、加 new.txt
    fs::write(case_dir.join("a.txt"), "v2\n").unwrap();
    fs::remove_file(case_dir.join("old.txt")).unwrap();
    fs::write(case_dir.join("new.txt"), "new\n").unwrap();
    run_git(&case_dir, &["add", "-A"]);
    let _c2 = commit::commit(&case_dir, &git_bare, id.clone(), id.clone(), "second".into())
        .expect("commit c2");

    // checkout 回 c1
    crate::checkout::checkout_commit(&case_dir, &git_bare, c1.clone())
        .expect("checkout c1 detached");

    assert_eq!(fs::read_to_string(case_dir.join("a.txt")).unwrap(), "v1\n");
    assert_eq!(fs::read_to_string(case_dir.join("old.txt")).unwrap(), "old\n");
    assert!(!case_dir.join("new.txt").exists(), "new.txt removed");

    let c1_hex = hex::encode(c1.as_bytes());
    let raw_head = fs::read_to_string(case_dir.join(".git/HEAD")).unwrap();
    assert_eq!(raw_head.trim(), c1_hex, "HEAD is detached to c1");

    // git write-tree 反映 index 状态，应与 c1 的 tree 一致
    let checked_out_tree = git_stdout(&case_dir, &["write-tree"])
        .lines().next().unwrap().trim().to_string();
    let c1_obj = CommitObject::read_loose_commit(&git_bare, &c1_hex);
    assert_eq!(
        checked_out_tree,
        hex::encode(c1_obj.tree.as_bytes()),
        "index tree after checkout equals c1 tree"
    );
}

/// 流程：git init → 写 branch.txt + common.txt → add → gift commit（side tip）
///       → gift update_ref 创建 side 分支指向该 commit
///       → 删除 branch.txt、修改 common.txt、新增 main.txt → add → gift commit（main tip）
///       → gift checkout_branch("side")
///       → 断言：branch.txt 恢复、common.txt 回到 side 版本、main.txt 被删除
///       → 断言：HEAD 为 symbolic ref 指向 refs/heads/side、side 分支 ref 文件内容正确
///
/// 验证 checkout 到分支时工作区正确还原，HEAD 保持 symbolic ref（不进入 detached）。
#[test]
fn checkout_branch_restores_tip_and_keeps_symbolic_head() {
    let case_dir = make_case_dir("checkout_branch_basic");
    let id = test_commit_identity();

    run_git(&case_dir, &["init"]);
    let git_bare = fs::canonicalize(case_dir.join(".git")).unwrap();

    // 制造 side 分支的 tip commit
    fs::write(case_dir.join("branch.txt"), "side\n").unwrap();
    fs::write(case_dir.join("common.txt"), "side version\n").unwrap();
    run_git(&case_dir, &["add", "-A"]);
    let side_tip = commit::commit(&case_dir, &git_bare, id.clone(), id.clone(), "side tip".into())
        .expect("commit side tip");
    update_ref(
        &case_dir,
        Path::new(".git"),
        &branch_ref_path(&git_bare, "side"),
        &side_tip,
    )
    .expect("create side branch");

    // 继续在默认分支上提交，制造差异
    fs::remove_file(case_dir.join("branch.txt")).unwrap();
    fs::write(case_dir.join("common.txt"), "main version\n").unwrap();
    fs::write(case_dir.join("main.txt"), "main\n").unwrap();
    run_git(&case_dir, &["add", "-A"]);
    let _main_tip = commit::commit(&case_dir, &git_bare, id.clone(), id.clone(), "main tip".into())
        .expect("commit main tip");

    // checkout side 分支
    crate::checkout::checkout_branch(&case_dir, &git_bare, "side")
        .expect("checkout side branch");

    assert_eq!(fs::read_to_string(case_dir.join("branch.txt")).unwrap(), "side\n");
    assert_eq!(fs::read_to_string(case_dir.join("common.txt")).unwrap(), "side version\n");
    assert!(!case_dir.join("main.txt").exists(), "main-only file removed");

    // HEAD 应为 symbolic ref，不进入 detached
    let head = fs::read_to_string(case_dir.join(".git/HEAD")).unwrap();
    assert_eq!(head.trim(), "ref: refs/heads/side");
    let side_ref = fs::read_to_string(case_dir.join(".git/refs/heads/side")).unwrap();
    assert_eq!(side_ref.trim(), hex::encode(side_tip.as_bytes()));
}

/// 流程：git init → 写 conflict.txt + stable.txt → add → gift commit（target，含 conflict.txt）
///       → 删除 conflict.txt、修改 stable.txt → add → gift commit（current，不含 conflict.txt）
///       → 在工作区手动创建 conflict.txt（未追踪文件）
///       → 尝试 gift checkout_commit(target) → 断言返回错误（untracked 冲突）
///       → 断言：conflict.txt 内容未被覆盖、stable.txt 未被回滚、HEAD 未改变
///
/// 验证 checkout 在写入任何文件前就检测 untracked 冲突，失败时不修改工作区和 HEAD。
#[test]
fn checkout_reports_untracked_file_conflict_before_writing() {
    let case_dir = make_case_dir("checkout_untracked_conflict");
    let id = test_commit_identity();

    run_git(&case_dir, &["init"]);
    let git_bare = fs::canonicalize(case_dir.join(".git")).unwrap();

    // target commit：包含 conflict.txt
    fs::write(case_dir.join("conflict.txt"), "tracked in target\n").unwrap();
    fs::write(case_dir.join("stable.txt"), "target stable\n").unwrap();
    run_git(&case_dir, &["add", "-A"]);
    let target = commit::commit(&case_dir, &git_bare, id.clone(), id.clone(), "target".into())
        .expect("commit target");

    // current commit：不含 conflict.txt
    fs::remove_file(case_dir.join("conflict.txt")).unwrap();
    fs::write(case_dir.join("stable.txt"), "current stable\n").unwrap();
    run_git(&case_dir, &["add", "-A"]);
    let current = commit::commit(&case_dir, &git_bare, id.clone(), id.clone(), "current".into())
        .expect("commit current");

    // 手动创建 untracked 文件，与 target 中的 conflict.txt 路径冲突
    fs::write(case_dir.join("conflict.txt"), "untracked local\n").unwrap();

    let err = crate::checkout::checkout_commit(&case_dir, test_git_dir(), target)
        .expect_err("checkout should reject untracked conflict");
    assert!(err.to_string().contains("untracked working tree file"), "unexpected error: {err:?}");

    // 工作区和 HEAD 不应被修改
    assert_eq!(
        fs::read_to_string(case_dir.join("conflict.txt")).unwrap(),
        "untracked local\n",
        "untracked file preserved"
    );
    assert_eq!(
        fs::read_to_string(case_dir.join("stable.txt")).unwrap(),
        "current stable\n",
        "tracked file was not overwritten after failed checkout"
    );
    let raw_head = fs::read_to_string(case_dir.join(".git/HEAD")).unwrap();
    assert!(
        raw_head.trim().ends_with("master") || raw_head.trim().ends_with("main"),
        "HEAD remains symbolic after failed checkout: {raw_head:?}"
    );
    let current_hex = hex::encode(current.as_bytes());
    let head_commit = git_stdout(&case_dir, &["rev-parse", "HEAD"])
        .lines().next().unwrap().trim().to_string();
    assert_eq!(head_commit, current_hex);
}
