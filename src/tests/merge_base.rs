use std::fs;
use std::process::Command;

use crate::merge_base::find_merge_base;
use crate::object::ObjectSha;

use super::{make_test_repo, run_git, git_stdout};

fn hex_to_sha1(hex: &str) -> ObjectSha {
    ObjectSha::SHA1(hex::decode(hex.trim()).unwrap().try_into().unwrap())
}

/// 调用 `git merge-base a b`，返回 OID hex 或 None（exit 1 表示无共同祖先）
fn git_merge_base(dir: &std::path::Path, a: &str, b: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["merge-base", a, b])
        .current_dir(dir)
        .output()
        .expect("git merge-base");
    if out.status.success() {
        Some(String::from_utf8(out.stdout).unwrap().trim().to_string())
    } else {
        None
    }
}

/// 线性历史：root → c1 → c2
/// merge_base(c1, c2) == c1（c1 是 c2 的直接父）
#[test]
fn linear_history_ancestor_is_parent() {
    let repo = make_test_repo("mb_linear");
    run_git(&repo.worktree, &["init"]);

    fs::write(repo.worktree.join("f.txt"), "v1\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "root"]);
    let root = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    fs::write(repo.worktree.join("f.txt"), "v2\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "c1"]);
    let c1 = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    fs::write(repo.worktree.join("f.txt"), "v3\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "c2"]);
    let c2 = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    let expected = git_merge_base(&repo.worktree, &c1, &c2).unwrap();
    assert_eq!(expected, c1, "git: merge-base(c1, c2) should be c1");

    let result = find_merge_base(&repo.git_abs, &hex_to_sha1(&c1), &hex_to_sha1(&c2))
        .unwrap().unwrap();
    assert_eq!(result.to_string(), expected);

    // root 也验证一下
    let expected_root = git_merge_base(&repo.worktree, &root, &c2).unwrap();
    let result_root = find_merge_base(&repo.git_abs, &hex_to_sha1(&root), &hex_to_sha1(&c2))
        .unwrap().unwrap();
    assert_eq!(result_root.to_string(), expected_root);
}

/// 分叉历史：base → branch_a / branch_b（两个分支从同一点分叉，尚未合并）
/// merge_base(tip_a, tip_b) == base
#[test]
fn forked_history_returns_fork_point() {
    let repo = make_test_repo("mb_fork");
    run_git(&repo.worktree, &["init"]);

    fs::write(repo.worktree.join("base.txt"), "base\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "base"]);
    let base = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    // branch_a
    run_git(&repo.worktree, &["checkout", "-b", "branch_a"]);
    fs::write(repo.worktree.join("a.txt"), "from a\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "tip_a"]);
    let tip_a = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    // branch_b（从 base 分叉）
    run_git(&repo.worktree, &["checkout", "-b", "branch_b", &base]);
    fs::write(repo.worktree.join("b.txt"), "from b\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "tip_b"]);
    let tip_b = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    let expected = git_merge_base(&repo.worktree, &tip_a, &tip_b).unwrap();
    assert_eq!(expected, base, "git: fork point should be base");

    let result = find_merge_base(&repo.git_abs, &hex_to_sha1(&tip_a), &hex_to_sha1(&tip_b))
        .unwrap().unwrap();
    assert_eq!(result.to_string(), expected);
}

/// 两个完全不相关的 root commit（孤立历史）→ None
#[test]
fn unrelated_histories_returns_none() {
    let repo = make_test_repo("mb_unrelated");
    run_git(&repo.worktree, &["init"]);

    fs::write(repo.worktree.join("f.txt"), "first\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "root1"]);
    let root1 = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    // 创建孤立分支（orphan），产生第二个 root commit
    run_git(&repo.worktree, &["checkout", "--orphan", "orphan"]);
    run_git(&repo.worktree, &["rm", "-rf", "."]);
    fs::write(repo.worktree.join("g.txt"), "second\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "root2"]);
    let root2 = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    assert!(git_merge_base(&repo.worktree, &root1, &root2).is_none(), "git: no common ancestor");

    let result = find_merge_base(&repo.git_abs, &hex_to_sha1(&root1), &hex_to_sha1(&root2)).unwrap();
    assert!(result.is_none());
}

/// 两个相同的 OID → 返回自身
#[test]
fn same_commit_returns_itself() {
    let repo = make_test_repo("mb_same");
    run_git(&repo.worktree, &["init"]);

    fs::write(repo.worktree.join("f.txt"), "v1\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "only"]);
    let c = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    let oid = hex_to_sha1(&c);
    let result = find_merge_base(&repo.git_abs, &oid, &oid).unwrap().unwrap();
    assert_eq!(result.to_string(), c);
}
