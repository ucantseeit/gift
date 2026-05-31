use std::fs;
use std::path::PathBuf;
use crate::git_paths::get_branch_ref_path;
use crate::object::ObjectSha;
use crate::reference::{read_ref, write_ref};
use crate::symbolic_ref::{read_symbolic_ref, write_symbolic_ref, SymbolicRef};
use super::{make_test_repo, run_git, git_stdout, git_commit_tree_with_env};

/// 流程：git init → 写文件 → git add → git write-tree 取 tree OID
///       → git commit-tree 生成 commit c0
///       → git update-ref 写入分支 ref
///       → gift read_ref 读取，断言 OID 与 c0 一致
///       → git commit-tree 生成 c1（c0 为 parent）
///       → gift update_ref 覆写分支 ref
///       → git rev-parse 验证磁盘内容为 c1
///       → gift read_ref 再次读取，断言 OID 与 c1 一致
#[test]
fn read_ref_and_update_ref_match_git() {
    let repo = make_test_repo("git_ref_update");

    // 准备仓库：写文件、init、add、生成 tree 和首个 commit
    fs::write(repo.worktree.join("f.txt"), "x\n").unwrap();
    run_git(&repo.worktree, &["init"]);
    run_git(&repo.worktree, &["add", "f.txt"]);
    let tree_hex = git_stdout(&repo.worktree, &["write-tree"])
        .lines().next().unwrap().trim().to_string();
    let c0 = git_commit_tree_with_env(&repo.worktree, &tree_hex, &[], "c0");
    let ref_abs = repo.git_abs.join(get_branch_ref_path("mine"));

    // git 写入分支 ref → gift 读取，断言一致
    run_git(&repo.worktree, &["update-ref", "refs/heads/mine", &c0]);
    let r = read_ref(&repo.git_abs, &ref_abs).expect("read_ref");
    assert_eq!(hex::encode(r.commit_id.as_bytes()), c0);

    // gift 写入新 commit c1 → git rev-parse 验证 → gift 再读取验证
    let c1 = git_commit_tree_with_env(&repo.worktree, &tree_hex, &[&c0], "c1");
    write_ref(
        &repo.git_abs,
        &ref_abs,
        &ObjectSha::SHA1(hex::decode(&c1).unwrap().try_into().unwrap()),
    )
    .expect("update_ref");

    let rev = git_stdout(&repo.worktree, &["rev-parse", "refs/heads/mine"])
        .lines().next().unwrap().trim().to_string();
    assert_eq!(rev, c1);
    let r2 = read_ref(&repo.git_abs, &ref_abs).expect("read_ref after gift update_ref");
    assert_eq!(hex::encode(r2.commit_id.as_bytes()), c1);
}

/// 流程：git init
///       → git add
///       → git write-tree 取 tree OID（不创建 commit）
///       → 尝试把 tree OID 当作 commit 写入分支 ref
///       → 断言被拒绝
#[test]
fn update_ref_rejects_non_commit_object() {
    let repo = make_test_repo("ref_reject_tree");

    // 准备仓库：只生成 tree，不生成 commit
    fs::write(repo.worktree.join("f.txt"), "y\n").unwrap();
    run_git(&repo.worktree, &["init"]);
    run_git(&repo.worktree, &["add", "f.txt"]);
    let tree_hex = git_stdout(&repo.worktree, &["write-tree"])
        .lines().next().unwrap().trim().to_string();

    // 把 tree OID 当作 commit 写分支 ref，应被拒绝
    let tree_sha = ObjectSha::SHA1(hex::decode(&tree_hex).unwrap().try_into().unwrap());
    let branch_ref_abs = 
        repo.git_abs.join(get_branch_ref_path("bad"));
    let err = write_ref(
        &repo.git_abs,
        &branch_ref_abs,
        &tree_sha,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("commit") || err.to_string().contains("tree"),
        "unexpected err: {err:?}"
    );
}

/// 流程：git init → 手动写入内容为 `ref: refs/heads/main` 的文件（symbolic ref 格式）
///       → 用 `read_ref`（只处理直接 OID ref）读取该文件 → 断言被拒绝
///
/// `read_ref` 只接受 40 位 hex 的 direct ref，symbolic ref 应交由 `read_symbolic_ref` 处理。
#[test]
fn read_ref_rejects_symbolic_file() {
    let repo = make_test_repo("ref_reject_sym");
    run_git(&repo.worktree, &["init"]);

    // 手动创建 symbolic ref 文件（p 是绝对路径，直接用）
    let branch_ref_abs = repo.git_abs.join(
        get_branch_ref_path("sym"));
    fs::create_dir_all(branch_ref_abs.parent().unwrap()).unwrap();
    fs::write(&branch_ref_abs, "ref: refs/heads/main\n").unwrap();

    // read_ref 读取 symbolic ref 应报错
    let err = read_ref(&repo.git_abs, &branch_ref_abs).unwrap_err();
    assert!(
        err.to_string().contains("symbolic") || err.to_string().contains("direct"),
        "unexpected err: {err:?}"
    );
}

/// 流程：git init → 写文件 add → git write-tree → git commit-tree 生成 commit
///       → git update-ref 创建分支 → git symbolic-ref 将 HEAD 指向该分支
///       → gift read_symbolic_ref 读取 HEAD → 断言解析出的 ref_name 与 git 一致
#[test]
fn read_symbolic_ref_matches_git() {
    let repo = make_test_repo("sym_read_git");

    // 准备仓库：有真实 commit 的分支，再把 HEAD 指向它
    fs::write(repo.worktree.join("f.txt"), "a\n").unwrap();
    run_git(&repo.worktree, &["init"]);
    run_git(&repo.worktree, &["add", "f.txt"]);
    let tree_hex = git_stdout(&repo.worktree, &["write-tree"])
        .lines().next().unwrap().trim().to_string();
    let c0 = git_commit_tree_with_env(&repo.worktree, &tree_hex, &[], "root");
    run_git(&repo.worktree, &["update-ref", "refs/heads/main", &c0]);
    run_git(&repo.worktree, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    // gift 读取 HEAD symbolic ref，断言解析出正确的 ref_name
    let head_path = repo.git_abs.join("HEAD");
    let s = read_symbolic_ref(&head_path).expect("read_symbolic_ref");
    assert_eq!(s.ref_path, PathBuf::from("refs/heads/main"));
}

/// 流程：git init → 写文件 add → git write-tree → git commit-tree 生成 commit
///       → git update-ref 创建分支 foo
///       → gift write_symbolic_ref 将 HEAD 写为指向 foo 的 symbolic ref
///       → git symbolic-ref -q HEAD 验证磁盘内容与 gift 写入一致
#[test]
fn write_symbolic_ref_matches_git() {
    let repo = make_test_repo("sym_write_git");

    // 准备仓库：有真实 commit 的分支 foo
    fs::write(repo.worktree.join("g.txt"), "b\n").unwrap();
    run_git(&repo.worktree, &["init"]);
    run_git(&repo.worktree, &["add", "g.txt"]);
    let tree_hex = git_stdout(&repo.worktree, &["write-tree"])
        .lines().next().unwrap().trim().to_string();
    let c0 = git_commit_tree_with_env(&repo.worktree, &tree_hex, &[], "tip");
    run_git(&repo.worktree, &["update-ref", "refs/heads/foo", &c0]);

    // gift 写入 HEAD → git 读取验证
    let sym = SymbolicRef { ref_path: PathBuf::from("refs/heads/foo") };
    write_symbolic_ref(&repo.worktree, &repo.git_abs.join("HEAD"), &sym).expect("write_symbolic_ref");

    let got = git_stdout(&repo.worktree, &["symbolic-ref", "-q", "HEAD"])
        .trim().to_string();
    assert_eq!(got, "refs/heads/foo");
}
