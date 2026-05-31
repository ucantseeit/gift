use std::fs;
use crate::commit;
use crate::object::CommitObject;
use super::{make_case_dir, run_git, git_stdout, git_commit_tree_with_env, test_git_dir, test_commit_identity};

/// 流程：写文件 → git init → git add → gift commit（第一次，symbolic HEAD 指向尚不存在的分支）
///       → 断言：HEAD 指向新 commit OID、HEAD 仍为 symbolic ref、第一个 commit 无 parent
///       → 修改文件 → git add → gift commit（第二次）
///       → 断言：HEAD 前进到新 OID、第二个 commit 的 parent 为第一个 OID
///
/// 验证 symbolic HEAD 下连续提交时 parent 链正确建立，HEAD 随分支 tip 移动。
#[test]
fn commit_on_branch_first_then_second() {
    let case_dir = make_case_dir("commit_branch_twice");
    let id = test_commit_identity();
    let git_bare = case_dir.join(".git");

    fs::write(case_dir.join("track.txt"), "v1\n").unwrap();
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "track.txt"]);

    // 第一次提交：无 parent，HEAD 初次落地
    let oid1 = commit::commit(&case_dir, test_git_dir(), id.clone(), id.clone(), "first commit".into())
        .expect("first commit");

    let hex1 = hex::encode(oid1.as_bytes());
    let head_git = git_stdout(&case_dir, &["rev-parse", "HEAD"])
        .lines().next().unwrap().trim().to_string();
    assert_eq!(head_git, hex1, "HEAD after first commit");

    let sym_out = git_stdout(&case_dir, &["symbolic-ref", "-q", "HEAD"]);
    assert!(sym_out.trim().starts_with("refs/heads/"), "still symbolic: {}", sym_out.trim());

    let p1 = CommitObject::read_loose_commit(&git_bare, &hex1);
    assert!(p1.parents.is_empty(), "root has no parent");
    assert_eq!(p1.message, b"first commit\n");

    // 第二次提交：parent 应为 oid1
    fs::write(case_dir.join("track.txt"), "v2\n").unwrap();
    run_git(&case_dir, &["add", "track.txt"]);

    let oid2 = commit::commit(&case_dir, test_git_dir(), id.clone(), id.clone(), "second".into())
        .expect("second commit");

    let hex2 = hex::encode(oid2.as_bytes());
    let head2 = git_stdout(&case_dir, &["rev-parse", "HEAD"])
        .lines().next().unwrap().trim().to_string();
    assert_eq!(head2, hex2);

    let p2 = CommitObject::read_loose_commit(&git_bare, &hex2);
    assert_eq!(p2.parents.len(), 1, "second commit has one parent");
    assert_eq!(hex::encode(p2.parents[0].as_bytes()), hex1);
    assert_eq!(p2.message, b"second\n");
}

/// 流程：写文件 → git init → git add → git write-tree 取 tree OID
///       → git commit-tree 生成种子 commit c0 → 手动将 HEAD 文件写为裸 OID（模拟 detached HEAD）
///       → 修改文件 → git add → gift commit
///       → 断言：HEAD 文件内容为新 OID（裸 hex，非 symbolic ref）、新 commit 的 parent 为 c0
///
/// 验证 detached HEAD 状态下 commit 后 HEAD 被直接更新为新 OID，而不是移动分支。
#[test]
fn commit_on_detached_head_updates_oid_and_parent() {
    let case_dir = make_case_dir("commit_detached");
    let id = test_commit_identity();
    let git_bare = case_dir.join(".git");

    // 准备种子 commit，再手动制造 detached HEAD
    fs::write(case_dir.join("only.txt"), "a\n").unwrap();
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "only.txt"]);
    let tree_hex = git_stdout(&case_dir, &["write-tree"])
        .lines().next().unwrap().trim().to_string();
    let c0 = git_commit_tree_with_env(&case_dir, &tree_hex, &[], "seed");
    fs::write(case_dir.join(".git/HEAD"), format!("{c0}\n")).unwrap(); // detached HEAD

    // 在 detached HEAD 上提交
    fs::write(case_dir.join("only.txt"), "b\n").unwrap();
    run_git(&case_dir, &["add", "only.txt"]);

    let oid_new = commit::commit(&case_dir, test_git_dir(), id.clone(), id.clone(), "while detached".into())
        .expect("commit detached");

    let hex_new = hex::encode(oid_new.as_bytes());
    let raw_head = fs::read_to_string(case_dir.join(".git/HEAD")).unwrap();
    assert_eq!(raw_head.trim(), hex_new, "HEAD must be raw oid");

    let parsed = CommitObject::read_loose_commit(&git_bare, &hex_new);
    assert_eq!(parsed.parents.len(), 1);
    assert_eq!(hex::encode(parsed.parents[0].as_bytes()), c0);
    assert_eq!(parsed.message, b"while detached\n");
}
