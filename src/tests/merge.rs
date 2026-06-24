use std::ffi::OsString;
use std::fs;
use std::path::Path;

use crate::merge::{merge_trees, MergeEntry};
use crate::object::{ObjectSha, TreeObject};

use super::{make_test_repo, run_git, git_stdout};

// ── 辅助函数 ─────────────────────────────────────────────────────────────────

/// 在 MergeEntry 列表中按路径段逐层查找（Subtree 自动下钻）。
fn find_entry<'a>(entries: &'a [MergeEntry], path: &[&str]) -> Option<&'a MergeEntry> {
    let target = OsString::from(path[0]);
    for entry in entries {
        let entry_name = match entry {
            MergeEntry::Take { name, .. }
            | MergeEntry::Delete { name }
            | MergeEntry::Conflict { name, .. }
            | MergeEntry::Subtree { name, .. } => name,
        };
        if *entry_name != target {
            continue;
        }
        if path.len() == 1 {
            return Some(entry);
        }
        if let MergeEntry::Subtree { entries, .. } = entry {
            return find_entry(entries, &path[1..]);
        }
        return None;
    }
    None
}

/// `git rev-parse <commit>:<path>` 得到该 commit 里某个文件/目录的 OID。
fn rev_parse_path(dir: &Path, commit: &str, path: &str) -> String {
    git_stdout(dir, &["rev-parse", &format!("{commit}:{path}")]).trim().to_string()
}

/// 将 hex OID 字符串转成 ObjectSha::SHA1。
fn hex_to_sha1(hex: &str) -> ObjectSha {
    ObjectSha::SHA1(hex::decode(hex).unwrap().try_into().unwrap())
}

/// 读出某个 commit 对应的 TreeObject。
fn commit_tree(git_abs: &Path, dir: &Path, commit_hex: &str) -> TreeObject {
    let tree_oid = git_stdout(dir, &["rev-parse", &format!("{commit_hex}^{{tree}}")])
        .trim().to_string();
    TreeObject::read_loose_tree(git_abs, &tree_oid).unwrap()
}

// ── 测试 ──────────────────────────────────────────────────────────────────────

/// 两级目录，双方各改不同文件，需要两次递归（顶层 → src/）。
///
/// 结构：
///   base:   src/foo.rs = "foo\n",  src/bar.rs = "bar\n"
///   ours:   src/foo.rs = "foo modified\n"（bar 不变）
///   theirs: src/bar.rs = "bar modified\n"（foo 不变）
///
/// 期望结果：
///   Subtree { "src",
///     Take { "bar.rs", their_bar_oid },
///     Take { "foo.rs", our_foo_oid },
///   }
#[test]
fn two_level_subtree_both_change_different_files() {
    let repo = make_test_repo("mt_two_level");
    run_git(&repo.worktree, &["init"]);

    // base
    fs::create_dir(repo.worktree.join("src")).unwrap();
    fs::write(repo.worktree.join("src/foo.rs"), "foo\n").unwrap();
    fs::write(repo.worktree.join("src/bar.rs"), "bar\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "base"]);
    let base_commit = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    // ours branch
    run_git(&repo.worktree, &["checkout", "-b", "ours"]);
    fs::write(repo.worktree.join("src/foo.rs"), "foo modified\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "ours"]);
    let our_commit = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    // theirs branch（从 base 分叉）
    run_git(&repo.worktree, &["checkout", "-b", "theirs", &base_commit]);
    fs::write(repo.worktree.join("src/bar.rs"), "bar modified\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "theirs"]);
    let their_commit = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    // 读树，运行 merge_trees
    let base_tree = commit_tree(&repo.git_abs, &repo.worktree, &base_commit);
    let our_tree  = commit_tree(&repo.git_abs, &repo.worktree, &our_commit);
    let their_tree = commit_tree(&repo.git_abs, &repo.worktree, &their_commit);

    let result = merge_trees(&repo.git_abs, &base_tree, &our_tree, &their_tree).unwrap();

    // src/ 必须是 Subtree（双方都改了它）
    let src_entry = find_entry(&result, &["src"]).expect("src should exist");
    assert!(
        matches!(src_entry, MergeEntry::Subtree { .. }),
        "src/ should be Subtree, got {:?}",
        src_entry
    );

    // src/foo.rs → Take，OID 应等于 ours 的版本
    let expected_foo = rev_parse_path(&repo.worktree, &our_commit, "src/foo.rs");
    let foo_entry = find_entry(&result, &["src", "foo.rs"]).expect("src/foo.rs should exist");
    match foo_entry {
        MergeEntry::Take { oid, .. } => {
            assert_eq!(oid.to_string(), expected_foo, "foo.rs oid mismatch");
        }
        other => panic!("src/foo.rs should be Take, got {:?}", other),
    }

    // src/bar.rs → Take，OID 应等于 theirs 的版本
    let expected_bar = rev_parse_path(&repo.worktree, &their_commit, "src/bar.rs");
    let bar_entry = find_entry(&result, &["src", "bar.rs"]).expect("src/bar.rs should exist");
    match bar_entry {
        MergeEntry::Take { oid, .. } => {
            assert_eq!(oid.to_string(), expected_bar, "bar.rs oid mismatch");
        }
        other => panic!("src/bar.rs should be Take, got {:?}", other),
    }
}

/// 三级目录，双方各改不同叶子，需要三次递归（顶层 → a/ → a/b/）。
///
/// 结构：
///   base:   a/b/x.txt = "x\n",  a/b/y.txt = "y\n"
///   ours:   a/b/x.txt 修改
///   theirs: a/b/y.txt 修改
///
/// 期望结果：
///   Subtree { "a",
///     Subtree { "b",
///       Take { "x.txt", our_x_oid },
///       Take { "y.txt", their_y_oid },
///     }
///   }
#[test]
fn three_level_subtree_recursive_merge() {
    let repo = make_test_repo("mt_three_level");
    run_git(&repo.worktree, &["init"]);

    // base
    fs::create_dir_all(repo.worktree.join("a/b")).unwrap();
    fs::write(repo.worktree.join("a/b/x.txt"), "x\n").unwrap();
    fs::write(repo.worktree.join("a/b/y.txt"), "y\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "base"]);
    let base_commit = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    // ours
    run_git(&repo.worktree, &["checkout", "-b", "ours"]);
    fs::write(repo.worktree.join("a/b/x.txt"), "x modified by ours\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "ours"]);
    let our_commit = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    // theirs
    run_git(&repo.worktree, &["checkout", "-b", "theirs", &base_commit]);
    fs::write(repo.worktree.join("a/b/y.txt"), "y modified by theirs\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "theirs"]);
    let their_commit = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    let base_tree  = commit_tree(&repo.git_abs, &repo.worktree, &base_commit);
    let our_tree   = commit_tree(&repo.git_abs, &repo.worktree, &our_commit);
    let their_tree = commit_tree(&repo.git_abs, &repo.worktree, &their_commit);

    let result = merge_trees(&repo.git_abs, &base_tree, &our_tree, &their_tree).unwrap();

    // a/ 必须是 Subtree
    assert!(
        matches!(find_entry(&result, &["a"]), Some(MergeEntry::Subtree { .. })),
        "a/ should be Subtree"
    );
    // a/b/ 必须是 Subtree（第二层递归产生）
    assert!(
        matches!(find_entry(&result, &["a", "b"]), Some(MergeEntry::Subtree { .. })),
        "a/b/ should be Subtree"
    );

    // 叶子 OID 验证
    let expected_x = rev_parse_path(&repo.worktree, &our_commit, "a/b/x.txt");
    match find_entry(&result, &["a", "b", "x.txt"]).expect("x.txt") {
        MergeEntry::Take { oid, .. } => assert_eq!(oid.to_string(), expected_x),
        other => panic!("x.txt should be Take, got {:?}", other),
    }

    let expected_y = rev_parse_path(&repo.worktree, &their_commit, "a/b/y.txt");
    match find_entry(&result, &["a", "b", "y.txt"]).expect("y.txt") {
        MergeEntry::Take { oid, .. } => assert_eq!(oid.to_string(), expected_y),
        other => panic!("y.txt should be Take, got {:?}", other),
    }
}

/// 三级目录，双方在同一嵌套文件上各自修改，产生深层 Conflict。
///
/// 结构：
///   base:   lib/core/mod.rs = "v1\n"
///   ours:   lib/core/mod.rs = "ours version\n"
///   theirs: lib/core/mod.rs = "theirs version\n"
///
/// 期望结果：
///   Subtree { "lib",
///     Subtree { "core",
///       Conflict { "mod.rs", base_oid, our_oid, their_oid }
///     }
///   }
#[test]
fn nested_dirs_conflict_at_leaf() {
    let repo = make_test_repo("mt_nested_conflict");
    run_git(&repo.worktree, &["init"]);

    // base
    fs::create_dir_all(repo.worktree.join("lib/core")).unwrap();
    fs::write(repo.worktree.join("lib/core/mod.rs"), "v1\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "base"]);
    let base_commit = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    // ours
    run_git(&repo.worktree, &["checkout", "-b", "ours"]);
    fs::write(repo.worktree.join("lib/core/mod.rs"), "ours version\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "ours"]);
    let our_commit = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    // theirs
    run_git(&repo.worktree, &["checkout", "-b", "theirs", &base_commit]);
    fs::write(repo.worktree.join("lib/core/mod.rs"), "theirs version\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "theirs"]);
    let their_commit = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    let base_tree  = commit_tree(&repo.git_abs, &repo.worktree, &base_commit);
    let our_tree   = commit_tree(&repo.git_abs, &repo.worktree, &our_commit);
    let their_tree = commit_tree(&repo.git_abs, &repo.worktree, &their_commit);

    let result = merge_trees(&repo.git_abs, &base_tree, &our_tree, &their_tree).unwrap();

    // lib/ 和 lib/core/ 必须是 Subtree（两层递归）
    assert!(matches!(find_entry(&result, &["lib"]), Some(MergeEntry::Subtree { .. })));
    assert!(matches!(find_entry(&result, &["lib", "core"]), Some(MergeEntry::Subtree { .. })));

    // lib/core/mod.rs 必须是 Conflict，三方 OID 均正确
    let base_oid  = hex_to_sha1(&rev_parse_path(&repo.worktree, &base_commit,  "lib/core/mod.rs"));
    let our_oid   = hex_to_sha1(&rev_parse_path(&repo.worktree, &our_commit,   "lib/core/mod.rs"));
    let their_oid = hex_to_sha1(&rev_parse_path(&repo.worktree, &their_commit, "lib/core/mod.rs"));

    match find_entry(&result, &["lib", "core", "mod.rs"]).expect("mod.rs") {
        MergeEntry::Conflict { base, ours, theirs, .. } => {
            assert_eq!(base.as_ref().map(|(_, o)| o),   Some(&base_oid),  "base oid");
            assert_eq!(ours.as_ref().map(|(_, o)| o),   Some(&our_oid),   "ours oid");
            assert_eq!(theirs.as_ref().map(|(_, o)| o), Some(&their_oid), "theirs oid");
        }
        other => panic!("mod.rs should be Conflict, got {:?}", other),
    }
}

/// 混合场景：一侧只新增了一整棵子树，另一侧修改了同级的普通文件。
/// 双方在子树层面"没有交集"，应分别 Take 而不产生 Subtree（目录只在一侧存在时直接 Take）。
///
/// 结构：
///   base:   root.txt = "root\n"
///   ours:   root.txt 不变；新增 extra/helper.rs = "helper\n"
///   theirs: root.txt = "root modified\n"；无 extra/
///
/// 期望结果（路径维度）：
///   Take { "extra",  mode=Directory, oid=our_extra_tree_oid }  ← 一侧新增整棵子树直接 Take
///   Take { "root.txt", their_root_oid }
#[test]
fn one_side_adds_subtree_other_modifies_file() {
    let repo = make_test_repo("mt_one_side_subtree");
    run_git(&repo.worktree, &["init"]);

    // base
    fs::write(repo.worktree.join("root.txt"), "root\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "base"]);
    let base_commit = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    // ours：新增整棵 extra/ 子树，root.txt 不动
    run_git(&repo.worktree, &["checkout", "-b", "ours"]);
    fs::create_dir(repo.worktree.join("extra")).unwrap();
    fs::write(repo.worktree.join("extra/helper.rs"), "helper\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "ours"]);
    let our_commit = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    // theirs：修改 root.txt，无 extra/
    run_git(&repo.worktree, &["checkout", "-b", "theirs", &base_commit]);
    fs::write(repo.worktree.join("root.txt"), "root modified\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "theirs"]);
    let their_commit = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    let base_tree  = commit_tree(&repo.git_abs, &repo.worktree, &base_commit);
    let our_tree   = commit_tree(&repo.git_abs, &repo.worktree, &our_commit);
    let their_tree = commit_tree(&repo.git_abs, &repo.worktree, &their_commit);

    let result = merge_trees(&repo.git_abs, &base_tree, &our_tree, &their_tree).unwrap();

    // extra/ 只在 ours 存在 → 直接 Take（不是 Subtree，因为 theirs 没有它）
    let expected_extra_oid = rev_parse_path(&repo.worktree, &our_commit, "extra");
    match find_entry(&result, &["extra"]).expect("extra/ should exist") {
        MergeEntry::Take { oid, .. } => {
            assert_eq!(oid.to_string(), expected_extra_oid, "extra tree oid mismatch");
        }
        other => panic!("extra/ should be Take (only ours has it), got {:?}", other),
    }

    // root.txt → Take theirs
    let expected_root = rev_parse_path(&repo.worktree, &their_commit, "root.txt");
    match find_entry(&result, &["root.txt"]).expect("root.txt") {
        MergeEntry::Take { oid, .. } => {
            assert_eq!(oid.to_string(), expected_root, "root.txt oid mismatch");
        }
        other => panic!("root.txt should be Take, got {:?}", other),
    }
}
