use std::fs;
use std::path::{Path, PathBuf};
use crate::git_paths::branch_ref_path;
use crate::object::ObjectSha;
use crate::reference::{read_ref, update_ref};
use crate::symbolic_ref::{read_symbolic_ref, write_symbolic_ref, SymbolicRef};
use super::{make_case_dir, run_git, git_stdout, git_commit_tree_with_env, test_git_dir};

/// `branch_ref_path` 对 `.git` 和 `.gift` 都正确拼出 `refs/heads/<name>`。
#[test]
fn branch_ref_path_joins_heads() {
    assert_eq!(
        branch_ref_path(test_git_dir(), "main"),
        PathBuf::from(".git")
            .join("refs")
            .join("heads")
            .join("main")
    );
    assert_eq!(
        branch_ref_path(Path::new(".gift"), "main"),
        PathBuf::from(".gift")
            .join("refs")
            .join("heads")
            .join("main")
    );
}

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
    let case_dir = make_case_dir("git_ref_update");

    // 准备仓库：写文件、init、add、生成 tree 和首个 commit
    fs::write(case_dir.join("f.txt"), "x\n").unwrap();
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "f.txt"]);
    let tree_hex = git_stdout(&case_dir, &["write-tree"])
        .lines().next().unwrap().trim().to_string();
    let c0 = git_commit_tree_with_env(&case_dir, &tree_hex, &[], "c0");
    let ref_path = branch_ref_path(test_git_dir(), "mine");

    // git 写入分支 ref → gift 读取，断言一致
    run_git(&case_dir, &["update-ref", "refs/heads/mine", &c0]);
    let r = read_ref(&case_dir, test_git_dir(), &ref_path).expect("read_ref");
    assert_eq!(hex::encode(r.commit_id.as_bytes()), c0);

    // gift 写入新 commit c1 → git rev-parse 验证 → gift 再读取验证
    let c1 = git_commit_tree_with_env(&case_dir, &tree_hex, &[&c0], "c1");
    update_ref(
        &case_dir,
        test_git_dir(),
        &ref_path,
        &ObjectSha::SHA1(hex::decode(&c1).unwrap().try_into().unwrap()),
    )
    .expect("update_ref");

    let rev = git_stdout(&case_dir, &["rev-parse", "refs/heads/mine"])
        .lines().next().unwrap().trim().to_string();
    assert_eq!(rev, c1);
    let r2 = read_ref(&case_dir, test_git_dir(), &ref_path).expect("read_ref after gift update_ref");
    assert_eq!(hex::encode(r2.commit_id.as_bytes()), c1);
}

/// 流程：git init 
///       → git add
///       → git write-tree 取 tree OID（不创建 commit）
///       → 尝试把 tree OID 当作 commit 写入分支 ref 
///       → 断言被拒绝
#[test]
fn update_ref_rejects_non_commit_object() {
    let case_dir = make_case_dir("ref_reject_tree");

    // 准备仓库：只生成 tree，不生成 commit
    fs::write(case_dir.join("f.txt"), "y\n").unwrap();
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "f.txt"]);
    let tree_hex = git_stdout(&case_dir, &["write-tree"])
        .lines().next().unwrap().trim().to_string();

    // 把 tree OID 当作 commit 写分支 ref，应被拒绝
    let tree_sha = ObjectSha::SHA1(hex::decode(&tree_hex).unwrap().try_into().unwrap());
    let err = update_ref(
        &case_dir,
        test_git_dir(),
        &branch_ref_path(test_git_dir(), "bad"),
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
    let case_dir = make_case_dir("ref_reject_sym");
    run_git(&case_dir, &["init"]);

    // 手动创建 symbolic ref 文件
    let p = branch_ref_path(test_git_dir(), "sym");
    let full = case_dir.join(&p);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(&full, "ref: refs/heads/main\n").unwrap();

    // read_ref 读取 symbolic ref 应报错
    let err = read_ref(&case_dir, test_git_dir(), &p).unwrap_err();
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
    let case_dir = make_case_dir("sym_read_git");

    // 准备仓库：有真实 commit 的分支，再把 HEAD 指向它
    fs::write(case_dir.join("f.txt"), "a\n").unwrap();
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "f.txt"]);
    let tree_hex = git_stdout(&case_dir, &["write-tree"])
        .lines().next().unwrap().trim().to_string();
    let c0 = git_commit_tree_with_env(&case_dir, &tree_hex, &[], "root");
    run_git(&case_dir, &["update-ref", "refs/heads/main", &c0]);
    run_git(&case_dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    // gift 读取 HEAD symbolic ref，断言解析出正确的 ref_name
    let head_path = test_git_dir().join("HEAD");
    let s = read_symbolic_ref(&case_dir, &head_path).expect("read_symbolic_ref");
    assert_eq!(s.ref_name, "refs/heads/main");
}

/// 流程：git init → 写文件 add → git write-tree → git commit-tree 生成 commit
///       → git update-ref 创建分支 foo
///       → gift write_symbolic_ref 将 HEAD 写为指向 foo 的 symbolic ref
///       → git symbolic-ref -q HEAD 验证磁盘内容与 gift 写入一致
#[test]
fn write_symbolic_ref_matches_git() {
    let case_dir = make_case_dir("sym_write_git");

    // 准备仓库：有真实 commit 的分支 foo
    fs::write(case_dir.join("g.txt"), "b\n").unwrap();
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "g.txt"]);
    let tree_hex = git_stdout(&case_dir, &["write-tree"])
        .lines().next().unwrap().trim().to_string();
    let c0 = git_commit_tree_with_env(&case_dir, &tree_hex, &[], "tip");
    run_git(&case_dir, &["update-ref", "refs/heads/foo", &c0]);

    // gift 写入 HEAD → git 读取验证
    let sym = SymbolicRef { ref_name: "refs/heads/foo".into() };
    write_symbolic_ref(&case_dir, &test_git_dir().join("HEAD"), &sym).expect("write_symbolic_ref");

    let got = git_stdout(&case_dir, &["symbolic-ref", "-q", "HEAD"])
        .trim().to_string();
    assert_eq!(got, "refs/heads/foo");
}
