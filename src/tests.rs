use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::index;
use crate::index::index_tree::{IndexRootTree, TreeNode};
use flate2::bufread::ZlibDecoder;

use crate::commit;
use crate::git_paths::branch_ref_path;
use crate::object::{commit_tree, CommitIdentity, CommitObject, FileMode, ObjectSha, TreeObject};
use crate::reference::{read_ref, update_ref};
use crate::symbolic_ref::{read_symbolic_ref, write_symbolic_ref, SymbolicRef};

use crate::status;

/// `run_git` 在 worktree 下创建的标准 git 目录（相对 worktree，与 `.gift` 等区分）
fn test_git_dir() -> &'static Path {
    Path::new(".git")
}

/// `commit` 测试用固定身份，避免读写进程环境变量。
fn test_commit_identity() -> CommitIdentity {
    CommitIdentity {
        name: "Gift Test".into(),
        email: "gift@test.local".into(),
        unix_time: 1_700_000_000,
        tz: "+0800".into(),
    }
}

pub fn make_case_dir(case_name: &str) -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let root = PathBuf::from("target")
        .join("inspect")
        .join(format!("{case_name}-{ts}"));

    fs::create_dir_all(&root).unwrap();
    root
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("failed to run git");

    assert!(status.success(), "git {:?} failed", args);
}

/// zlib 解压后的完整 loose object 字节（含 `type len\0` 头）
fn decompress_loose_object(git_dir: &Path, hex_oid: &str) -> Vec<u8> {
    let loose = crate::git_paths::loose_object_path(git_dir, hex_oid);
    let f = fs::File::open(&loose).expect("open loose object");
    let mut zlib = ZlibDecoder::new(std::io::BufReader::new(f));
    let mut raw = Vec::new();
    zlib.read_to_end(&mut raw).expect("decompress");
    raw
}

/// 在指定工作目录下调用系统的 git commit-tree，并固定 author/committer 环境变量，
/// 返回新产生的 commit 的 SHA（40 位 hex 字符串）。
/// 参数:
///      dir：仓库根目录,   
///      tree：根 tree 的 OID 字符串（通常来自 git write-tree),
///      parents：父 commit 列表
///      msg：-m 后面的提交说明
fn git_commit_tree_with_env(dir: &Path, tree: &str, parents: &[&str], msg: &str) -> String {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test Author")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0800")
        .env("GIT_COMMITTER_NAME", "Test Committer")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .env("GIT_COMMITTER_DATE", "1700000000 +0800")
        .arg("commit-tree")
        .arg(tree);
    for p in parents {
        cmd.arg("-p").arg(p);
    }
    cmd.args(["-m", msg]);
    let out = cmd.output().expect("git commit-tree");
    assert!(
        out.status.success(),
        "git commit-tree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf-8")
        .lines()
        .next()
        .expect("commit oid line")
        .trim()
        .to_string()
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8 stdout")
}

/// `git ls-tree` 一行：`100644 blob <sha>\tname`
fn parse_ls_tree_line(line: &str) -> (String, String, String, String) {
    let (left, name) = line
        .split_once('\t')
        .expect("ls-tree line must contain tab");
    let mut it = left.split_whitespace();
    let mode = it.next().expect("mode").to_string();
    let obj_type = it.next().expect("type").to_string();
    let sha = it.next().expect("sha").to_string();
    assert!(it.next().is_none(), "unexpected extra fields: {line:?}");
    (mode, obj_type, sha, name.to_string())
}

fn mode_word_to_file_mode(mode: &str) -> FileMode {
    match mode {
        "100644" => FileMode::NExecRegularFile,
        "100755" => FileMode::ExecRegularFile,
        "120000" => FileMode::SymbolicLink,
        "160000" => FileMode::Gitlink,
        // `git ls-tree` 对 tree 常用 6 位 `040000`；对象里 on-disk 常为 `40000`
        "40000" | "040000" => FileMode::Directory,
        other => panic!("unexpected ls-tree mode {other:?}"),
    }
}

/// 使用 `ls-tree -z`，路径为原始 UTF-8（非 ASCII 不会被 `"\345..."` 转义）。
fn git_ls_tree_map(dir: &Path, tree_oid: &str) -> BTreeMap<String, (String, String, String)> {
    let stdout = git_stdout(dir, &["ls-tree", "-z", tree_oid]);
    let mut m = BTreeMap::new();
    for chunk in stdout.split_terminator('\0') {
        if chunk.is_empty() {
            continue;
        }
        let (mode, obj_type, sha, name) = parse_ls_tree_line(chunk);
        m.insert(name, (mode, obj_type, sha));
    }
    m
}

/// DFS 收集所有 blob 叶子：相对路径 → (mode, oid)。
fn collect_blob_leaves_from_tree(
    rel: PathBuf,
    node: &TreeNode,
    out: &mut BTreeMap<PathBuf, (FileMode, ObjectSha)>,
) {
    match node {
        TreeNode::Blob(leaf) => {
            out.insert(rel, (leaf.file_mode(), leaf.object_name().clone()));
        }
        TreeNode::Tree(map) => {
            for (seg, child) in map {
                let mut next = rel.clone();
                next.push(seg);
                collect_blob_leaves_from_tree(next, child, out);
            }
        }
    }
}

fn collect_blob_leaves_from_root(root: &IndexRootTree) -> BTreeMap<PathBuf, (FileMode, ObjectSha)> {
    let mut out = BTreeMap::new();
    for (seg, child) in root.root_children() {
        let mut rel = PathBuf::new();
        rel.push(seg);
        collect_blob_leaves_from_tree(rel, child, &mut out);
    }
    out
}

#[test]
fn cmp_init() {
    let case_dir = make_case_dir("init");

    // 1. 构造测试需要的文件
    std::fs::write(case_dir.join("a.txt"), "hello\n").unwrap();
    std::fs::create_dir_all(case_dir.join("foo")).unwrap();
    std::fs::write(case_dir.join("foo").join("main.rs"), "fn main() {}\n").unwrap();

    // 2. 真实 git
    run_git(&case_dir, &["init"]);

    // 4. 我的实现
    super::init(&(&case_dir.join(".gift"))).unwrap();

    // 5. 做断言
}

#[test]
fn cmp_write_obj() {
    let case_dir = make_case_dir("write_hash_object");

    // 1. 构造测试需要的文件
    std::fs::write(case_dir.join("a.txt"), "hello\n").unwrap();
    std::fs::create_dir_all(case_dir.join("foo")).unwrap();
    std::fs::write(case_dir.join("foo").join("bar"), "你好！aa\n").unwrap();

    // 2. 真实 git
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["hash-object", "-w", "a.txt"]);
    run_git(&case_dir, &["hash-object", "-w", "foo/bar"]);

    // 4. 我的实现
    let gift_dir = case_dir.join(".gift");
    super::init(&gift_dir).unwrap();

    let my_obj_path = case_dir.join("a.txt");
    let (obj_hash, obj_content) = super::object::hash_object(my_obj_path).unwrap();
    super::object::write_hash_object(&gift_dir, &obj_hash, &obj_content).unwrap();

    let my_obj_path = case_dir.join("foo/bar");
    let (obj_hash, obj_content) = super::object::hash_object(my_obj_path).unwrap();
    super::object::write_hash_object(&gift_dir, &obj_hash, &obj_content).unwrap();
}

#[test]
fn parse_index() {
    let case_dir = make_case_dir("parse_index");

    // 1. 构造测试需要的文件
    std::fs::write(case_dir.join("a.txt"), "hello\n").unwrap();
    std::fs::create_dir_all(case_dir.join("foo").join("啊")).unwrap();
    std::fs::write(case_dir.join("foo").join("啊").join("bar"), "你好！aa\n").unwrap();

    // 2. git指令
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "."]);

    // 3. 解析index_file
    index::display_index_file(&case_dir.join(".git/index")).unwrap();
}

/// `stage_paths` 写入 objects + index 后，用 `parse_index_file` 读回并校验路径、oid 及 loose object 文件存在。
#[test]
fn stage_paths_roundtrip_index() {
    // 准备工作环境
    let case_dir = make_case_dir("stage_roundtrip");
    let work_tree = fs::canonicalize(&case_dir).unwrap();
    let git_dir = work_tree.join(".gift");
    super::init(&git_dir).unwrap();

    fs::write(work_tree.join("top.txt"), b"hello\n").unwrap();
    fs::create_dir_all(work_tree.join("sub")).unwrap();
    fs::write(work_tree.join("sub").join("nested.txt"), b"x\n").unwrap();

    // 执行stage操作
    let inputs = vec![work_tree.join("top.txt"), work_tree.join("sub")];
    super::staging::stage_paths(&git_dir, &work_tree, &inputs, true).unwrap();

    // 拿到并检验index_file
    let idx = index::parse_index_file(git_dir.join("index")).unwrap();
    assert_eq!(idx.version(), 2, "index version");
    assert_eq!(idx.entries().len(), 2, "entry count");

    let mut paths: Vec<Vec<u8>> = idx.entries().iter().map(|e| e.path().to_vec()).collect();
    paths.sort();
    assert_eq!(
        paths,
        vec![b"sub/nested.txt".to_vec(), b"top.txt".to_vec()],
        "index paths"
    );

    for e in idx.entries() {
        let rel = std::str::from_utf8(e.path()).expect("entry path utf-8");
        let disk = work_tree.join(rel);
        let (sha, _) = super::object::hash_object(&disk).unwrap();
        assert_eq!(
            hex::encode(sha.as_bytes()),
            hex::encode(e.obj_name().as_bytes()),
            "blob oid mismatch for {rel}"
        );

        let hex_oid = hex::encode(e.obj_name().as_bytes());
        let loose = crate::git_paths::loose_object_path(&git_dir, &hex_oid);
        assert!(
            loose.is_file(),
            "loose object file should exist: {}",
            loose.display()
        );
    }
}

/// 用真实 `git write-tree` 产生的 tree loose object，校验 `read_object_type` + `read_tree` 与 `git ls-tree` 一致（含 `100755` 与 `120000`）。
#[cfg(unix)]
#[test]
fn cmp_read_tree_matches_git_ls_tree() {
    use std::os::unix::fs::symlink;

    // 准备测试环境
    let case_dir = make_case_dir("read_tree");
    let git_dir = case_dir.join(".git");

    fs::write(case_dir.join("a.txt"), "hello\n").unwrap();
    fs::create_dir_all(case_dir.join("foo").join("啊")).unwrap();
    fs::write(case_dir.join("foo").join("啊").join("bar"), "你好！aa\n").unwrap();
    fs::write(case_dir.join("script.sh"), "#!/bin/sh\necho ok\n").unwrap();
    symlink("a.txt", case_dir.join("link")).expect("symlink link -> a.txt");

    let chmod_ok = Command::new("chmod")
        .args(["+x", "script.sh"])
        .current_dir(&case_dir)
        .status()
        .expect("chmod");
    assert!(chmod_ok.success(), "chmod +x script.sh");

    // 执行git指令
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "."]);

    let root_hex = git_stdout(&case_dir, &["write-tree"])
        .lines()
        .next()
        .expect("write-tree line")
        .trim()
        .to_string();
    assert_eq!(root_hex.len(), 40, "root tree oid hex length");

    // 得到git cat-file的结果
    let cat_t = git_stdout(&case_dir, &["cat-file", "-t", &root_hex])
        .trim()
        .to_string();
    assert_eq!(cat_t, "tree");

    // 得到git ls-tree的结果
    let expected_root = git_ls_tree_map(&case_dir, &root_hex);
    // 用我们实现的函数去读取得到的结果
    let tree_root = TreeObject::read_loose_tree(&git_dir, &root_hex);

    assert_eq!(
        hex::encode(tree_root.object_name().as_bytes()),
        root_hex,
        "TreeObject carries the opened oid"
    );
    assert_eq!(
        tree_root.entries().len(),
        expected_root.len(),
        "root entry count vs git ls-tree"
    );

    for (name, entry) in tree_root.entries() {
        let key = name.to_str().expect("utf-8 name in fixture");
        let (mode, obj_type, sha) = expected_root
            .get(key)
            .unwrap_or_else(|| panic!("unexpected entry {key:?}"));
        assert_eq!(
            mode_word_to_file_mode(mode),
            entry.file_mode,
            "mode for {key}"
        );
        assert_eq!(hex::encode(entry.object_name.as_bytes()), *sha, "sha {key}");
        match entry.file_mode {
            FileMode::Directory => assert_eq!(obj_type, "tree"),
            FileMode::SymbolicLink | FileMode::NExecRegularFile | FileMode::ExecRegularFile => {
                assert_eq!(obj_type, "blob");
            }
            _ => panic!("unexpected FileMode in fixture"),
        }
    }

    // 子 tree：foo -> 啊 -> bar
    let (_, _, foo_tree_sha) = expected_root.get("foo").expect("foo tree");
    let expected_foo = git_ls_tree_map(&case_dir, foo_tree_sha);
    let tree_foo = TreeObject::read_loose_tree(&git_dir, foo_tree_sha);
    assert_eq!(tree_foo.entries().len(), expected_foo.len());
    assert_eq!(tree_foo.entries().len(), 1, "foo/ only contains 啊");

    let (_, _, ah_tree_sha) = expected_foo.get("啊").expect("啊 tree under foo");
    let expected_ah = git_ls_tree_map(&case_dir, ah_tree_sha);
    let tree_ah = TreeObject::read_loose_tree(&git_dir, ah_tree_sha);
    assert_eq!(tree_ah.entries().len(), 1);
    assert_eq!(tree_ah.entries().len(), expected_ah.len());

    let bar_entry = tree_ah.entries().get(OsStr::new("bar")).expect("bar blob");
    assert_eq!(bar_entry.file_mode, FileMode::NExecRegularFile);
    let (_, _, bar_sha) = expected_ah.get("bar").unwrap();
    assert_eq!(hex::encode(bar_entry.object_name.as_bytes()), *bar_sha);
}

/// 仅 `from_index_file`：内存树中每个 blob 叶子的路径、mode、OID 与 index 条目一致。
#[test]
fn from_index_file_matches_parsed_index_entries() {
    let case_dir = make_case_dir("from_index_only");
    fs::write(case_dir.join("a.txt"), "hello\n").unwrap();
    fs::create_dir_all(case_dir.join("foo").join("啊")).unwrap();
    fs::write(case_dir.join("foo").join("啊").join("bar"), "你好！aa\n").unwrap();

    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "."]);

    let idx = index::parse_index_file(case_dir.join(".git/index")).unwrap();
    let mut expected: BTreeMap<PathBuf, (FileMode, ObjectSha)> = BTreeMap::new();
    for e in idx.entries() {
        let path = e.decode_entry_path();
        expected.insert(path, (e.file_mode(), e.obj_name().clone()));
    }

    let root = IndexRootTree::from_index_file(&idx).expect("from_index_file");
    let got = collect_blob_leaves_from_root(&root);

    assert_eq!(
        got.len(),
        idx.entries().len(),
        "leaf count == index entries"
    );
    assert_eq!(got.len(), expected.len());
    for (path, exp) in &expected {
        assert_eq!(got.get(path), Some(exp), "{}", path.display());
    }
    for path in got.keys() {
        assert!(
            expected.contains_key(path),
            "unexpected leaf {}",
            path.display()
        );
    }
}

/// `from_index_file` + `write_tree`：根 OID 与 `git write-tree` 一致，且各层 tree 与 `git ls-tree -z` 一致。
#[cfg(unix)]
#[test]
fn from_index_file_write_tree_matches_git_write_tree() {
    use std::os::unix::fs::symlink;

    let case_dir = make_case_dir("index_write_tree");
    let git_dir = case_dir.join(".git");

    fs::write(case_dir.join("a.txt"), "hello\n").unwrap();
    fs::create_dir_all(case_dir.join("foo").join("啊")).unwrap();
    fs::write(case_dir.join("foo").join("啊").join("bar"), "你好！aa\n").unwrap();
    fs::write(case_dir.join("script.sh"), "#!/bin/sh\necho ok\n").unwrap();
    symlink("a.txt", case_dir.join("link")).expect("symlink");

    let chmod_ok = Command::new("chmod")
        .args(["+x", "script.sh"])
        .current_dir(&case_dir)
        .status()
        .expect("chmod");
    assert!(chmod_ok.success());

    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "."]);

    let idx = index::parse_index_file(git_dir.join("index")).unwrap();
    let root = IndexRootTree::from_index_file(&idx).expect("from_index_file");
    let gift_root_oid = root.write_tree(&git_dir, true).expect("write_tree");

    let want_hex = git_stdout(&case_dir, &["write-tree"])
        .lines()
        .next()
        .expect("write-tree")
        .trim()
        .to_string();
    assert_eq!(want_hex.len(), 40);
    assert_eq!(
        hex::encode(gift_root_oid.as_bytes()),
        want_hex,
        "root tree oid vs git write-tree"
    );

    let loose = crate::git_paths::loose_object_path(&git_dir, &want_hex);
    assert!(loose.is_file(), "root loose object exists");

    let cat_t = git_stdout(&case_dir, &["cat-file", "-t", &want_hex])
        .trim()
        .to_string();
    assert_eq!(cat_t, "tree");

    let expected_root = git_ls_tree_map(&case_dir, &want_hex);
    let tree_root = TreeObject::read_loose_tree(&git_dir, &want_hex);
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
    let expected_foo = git_ls_tree_map(&case_dir, foo_tree_sha);
    let tree_foo = TreeObject::read_loose_tree(&git_dir, foo_tree_sha);
    assert_eq!(tree_foo.entries().len(), expected_foo.len());
    assert_eq!(tree_foo.entries().len(), 1);

    let (_, _, ah_tree_sha) = expected_foo.get("啊").expect("啊");
    let expected_ah = git_ls_tree_map(&case_dir, ah_tree_sha);
    let tree_ah = TreeObject::read_loose_tree(&git_dir, ah_tree_sha);
    assert_eq!(tree_ah.entries().len(), expected_ah.len());
    let bar_entry = tree_ah.entries().get(OsStr::new("bar")).expect("bar");
    assert_eq!(bar_entry.file_mode, FileMode::NExecRegularFile);
    let (_, _, bar_sha) = expected_ah.get("bar").unwrap();
    assert_eq!(hex::encode(bar_entry.object_name.as_bytes()), *bar_sha);
}

/// `git commit-tree` 生成的 loose commit：`read_loose_commit` 字段一致，且 `to_binary` 与解压字节一致。
#[test]
fn read_commit_matches_git_commit_tree() {
    let case_dir = make_case_dir("read_commit");
    let git_dir = case_dir.join(".git");

    fs::write(case_dir.join("f.txt"), "content\n").unwrap();
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "f.txt"]);
    let tree_hex = git_stdout(&case_dir, &["write-tree"])
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_string();

    let c1 = git_commit_tree_with_env(&case_dir, &tree_hex, &[], "first commit");
    assert_eq!(c1.len(), 40);

    let raw1 = decompress_loose_object(&git_dir, &c1);
    let parsed1 = CommitObject::read_loose_commit(&git_dir, &c1);
    assert_eq!(hex::encode(parsed1.object_name().as_bytes()), c1);
    assert_eq!(parsed1.parents.len(), 0);
    assert_eq!(parsed1.message, b"first commit\n");
    assert_eq!(parsed1.author.name, "Test Author");
    assert_eq!(parsed1.committer.email, "committer@example.com");
    assert_eq!(parsed1.to_binary(), raw1);

    let c2 = git_commit_tree_with_env(&case_dir, &tree_hex, &[&c1], "second commit");
    let parsed2 = CommitObject::read_loose_commit(&git_dir, &c2);
    assert_eq!(parsed2.parents.len(), 1);
    assert_eq!(
        hex::encode(parsed2.parents[0].as_bytes()),
        c1,
        "single parent oid"
    );
    assert_eq!(parsed2.to_binary(), decompress_loose_object(&git_dir, &c2));
}

/// 合并提交两个 parent：顺序与 `git commit-tree -p A -p B` 一致。
#[test]
fn read_commit_merge_two_parents() {
    let case_dir = make_case_dir("commit_merge_parents");
    let git_dir = case_dir.join(".git");

    fs::write(case_dir.join("x.txt"), "x\n").unwrap();
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "x.txt"]);
    let tree_hex = git_stdout(&case_dir, &["write-tree"])
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_string();

    let p1 = git_commit_tree_with_env(&case_dir, &tree_hex, &[], "branch one");
    let p2 = git_commit_tree_with_env(&case_dir, &tree_hex, &[], "branch two");
    let merge = git_commit_tree_with_env(&case_dir, &tree_hex, &[&p1, &p2], "merge both");

    let parsed = CommitObject::read_loose_commit(&git_dir, &merge);
    assert_eq!(parsed.parents.len(), 2);
    assert_eq!(hex::encode(parsed.parents[0].as_bytes()), p1);
    assert_eq!(hex::encode(parsed.parents[1].as_bytes()), p2);
    assert_eq!(
        parsed.to_binary(),
        decompress_loose_object(&git_dir, &merge)
    );
}

/// `commit_tree`：写入 objects 后 OID 与 `to_binary` 的 SHA1 一致。
#[test]
fn commit_tree_writes_loose_object() {
    let case_dir = make_case_dir("commit_tree_fn");
    let git_dir = case_dir.join(".git");

    fs::write(case_dir.join("blob.txt"), "blob\n").unwrap();
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "blob.txt"]);
    let tree_hex = git_stdout(&case_dir, &["write-tree"])
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_string();
    let tree_oid: [u8; 20] = hex::decode(&tree_hex).unwrap().try_into().unwrap();
    let tree_sha = ObjectSha::SHA1(tree_oid);

    let commit = CommitObject::new(
        tree_sha.clone(),
        Vec::new(),
        CommitIdentity {
            name: "A".into(),
            email: "a@b.c".into(),
            unix_time: 1_700_000_001,
            tz: "+0000".into(),
        },
        CommitIdentity {
            name: "C".into(),
            email: "c@d.e".into(),
            unix_time: 1_700_000_001,
            tz: "+0000".into(),
        },
        Vec::new(),
        b"gift commit-tree test\n".to_vec(),
    );

    let oid = commit_tree(&git_dir, &commit).expect("commit_tree");
    let hex_out = hex::encode(oid.as_bytes());
    let loose = crate::git_paths::loose_object_path(&git_dir, &hex_out);
    assert!(loose.is_file(), "loose commit written");

    let round = CommitObject::read_loose_commit(&git_dir, &hex_out);
    assert_eq!(round.tree, tree_sha);
    assert_eq!(round.message, commit.message);
    assert_eq!(
        decompress_loose_object(&git_dir, &hex_out),
        commit.to_binary()
    );
}

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

/// `read_ref` 与 `git update-ref` 一致；`update_ref` 写入后 `git rev-parse` 一致。
#[test]
fn read_ref_and_update_ref_match_git() {
    let case_dir = make_case_dir("git_ref_update");

    fs::write(case_dir.join("f.txt"), "x\n").unwrap();
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "f.txt"]);
    let tree_hex = git_stdout(&case_dir, &["write-tree"])
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_string();
    let c0 = git_commit_tree_with_env(&case_dir, &tree_hex, &[], "c0");
    let ref_path = branch_ref_path(test_git_dir(), "mine");

    run_git(&case_dir, &["update-ref", &format!("refs/heads/mine"), &c0]);

    let r = read_ref(&case_dir, test_git_dir(), &ref_path).expect("read_ref");
    assert_eq!(hex::encode(r.commit_id.as_bytes()), c0);

    let c1 = git_commit_tree_with_env(&case_dir, &tree_hex, &[&c0], "c1");
    update_ref(
        &case_dir,
        test_git_dir(),
        &ref_path,
        &ObjectSha::SHA1(hex::decode(&c1).unwrap().try_into().unwrap()),
    )
    .expect("update_ref");

    let rev = git_stdout(&case_dir, &["rev-parse", "refs/heads/mine"])
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(rev, c1);
    let r2 =
        read_ref(&case_dir, test_git_dir(), &ref_path).expect("read_ref after gift update_ref");
    assert_eq!(hex::encode(r2.commit_id.as_bytes()), c1);
}

/// `update_ref` 要求目标为 commit，不能指向 tree。
#[test]
fn update_ref_rejects_non_commit_object() {
    let case_dir = make_case_dir("ref_reject_tree");

    fs::write(case_dir.join("f.txt"), "y\n").unwrap();
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "f.txt"]);
    let tree_hex = git_stdout(&case_dir, &["write-tree"])
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_string();
    let tree_sha = ObjectSha::SHA1(hex::decode(&tree_hex).unwrap().try_into().unwrap());
    let err = update_ref(
        &case_dir,
        test_git_dir(),
        branch_ref_path(test_git_dir(), "bad"),
        &tree_sha,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("commit") || err.to_string().contains("tree"),
        "unexpected err: {err:?}"
    );
}

/// 含 `ref:` 的 symbolic ref 文件不由 `read_ref` 解析。
#[test]
fn read_ref_rejects_symbolic_file() {
    let case_dir = make_case_dir("ref_reject_sym");
    run_git(&case_dir, &["init"]);

    let p = branch_ref_path(test_git_dir(), "sym");
    let full = case_dir.join(&p);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(&full, "ref: refs/heads/main\n").unwrap();

    let err = read_ref(&case_dir, test_git_dir(), &p).unwrap_err();
    assert!(
        err.to_string().contains("symbolic") || err.to_string().contains("direct"),
        "unexpected err: {err:?}"
    );
}

/// `git symbolic-ref` 写入 HEAD 后，`read_symbolic_ref` 得到 worktree 相对路径。
#[test]
fn read_symbolic_ref_matches_git() {
    let case_dir = make_case_dir("sym_read_git");
    fs::write(case_dir.join("f.txt"), "a\n").unwrap();
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "f.txt"]);
    let tree_hex = git_stdout(&case_dir, &["write-tree"])
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_string();
    let c0 = git_commit_tree_with_env(&case_dir, &tree_hex, &[], "root");
    run_git(&case_dir, &["update-ref", "refs/heads/main", &c0]);
    run_git(&case_dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let head_path = test_git_dir().join("HEAD");
    let s = read_symbolic_ref(&case_dir, &head_path).expect("read_symbolic_ref");
    assert_eq!(s.ref_name, "refs/heads/main");
}

/// `write_symbolic_ref` 后，`git symbolic-ref -q HEAD` 读到目标 ref 名。
#[test]
fn write_symbolic_ref_matches_git() {
    let case_dir = make_case_dir("sym_write_git");
    fs::write(case_dir.join("g.txt"), "b\n").unwrap();
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "g.txt"]);
    let tree_hex = git_stdout(&case_dir, &["write-tree"])
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_string();
    let c0 = git_commit_tree_with_env(&case_dir, &tree_hex, &[], "tip");
    run_git(&case_dir, &["update-ref", "refs/heads/foo", &c0]);

    let sym = SymbolicRef {
        ref_name: "refs/heads/foo".into(),
    };
    write_symbolic_ref(&case_dir, test_git_dir().join("HEAD"), &sym).expect("write_symbolic_ref");

    let got = git_stdout(&case_dir, &["symbolic-ref", "-q", "HEAD"])
        .trim()
        .to_string();
    assert_eq!(got, "refs/heads/foo");
}

/// 分支 symbolic HEAD：首次提交无 parent；第二次提交单 parent 且为首次 OID，`HEAD` 仍为 symbolic。
#[test]
fn commit_on_branch_first_then_second() {
    let case_dir = make_case_dir("commit_branch_twice");
    let id = test_commit_identity();
    let git_bare = case_dir.join(".git");

    fs::write(case_dir.join("track.txt"), "v1\n").unwrap();
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "track.txt"]);

    let oid1 = commit::commit(
        &case_dir,
        test_git_dir(),
        id.clone(),
        id.clone(),
        "first commit".into(),
    )
    .expect("first commit");

    let hex1 = hex::encode(oid1.as_bytes());
    let head_git = git_stdout(&case_dir, &["rev-parse", "HEAD"])
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head_git, hex1, "HEAD after first commit");

    let sym_out = git_stdout(&case_dir, &["symbolic-ref", "-q", "HEAD"]);
    let sym = sym_out.trim();
    assert!(sym.starts_with("refs/heads/"), "still symbolic: {sym}");

    let p1 = CommitObject::read_loose_commit(&git_bare, &hex1);
    assert!(p1.parents.is_empty(), "root has no parent");
    assert_eq!(p1.message, b"first commit\n");

    fs::write(case_dir.join("track.txt"), "v2\n").unwrap();
    run_git(&case_dir, &["add", "track.txt"]);

    let oid2 = commit::commit(
        &case_dir,
        test_git_dir(),
        id.clone(),
        id.clone(),
        "second".into(),
    )
    .expect("second commit");

    let hex2 = hex::encode(oid2.as_bytes());
    let head2 = git_stdout(&case_dir, &["rev-parse", "HEAD"])
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head2, hex2);

    let p2 = CommitObject::read_loose_commit(&git_bare, &hex2);
    assert_eq!(p2.parents.len(), 1, "second commit has one parent");
    assert_eq!(hex::encode(p2.parents[0].as_bytes()), hex1);
    assert_eq!(p2.message, b"second\n");
}

/// detached `HEAD`：`commit` 后 `HEAD` 为裸 OID，新 commit 的 parent 为 detached 指向的旧 OID。
#[test]
fn commit_on_detached_head_updates_oid_and_parent() {
    let case_dir = make_case_dir("commit_detached");
    let id = test_commit_identity();
    let git_bare = case_dir.join(".git");

    fs::write(case_dir.join("only.txt"), "a\n").unwrap();
    run_git(&case_dir, &["init"]);
    run_git(&case_dir, &["add", "only.txt"]);
    let tree_hex = git_stdout(&case_dir, &["write-tree"])
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_string();
    let c0 = git_commit_tree_with_env(&case_dir, &tree_hex, &[], "seed");
    fs::write(case_dir.join(".git/HEAD"), format!("{c0}\n")).unwrap();

    fs::write(case_dir.join("only.txt"), "b\n").unwrap();
    run_git(&case_dir, &["add", "only.txt"]);

    let oid_new = commit::commit(
        &case_dir,
        test_git_dir(),
        id.clone(),
        id.clone(),
        "while detached".into(),
    )
    .expect("commit detached");

    let hex_new = hex::encode(oid_new.as_bytes());
    let raw_head = fs::read_to_string(case_dir.join(".git/HEAD")).unwrap();
    assert_eq!(raw_head.trim(), hex_new, "HEAD must be raw oid");

    let parsed = CommitObject::read_loose_commit(&git_bare, &hex_new);
    assert_eq!(parsed.parents.len(), 1);
    assert_eq!(hex::encode(parsed.parents[0].as_bytes()), c0);
    assert_eq!(parsed.message, b"while detached\n");
}

#[test]
fn checkout_commit_detaches_and_restores_worktree_without_symlinks() {
    let case_dir = make_case_dir("checkout_detached_basic");
    let id = test_commit_identity();

    run_git(&case_dir, &["init"]);
    let git_bare = fs::canonicalize(case_dir.join(".git")).unwrap();

    fs::write(case_dir.join("a.txt"), "v1\n").unwrap();
    fs::write(case_dir.join("old.txt"), "old\n").unwrap();
    run_git(&case_dir, &["add", "-A"]);
    let c1 = commit::commit(&case_dir, &git_bare, id.clone(), id.clone(), "first".into())
        .expect("commit c1");

    fs::write(case_dir.join("a.txt"), "v2\n").unwrap();
    fs::remove_file(case_dir.join("old.txt")).unwrap();
    fs::write(case_dir.join("new.txt"), "new\n").unwrap();
    run_git(&case_dir, &["add", "-A"]);
    let _c2 = commit::commit(
        &case_dir,
        &git_bare,
        id.clone(),
        id.clone(),
        "second".into(),
    )
    .expect("commit c2");

    crate::checkout::checkout_commit(&case_dir, test_git_dir(), c1.clone())
        .expect("checkout c1 detached");

    assert_eq!(fs::read_to_string(case_dir.join("a.txt")).unwrap(), "v1\n");
    assert_eq!(
        fs::read_to_string(case_dir.join("old.txt")).unwrap(),
        "old\n"
    );
    assert!(!case_dir.join("new.txt").exists(), "new.txt removed");

    let c1_hex = hex::encode(c1.as_bytes());
    let raw_head = fs::read_to_string(case_dir.join(".git/HEAD")).unwrap();
    assert_eq!(raw_head.trim(), c1_hex, "HEAD is detached to c1");

    let checked_out_tree = git_stdout(&case_dir, &["write-tree"])
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_string();
    let c1_obj = CommitObject::read_loose_commit(&git_bare, &c1_hex);
    assert_eq!(
        checked_out_tree,
        hex::encode(c1_obj.tree.as_bytes()),
        "index tree after checkout equals c1 tree"
    );
}

#[test]
fn checkout_branch_restores_tip_and_keeps_symbolic_head() {
    let case_dir = make_case_dir("checkout_branch_basic");
    let id = test_commit_identity();

    run_git(&case_dir, &["init"]);
    let git_bare = fs::canonicalize(case_dir.join(".git")).unwrap();

    fs::write(case_dir.join("branch.txt"), "side\n").unwrap();
    fs::write(case_dir.join("common.txt"), "side version\n").unwrap();
    run_git(&case_dir, &["add", "-A"]);
    let side_tip = commit::commit(
        &case_dir,
        &git_bare,
        id.clone(),
        id.clone(),
        "side tip".into(),
    )
    .expect("commit side tip");
    update_ref(
        &case_dir,
        test_git_dir(),
        branch_ref_path(test_git_dir(), "side"),
        &side_tip,
    )
    .expect("create side branch");

    fs::remove_file(case_dir.join("branch.txt")).unwrap();
    fs::write(case_dir.join("common.txt"), "main version\n").unwrap();
    fs::write(case_dir.join("main.txt"), "main\n").unwrap();
    run_git(&case_dir, &["add", "-A"]);
    let _main_tip = commit::commit(
        &case_dir,
        &git_bare,
        id.clone(),
        id.clone(),
        "main tip".into(),
    )
    .expect("commit main tip");

    crate::checkout::checkout_branch(&case_dir, test_git_dir(), "side")
        .expect("checkout side branch");

    assert_eq!(
        fs::read_to_string(case_dir.join("branch.txt")).unwrap(),
        "side\n"
    );
    assert_eq!(
        fs::read_to_string(case_dir.join("common.txt")).unwrap(),
        "side version\n"
    );
    assert!(
        !case_dir.join("main.txt").exists(),
        "main-only file removed"
    );

    let head = fs::read_to_string(case_dir.join(".git/HEAD")).unwrap();
    assert_eq!(head.trim(), "ref: refs/heads/side");
    let side_ref = fs::read_to_string(case_dir.join(".git/refs/heads/side")).unwrap();
    assert_eq!(side_ref.trim(), hex::encode(side_tip.as_bytes()));
}

#[test]
fn checkout_reports_untracked_file_conflict_before_writing() {
    let case_dir = make_case_dir("checkout_untracked_conflict");
    let id = test_commit_identity();

    run_git(&case_dir, &["init"]);
    let git_bare = fs::canonicalize(case_dir.join(".git")).unwrap();

    fs::write(case_dir.join("conflict.txt"), "tracked in target\n").unwrap();
    fs::write(case_dir.join("stable.txt"), "target stable\n").unwrap();
    run_git(&case_dir, &["add", "-A"]);
    let target = commit::commit(
        &case_dir,
        &git_bare,
        id.clone(),
        id.clone(),
        "target".into(),
    )
    .expect("commit target");

    fs::remove_file(case_dir.join("conflict.txt")).unwrap();
    fs::write(case_dir.join("stable.txt"), "current stable\n").unwrap();
    run_git(&case_dir, &["add", "-A"]);
    let current = commit::commit(
        &case_dir,
        &git_bare,
        id.clone(),
        id.clone(),
        "current".into(),
    )
    .expect("commit current");

    fs::write(case_dir.join("conflict.txt"), "untracked local\n").unwrap();
    let err = crate::checkout::checkout_commit(&case_dir, test_git_dir(), target)
        .expect_err("checkout should reject untracked conflict");
    assert!(
        err.to_string().contains("untracked working tree file"),
        "unexpected error: {err:?}"
    );

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
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(head_commit, current_hex);
}

#[test]
fn test_status_basic() {
    let case_dir = make_case_dir("status_basic");
    
    run_git(&case_dir, &["init"]);
    
    // 创建文件并提交
    fs::write(case_dir.join("file.txt"), "v1\n").unwrap();
    run_git(&case_dir, &["add", "file.txt"]);
    run_git(&case_dir, &["commit", "-m", "initial"]);
    
    // 修改文件
    fs::write(case_dir.join("file.txt"), "v2\n").unwrap();
    
    let worktree = &case_dir;
    let git_dir = test_git_dir();
    let status = status::status(worktree, git_dir).expect("status failed");
    
    // 验证状态
    assert_eq!(status.staged.len(), 0, "should have no staged changes");
    assert_eq!(status.unstaged.len(), 1, "should have 1 unstaged change");
    assert_eq!(status.untracked.len(), 0, "should have no untracked files");
    
    if let Some(entry) = status.unstaged.first() {
        assert_eq!(entry.change_type, status::ChangeType::Modified);
        assert!(entry.path.ends_with("file.txt"));
    }
}

#[test]
fn test_load_head_tree_debug() {
    let case_dir = make_case_dir("load_head_tree_debug");
    
    // 初始化仓库并创建一次提交
    run_git(&case_dir, &["init"]);
    
    fs::write(case_dir.join("file1.txt"), "content 1\n").unwrap();
    fs::write(case_dir.join("file2.txt"), "content 2\n").unwrap();
    fs::create_dir_all(case_dir.join("subdir")).unwrap();
    fs::write(case_dir.join("subdir").join("file3.txt"), "content 3\n").unwrap();
    
    run_git(&case_dir, &["add", "."]);
    run_git(&case_dir, &["commit", "-m", "initial commit"]);
    
    let worktree = &case_dir;
    let git_dir = Path::new(".git");
    let git_abs = crate::git_paths::resolve_git_dir(worktree, git_dir);
    
    eprintln!("\n=== Testing load_head_tree ===");
    eprintln!("worktree: {:?}", worktree);
    eprintln!("git_dir (relative): {:?}", git_dir);
    eprintln!("git_abs (absolute): {:?}", git_abs);
    
    // 步骤1: 读取 HEAD
    eprintln!("\n--- Step 1: Reading HEAD ---");
    let head = match crate::head::Head::read(worktree, git_dir) {
        Ok(h) => {
            eprintln!("✓ HEAD read successfully: {:?}", h);
            h
        }
        Err(e) => {
            eprintln!("✗ Failed to read HEAD: {}", e);
            panic!("Cannot proceed without HEAD");
        }
    };

    // 步骤2: 获取当前 commit OID
    eprintln!("\n--- Step 2: Getting current commit OID ---");
    let commit_oid = match head.current_commit(worktree, git_dir) {
        Ok(oid) => {
            eprintln!("✓ Current commit OID: {}", oid.to_string());
            oid
        }
        Err(e) => {
            eprintln!("✗ Failed to get current commit: {}", e);
            panic!("Cannot proceed without commit OID");
        }
    };
    
    // 步骤3: 读取 commit 对象
    eprintln!("\n--- Step 3: Reading commit object ---");
    let commit_hex = commit_oid.to_string();
    let commit_obj_path = git_abs.join("objects").join(&commit_hex[0..2]).join(&commit_hex[2..]);
    eprintln!("Commit object path: {:?}", commit_obj_path);
    eprintln!("Commit object exists: {}", commit_obj_path.exists());
    
    let commit_obj = CommitObject::read_loose_commit(&git_abs, &commit_hex);
    eprintln!("✓ Commit object read successfully");
    eprintln!("  - Tree OID: {}", commit_obj.tree.to_string());
    eprintln!("  - Parent count: {}", commit_obj.parents.len());
    
    // 步骤4: 读取 tree 对象
    eprintln!("\n--- Step 4: Reading tree object ---");
    let tree_oid = commit_obj.tree;
    let tree_hex = tree_oid.to_string();
    let tree_obj_path = git_abs.join("objects").join(&tree_hex[0..2]).join(&tree_hex[2..]);
    eprintln!("Tree object path: {:?}", tree_obj_path);
    eprintln!("Tree object exists: {}", tree_obj_path.exists());
    
    let tree_obj = TreeObject::read_loose_tree(&git_abs, &tree_hex);
    eprintln!("✓ Tree object read successfully");
    eprintln!("  - Entries count: {}", tree_obj.entries().len());
    
    // 步骤5: 调用 load_head_tree
    eprintln!("\n--- Step 5: Calling load_head_tree ---");
    let head_tree_result = crate::status::load_head_tree(worktree, git_dir);
    
    match &head_tree_result {
        Ok(Some(head_tree)) => {
            eprintln!("✓ load_head_tree returned Some(HeadTree)");
            eprintln!("  HeadTree entries count: {}", head_tree.entries().len());
            eprintln!("\n  All entries (path -> hash):");
            
            let mut paths: Vec<_> = head_tree.entries().keys().collect();
            paths.sort();
            
            for path_bytes in paths {
                let path_str = String::from_utf8_lossy(path_bytes);
                let hash = head_tree.get_hash(path_bytes).unwrap();
                eprintln!("    - {} -> {}", path_str, hash.to_string());
            }
        }
        Ok(None) => {
            eprintln!("⚠ load_head_tree returned None (no HEAD commit)");
        }
        Err(e) => {
            eprintln!("✗ load_head_tree returned error: {}", e);
        }
    }
    
    // 验证结果
    eprintln!("\n--- Step 6: Verification ---");
    assert!(head_tree_result.is_ok(), "load_head_tree should not error");
    
    let head_tree = head_tree_result.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(head_tree.entries().len(), 3, "Should have 3 files (file1.txt, file2.txt, subdir/file3.txt)");
    
    // 验证文件是否都在
    assert!(head_tree.contains_path(b"file1.txt"), "file1.txt should be in HEAD tree");
    assert!(head_tree.contains_path(b"file2.txt"), "file2.txt should be in HEAD tree");
    assert!(head_tree.contains_path(b"subdir/file3.txt"), "subdir/file3.txt should be in HEAD tree");
    
    eprintln!("✓ All verifications passed");
    eprintln!("=== End test ===\n");
}

#[test]
fn test_status_functionality() {
    let case_dir = make_case_dir("status_test");
    let worktree = &case_dir;
    let git_dir = Path::new(".git");
    
    eprintln!("\n=== Testing git status functionality ===\n");
    
    // 初始化仓库
    run_git(worktree, &["init"]);
    
    // 创建初始文件并提交
    fs::write(worktree.join("file1.txt"), "initial content\n").unwrap();
    fs::write(worktree.join("file2.txt"), "initial content\n").unwrap();
    fs::create_dir_all(worktree.join("subdir")).unwrap();
    fs::write(worktree.join("subdir").join("file3.txt"), "initial content\n").unwrap();
    
    run_git(worktree, &["add", "file1.txt", "file2.txt", "subdir/file3.txt"]);
    run_git(worktree, &["commit", "-m", "initial commit"]);
    
    eprintln!("✓ Initial commit created");
    
    // 测试1: 初始状态应该没有改动
    let status = crate::status::status(worktree, git_dir).expect("status");
    assert_eq!(status.staged.len(), 0, "No staged changes expected");
    assert_eq!(status.unstaged.len(), 0, "No unstaged changes expected");
    assert_eq!(status.untracked.len(), 0, "No untracked files expected");
    eprintln!("✓ Test 1 passed: Clean state");
    
    // 测试2: 修改已跟踪文件（未暂存）
    fs::write(worktree.join("file1.txt"), "modified content\n").unwrap();
    let status = crate::status::status(worktree, git_dir).expect("status");
    assert_eq!(status.unstaged.len(), 1, "Should have 1 unstaged change");
    assert_eq!(status.unstaged[0].path, PathBuf::from("file1.txt"));
    assert_eq!(status.unstaged[0].change_type, crate::status::ChangeType::Modified);
    eprintln!("✓ Test 2 passed: Modified tracked file (unstaged)");
    
    // 测试3: 暂存修改
    run_git(worktree, &["add", "file1.txt"]);
    let status = crate::status::status(worktree, git_dir).expect("status");
    assert_eq!(status.staged.len(), 1, "Should have 1 staged change");
    assert_eq!(status.staged[0].path, PathBuf::from("file1.txt"));
    assert_eq!(status.staged[0].change_type, crate::status::ChangeType::Modified);
    assert_eq!(status.unstaged.len(), 0, "No unstaged changes expected after add");
    eprintln!("✓ Test 3 passed: Staged modification");
    
    // 测试4: 新文件（未追踪）
    fs::write(worktree.join("new_file.txt"), "new file\n").unwrap();
    let status = crate::status::status(worktree, git_dir).expect("status");
    assert_eq!(status.untracked.len(), 1, "Should have 1 untracked file");
    assert_eq!(status.untracked[0], PathBuf::from("new_file.txt"));
    eprintln!("✓ Test 4 passed: Untracked file");
    
    // 测试5: 删除文件（未暂存）
    fs::remove_file(worktree.join("file2.txt")).unwrap();
    let status = crate::status::status(worktree, git_dir).expect("status");
    let deleted: Vec<_> = status.unstaged.iter()
        .filter(|e| e.change_type == crate::status::ChangeType::Deleted)
        .collect();
    assert_eq!(deleted.len(), 1, "Should have 1 deleted file");
    assert_eq!(deleted[0].path, PathBuf::from("file2.txt"));
    eprintln!("✓ Test 5 passed: Deleted file (unstaged)");
    
    // 测试6: 暂存删除
    run_git(worktree, &["add", "file2.txt"]);
    let status = crate::status::status(worktree, git_dir).expect("status");
    let staged_deleted: Vec<_> = status.staged.iter()
        .filter(|e| e.change_type == crate::status::ChangeType::Deleted)
        .collect();
    assert_eq!(staged_deleted.len(), 1, "Should have 1 staged deletion");
    assert_eq!(staged_deleted[0].path, PathBuf::from("file2.txt"));
    eprintln!("✓ Test 6 passed: Staged deletion");
    
    // 测试7: 添加新文件（应该从 untracked 移除）
    run_git(worktree, &["add", "new_file.txt"]);
    let status = crate::status::status(worktree, git_dir).expect("status");
    assert_eq!(status.untracked.len(), 0, "New file should no longer be untracked after add");
    assert_eq!(status.staged.len(), 3, "Should have staged: modified file1.txt, deleted file2.txt, new_file.txt");
    eprintln!("✓ Test 7 passed: New file staged");
    
    // 测试8: 子目录中的新文件（未追踪）
    fs::write(worktree.join("subdir").join("file4.txt"), "new file in subdir\n").unwrap();
    let status = crate::status::status(worktree, git_dir).expect("status");
    assert_eq!(status.untracked.len(), 1, "Should have untracked file in subdir");
    assert_eq!(status.untracked[0], PathBuf::from("subdir/file4.txt"));
    eprintln!("✓ Test 8 passed: Untracked file in subdirectory");
    
    // 测试9: 提交所有改动（不包括 file4.txt）
    run_git(worktree, &["commit", "-m", "second commit"]);
    let status = crate::status::status(worktree, git_dir).expect("status");
    assert_eq!(status.staged.len(), 0, "No staged changes after commit");
    assert_eq!(status.unstaged.len(), 0, "No unstaged changes after commit");
    // 注意：subdir/file4.txt 仍然存在，应该是 untracked
    assert_eq!(status.untracked.len(), 1, "Should have 1 untracked file (subdir/file4.txt)");
    assert_eq!(status.untracked[0], PathBuf::from("subdir/file4.txt"));
    eprintln!("✓ Test 9 passed: After commit, only untracked file4.txt remains");
    
    // 测试10: 清理 untracked 文件后再提交
    fs::remove_file(worktree.join("subdir").join("file4.txt")).unwrap();
    let status = crate::status::status(worktree, git_dir).expect("status");
    assert_eq!(status.untracked.len(), 0, "No untracked files after removal");
    eprintln!("✓ Test 10 passed: Clean state after removing untracked file");
    
    // 测试11: 混合场景 - 同时有 staged、unstaged 和 untracked
    fs::write(worktree.join("file1.txt"), "third version\n").unwrap();  // unstaged modify
    fs::write(worktree.join("file2.txt"), "new content\n").unwrap();    // file2.txt was deleted, now recreated (untracked)
    fs::write(worktree.join("new_file2.txt"), "another new file\n").unwrap(); // untracked
    run_git(worktree, &["add", "file1.txt"]);  // stage modification
    
    let status = crate::status::status(worktree, git_dir).expect("status");
    
    // 检查 staged (file1.txt modified)
    let staged_modified: Vec<_> = status.staged.iter()
        .filter(|e| e.change_type == crate::status::ChangeType::Modified)
        .collect();
    assert_eq!(staged_modified.len(), 1);
    assert_eq!(staged_modified[0].path, PathBuf::from("file1.txt"));
    
    // 检查 untracked (应该有 new_file2.txt 和 file2.txt)
    assert!(status.untracked.len() >= 2, "Should have at least 2 untracked files");
    
    eprintln!("✓ Test 11 passed: Mixed scenario");
    
    eprintln!("\n=== All status tests passed! ===\n");
}