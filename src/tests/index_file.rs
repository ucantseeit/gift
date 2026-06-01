use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use crate::index::index_file;
use crate::index::index_tree::TreeNode;
use crate::object::{FileMode, ObjectSha, TreeObject};
use super::{
    make_test_repo, make_gift_repo, run_git, git_stdout, git_ls_tree_map,
    mode_word_to_file_mode, collect_blob_leaves_from_root,
};

/// 流程：创建多级目录文件（含中文路径）→ git init → git add
///       → gift display_index_file 解析并打印 .git/index
///
/// 验证解析不报错、能处理非 ASCII 路径（no panic）。输出仅通过 --nocapture 可见，无断言。
#[test]
fn parse_index() {
    let repo = make_test_repo("parse_index");

    fs::write(repo.worktree.join("a.txt"), "hello\n").unwrap();
    fs::create_dir_all(repo.worktree.join("foo").join("啊")).unwrap();
    fs::write(repo.worktree.join("foo").join("啊").join("bar"), "你好！aa\n").unwrap();

    run_git(&repo.worktree, &["init"]);
    run_git(&repo.worktree, &["add", "."]);

    index_file::display_index_file(&repo.git_abs.join("index")).unwrap();
}

/// 流程：gift init → 写文件（顶层 + 子目录）→ gift stage_paths（相当于 git add）
///       → gift parse_index_file 读回 index
///       → 断言：版本号为 2、entry 数量正确、路径排序正确
///       → 对每个 entry：计算文件 SHA1 与 index 中的 OID 一致，loose object 文件存在
///
/// 全程不调用 git，验证 gift stage → index 写入 → 读回的完整闭环。
#[test]
fn stage_paths_roundtrip_index() {
    let repo = make_gift_repo("stage_roundtrip");
    crate::init(&repo.git_abs).unwrap();

    fs::write(repo.worktree.join("top.txt"), b"hello\n").unwrap();
    fs::create_dir_all(repo.worktree.join("sub")).unwrap();
    fs::write(repo.worktree.join("sub").join("nested.txt"), b"x\n").unwrap();

    let inputs = vec![repo.worktree.join("top.txt"), repo.worktree.join("sub")];
    crate::staging::stage_paths(&repo.git_abs, &repo.worktree, &inputs, true).unwrap();

    let idx = index_file::parse_index_file(&repo.git_abs.join("index")).unwrap();
    assert_eq!(idx.version(), 2, "index version");
    assert_eq!(idx.entries().len(), 2, "entry count");

    let mut paths: Vec<Vec<u8>> = idx.entries().map(|e| e.path().to_vec()).collect();
    paths.sort();
    assert_eq!(
        paths,
        vec![b"sub/nested.txt".to_vec(), b"top.txt".to_vec()],
        "index paths"
    );

    for e in idx.entries() {
        let rel = std::str::from_utf8(e.path()).expect("entry path utf-8");
        let disk = repo.worktree.join(rel);
        let (sha, _) = crate::object::hash_object(&disk).unwrap();
        assert_eq!(
            hex::encode(sha.as_bytes()),
            hex::encode(e.obj_name().as_bytes()),
            "blob oid mismatch for {rel}"
        );

        let hex_oid = hex::encode(e.obj_name().as_bytes());
        let loose = crate::git_paths::loose_object_path(&repo.git_abs, &hex_oid);
        assert!(
            loose.is_file(),
            "loose object file should exist: {}",
            loose.display()
        );
    }
}

/// 流程：创建多种文件（普通文件、可执行文件 `chmod +x`、符号链接、多级中文目录）
///       → git init → git add → git write-tree 取 tree OID
///       → gift read_loose_tree 解析根 tree 及子 tree
///       → 与 git ls-tree -z 的输出逐条对拍（mode、type、sha、name）
///
/// 覆盖所有常见 FileMode：NExecRegularFile、ExecRegularFile、SymbolicLink、Directory。
#[cfg(unix)]
#[test]
fn cmp_read_tree_matches_git_ls_tree() {
    use std::os::unix::fs::symlink;

    let repo = make_test_repo("read_tree");

    // 准备各种类型的文件
    fs::write(repo.worktree.join("a.txt"), "hello\n").unwrap();
    fs::create_dir_all(repo.worktree.join("foo").join("啊")).unwrap();
    fs::write(repo.worktree.join("foo").join("啊").join("bar"), "你好！aa\n").unwrap();
    fs::write(repo.worktree.join("script.sh"), "#!/bin/sh\necho ok\n").unwrap();
    symlink("a.txt", repo.worktree.join("link")).expect("symlink link -> a.txt");

    let chmod_ok = Command::new("chmod")
        .args(["+x", "script.sh"])
        .current_dir(&repo.worktree)
        .status()
        .expect("chmod");
    assert!(chmod_ok.success(), "chmod +x script.sh");

    run_git(&repo.worktree, &["init"]);
    run_git(&repo.worktree, &["add", "."]);

    let root_hex = git_stdout(&repo.worktree, &["write-tree"])
        .lines().next().expect("write-tree line").trim().to_string();
    assert_eq!(root_hex.len(), 40, "root tree oid hex length");

    let cat_t = git_stdout(&repo.worktree, &["cat-file", "-t", &root_hex]).trim().to_string();
    assert_eq!(cat_t, "tree");

    // 根 tree：gift 解析结果与 git ls-tree 逐条对拍
    let expected_root = git_ls_tree_map(&repo.worktree, &root_hex);
    let tree_root = TreeObject::read_loose_tree(&repo.git_abs, &root_hex).unwrap();

    assert_eq!(
        hex::encode(tree_root.object_name().as_bytes()),
        root_hex,
        "TreeObject carries the opened oid"
    );
    assert_eq!(tree_root.entries().len(), expected_root.len(), "root entry count vs git ls-tree");

    for (name, entry) in tree_root.entries() {
        let key = name.to_str().expect("utf-8 name in fixture");
        let (mode, obj_type, sha) = expected_root
            .get(key)
            .unwrap_or_else(|| panic!("unexpected entry {key:?}"));
        assert_eq!(mode_word_to_file_mode(mode), entry.file_mode, "mode for {key}");
        assert_eq!(hex::encode(entry.object_name.as_bytes()), *sha, "sha {key}");
        match entry.file_mode {
            FileMode::Directory => assert_eq!(obj_type, "tree"),
            FileMode::SymbolicLink | FileMode::NExecRegularFile | FileMode::ExecRegularFile => {
                assert_eq!(obj_type, "blob");
            }
            _ => panic!("unexpected FileMode in fixture"),
        }
    }

    // 递归验证子 tree：foo/ → 啊/ → bar
    let (_, _, foo_tree_sha) = expected_root.get("foo").expect("foo tree");
    let expected_foo = git_ls_tree_map(&repo.worktree, foo_tree_sha);
    let tree_foo = TreeObject::read_loose_tree(&repo.git_abs, foo_tree_sha).unwrap();
    assert_eq!(tree_foo.entries().len(), expected_foo.len());
    assert_eq!(tree_foo.entries().len(), 1, "foo/ only contains 啊");

    let (_, _, ah_tree_sha) = expected_foo.get("啊").expect("啊 tree under foo");
    let expected_ah = git_ls_tree_map(&repo.worktree, ah_tree_sha);
    let tree_ah = TreeObject::read_loose_tree(&repo.git_abs, ah_tree_sha).unwrap();
    assert_eq!(tree_ah.entries().len(), 1);
    assert_eq!(tree_ah.entries().len(), expected_ah.len());

    let bar_entry = tree_ah.entries().get(OsStr::new("bar")).expect("bar blob");
    assert_eq!(bar_entry.file_mode, FileMode::NExecRegularFile);
    let (_, _, bar_sha) = expected_ah.get("bar").unwrap();
    assert_eq!(hex::encode(bar_entry.object_name.as_bytes()), *bar_sha);
}

/// 流程：创建多级目录文件（含中文路径）→ git init → git add
///       → gift parse_index_file 读取 index，把 entries 转成 path→(mode,oid) 映射（expected）
///       → gift from_index_file 构建内存树，DFS 收集所有 blob 叶子（got）
///       → 双向断言：got 与 expected 的 path、mode、oid 完全一致
///
/// 验证 from_index_file 把扁平 index 正确转换为嵌套树结构，无遗漏也无多余叶子。
#[test]
fn from_index_file_matches_parsed_index_entries() {
    let repo = make_test_repo("from_index_only");
    fs::write(repo.worktree.join("a.txt"), "hello\n").unwrap();
    fs::create_dir_all(repo.worktree.join("foo").join("啊")).unwrap();
    fs::write(repo.worktree.join("foo").join("啊").join("bar"), "你好！aa\n").unwrap();

    run_git(&repo.worktree, &["init"]);
    run_git(&repo.worktree, &["add", "."]);

    // expected：直接从 index entries 建映射
    let idx = index_file::parse_index_file(&repo.git_abs.join("index")).unwrap();
    let mut expected: BTreeMap<PathBuf, (FileMode, ObjectSha)> = BTreeMap::new();
    for e in idx.entries() {
        let path = e.decode_entry_path();
        expected.insert(path, (e.file_mode(), e.obj_name().clone()));
    }

    // got：经 from_index_file → DFS 收集 blob 叶子
    let root = TreeNode::from_index_file(&idx).expect("from_index_file");
    let got = collect_blob_leaves_from_root(&root);

    assert_eq!(got.len(), idx.entries().len(), "leaf count == index entries");
    assert_eq!(got.len(), expected.len());
    for (path, exp) in &expected {
        assert_eq!(got.get(path), Some(exp), "{}", path.display());
    }
    for path in got.keys() {
        assert!(expected.contains_key(path), "unexpected leaf {}", path.display());
    }
}

/// 流程：创建多种文件（同 cmp_read_tree_matches_git_ls_tree 的 fixture）
///       → git init → git add
///       → gift parse_index_file → from_index_file → write_tree 生成根 tree OID
///       → git write-tree 生成根 tree OID
///       → 断言两个 OID 完全一致（逐字节兼容）
///       → 对根 tree 及子 tree，逐条与 git ls-tree -z 对拍（mode、type、sha）
///
/// 这是对 git add + git write-tree 完整流程的对拍测试：gift 必须产出与 git 相同的 OID。
#[cfg(unix)]
#[test]
fn from_index_file_write_tree_matches_git_write_tree() {
    use std::os::unix::fs::symlink;

    let repo = make_test_repo("index_write_tree");

    fs::write(repo.worktree.join("a.txt"), "hello\n").unwrap();
    fs::create_dir_all(repo.worktree.join("foo").join("啊")).unwrap();
    fs::write(repo.worktree.join("foo").join("啊").join("bar"), "你好！aa\n").unwrap();
    fs::write(repo.worktree.join("script.sh"), "#!/bin/sh\necho ok\n").unwrap();
    symlink("a.txt", repo.worktree.join("link")).expect("symlink");

    let chmod_ok = Command::new("chmod")
        .args(["+x", "script.sh"])
        .current_dir(&repo.worktree)
        .status()
        .expect("chmod");
    assert!(chmod_ok.success());

    run_git(&repo.worktree, &["init"]);
    run_git(&repo.worktree, &["add", "."]);

    // gift 写 tree，git 写 tree，对比根 OID
    let idx = index_file::parse_index_file(&repo.git_abs.join("index")).unwrap();
    let root = TreeNode::from_index_file(&idx).expect("from_index_file");
    let gift_root_oid = root.write_tree_return_entry(&repo.git_abs, true).object_name;

    let want_hex = git_stdout(&repo.worktree, &["write-tree"])
        .lines().next().expect("write-tree").trim().to_string();
    assert_eq!(want_hex.len(), 40);
    assert_eq!(
        hex::encode(gift_root_oid.as_bytes()),
        want_hex,
        "root tree oid vs git write-tree"
    );

    let loose = crate::git_paths::loose_object_path(&repo.git_abs, &want_hex);
    assert!(loose.is_file(), "root loose object exists");

    let cat_t = git_stdout(&repo.worktree, &["cat-file", "-t", &want_hex]).trim().to_string();
    assert_eq!(cat_t, "tree");

    // 逐层对拍 tree entries
    let expected_root = git_ls_tree_map(&repo.worktree, &want_hex);
    let tree_root = TreeObject::read_loose_tree(&repo.git_abs, &want_hex).unwrap();
    assert_eq!(tree_root.entries().len(), expected_root.len());

    for (name, entry) in tree_root.entries() {
        let key = name.to_str().expect("utf-8 name");
        let (mode, obj_type, sha) = expected_root.get(key).expect("ls-tree key");
        assert_eq!(mode_word_to_file_mode(mode), entry.file_mode, "mode {key}");
        assert_eq!(hex::encode(entry.object_name.as_bytes()), *sha, "sha {key}");
        match entry.file_mode {
            FileMode::Directory => assert_eq!(obj_type, "tree"),
            FileMode::SymbolicLink | FileMode::NExecRegularFile | FileMode::ExecRegularFile => {
                assert_eq!(obj_type, "blob");
            }
            _ => panic!("unexpected mode"),
        }
    }

    let (_, _, foo_tree_sha) = expected_root.get("foo").expect("foo");
    let expected_foo = git_ls_tree_map(&repo.worktree, foo_tree_sha);
    let tree_foo = TreeObject::read_loose_tree(&repo.git_abs, foo_tree_sha).unwrap();
    assert_eq!(tree_foo.entries().len(), expected_foo.len());
    assert_eq!(tree_foo.entries().len(), 1);

    let (_, _, ah_tree_sha) = expected_foo.get("啊").expect("啊");
    let expected_ah = git_ls_tree_map(&repo.worktree, ah_tree_sha);
    let tree_ah = TreeObject::read_loose_tree(&repo.git_abs, ah_tree_sha).unwrap();
    assert_eq!(tree_ah.entries().len(), expected_ah.len());
    let bar_entry = tree_ah.entries().get(OsStr::new("bar")).expect("bar");
    assert_eq!(bar_entry.file_mode, FileMode::NExecRegularFile);
    let (_, _, bar_sha) = expected_ah.get("bar").unwrap();
    assert_eq!(hex::encode(bar_entry.object_name.as_bytes()), *bar_sha);
}
