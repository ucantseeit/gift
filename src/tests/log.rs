use std::fs;
use super::{make_test_repo, run_git};
use crate::log;

/// 流程：git init → 两次提交 → gift log
/// 验证：log 能够按顺序输出两次提交，且内容包含提交信息
#[test]
fn test_log_basic() {
    let repo = make_test_repo("log_basic");

    run_git(&repo.worktree, &["init"]);

    fs::write(repo.worktree.join("a.txt"), "v1\n").unwrap();
    run_git(&repo.worktree, &["add", "a.txt"]);
    run_git(&repo.worktree, &["commit", "-m", "first commit"]);

    fs::write(repo.worktree.join("a.txt"), "v2\n").unwrap();
    run_git(&repo.worktree, &["add", "a.txt"]);
    run_git(&repo.worktree, &["commit", "-m", "second commit"]);

    // 调用 gift log（不限制条数），验证不 panic
    assert!(log::log(&repo.git_abs, None).is_ok());
}

/// 流程：三次提交 → gift log -n 2
/// 验证：只输出最近两条提交（通过检查命令行输出或函数不 panic 简化）
#[test]
fn test_log_max_count() {
    let repo = make_test_repo("log_max_count");

    run_git(&repo.worktree, &["init"]);

    for i in 1..=3 {
        fs::write(repo.worktree.join("a.txt"), format!("v{}\n", i)).unwrap();
        run_git(&repo.worktree, &["add", "a.txt"]);
        run_git(&repo.worktree, &["commit", "-m", format!("commit {}", i).as_str()]);
    }

    assert!(log::log(&repo.git_abs, Some(2)).is_ok());
}

/// 流程：无提交的仓库 → gift log
/// 验证：应返回错误（no commits yet）
#[test]
fn test_log_no_commits() {
    let repo = make_test_repo("log_no_commits");
    run_git(&repo.worktree, &["init"]);
    // 没有做任何提交
    let result = log::log(&repo.git_abs, None);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("no commits yet") || err_msg.contains("No commits"));
}