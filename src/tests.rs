// 测试辅助函数，由各子模块共享。具体测试见 tests/ 下各子文件。

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::index::index_tree::TreeNode;
use crate::object::{CommitIdentity, FileMode, ObjectSha};
use flate2::bufread::ZlibDecoder;

mod object;
mod index;
mod reference;
mod commit;
mod checkout;
mod status;

/// 测试用的独立目录，持有 worktree 和 git/gift 仓库目录的绝对路径。
pub(super) struct TestRepo {
    /// 工作区根目录（绝对路径）
    pub worktree: PathBuf,
    /// git/gift 仓库目录（绝对路径，= worktree/.git 或 worktree/.gift）
    pub git_abs: PathBuf,
}

/// 创建独立测试目录，git_abs = worktree/.git（配合 `git init` 使用）
pub(super) fn make_test_repo(case_name: &str) -> TestRepo {
    let worktree = make_worktree_dir(case_name);
    let git_abs = worktree.join(".git");
    TestRepo { worktree, git_abs }
}

/// 创建独立测试目录，git_abs = worktree/.gift（配合 `gift init` 使用）
pub(super) fn make_gift_repo(case_name: &str) -> TestRepo {
    let worktree = make_worktree_dir(case_name);
    let git_abs = worktree.join(".gift");
    TestRepo { worktree, git_abs }
}

fn make_worktree_dir(case_name: &str) -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let root = PathBuf::from("target")
        .join("inspect")
        .join(format!("{case_name}-{ts}"));
    fs::create_dir_all(&root).unwrap();
    let cwd = std::env::current_dir().unwrap();
    cwd.join(root)
}

/// 所有 commit 测试共用的固定 author/committer，保证 OID 可重复（不受系统时间、git config 影响）。
pub(super) fn test_commit_identity() -> CommitIdentity {
    CommitIdentity {
        name: "Gift Test".into(),
        email: "gift@test.local".into(),
        unix_time: 1_700_000_000,
        tz: "+0800".into(),
    }
}


/// 在 `dir` 下运行真实 `git` 命令，失败则 panic（断言测试前置条件已满足）。
pub(super) fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("failed to run git");

    assert!(status.success(), "git {:?} failed", args);
}

/// zlib 解压后的完整 loose object 字节（含 `type len\0` 头）
pub(super) fn decompress_loose_object(git_abs: &Path, hex_oid: &str) -> Vec<u8> {
    let loose = crate::git_paths::loose_object_path(git_abs, hex_oid);
    let f = fs::File::open(&loose).expect("open loose object");
    let mut zlib = ZlibDecoder::new(std::io::BufReader::new(f));
    let mut raw = Vec::new();
    zlib.read_to_end(&mut raw).expect("decompress");
    raw
}

/// 在指定工作目录下调用系统的 `git commit-tree`，固定 author/committer 环境变量，
/// 返回新 commit 的 SHA（40 位 hex）。
pub(super) fn git_commit_tree_with_env(dir: &Path, tree: &str, parents: &[&str], msg: &str) -> String {
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

/// 运行真实 `git` 命令并返回 stdout，失败则 panic。常用于取 OID（`write-tree`、`rev-parse` 等）。
pub(super) fn git_stdout(dir: &Path, args: &[&str]) -> String {
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

/// 解析 `git ls-tree` 单行（`100644 blob <sha>\tname`），返回 (mode, type, sha, name)。
pub(super) fn parse_ls_tree_line(line: &str) -> (String, String, String, String) {
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

/// `git ls-tree` 输出的 mode 字符串 → `FileMode` 枚举，用于与 gift 解析结果对拍。
pub(super) fn mode_word_to_file_mode(mode: &str) -> FileMode {
    match mode {
        "100644" => FileMode::NExecRegularFile,
        "100755" => FileMode::ExecRegularFile,
        "120000" => FileMode::SymbolicLink,
        "160000" => FileMode::Gitlink,
        "40000" | "040000" => FileMode::Directory,
        other => panic!("unexpected ls-tree mode {other:?}"),
    }
}

/// 用 `git ls-tree -z` 把 tree 展开为 `name → (mode, type, sha)` 映射，用于逐条与 gift 结果对拍。
/// `-z` 以 NUL 分隔，避免非 ASCII 文件名被转义。
pub(super) fn git_ls_tree_map(dir: &Path, tree_oid: &str) -> BTreeMap<String, (String, String, String)> {
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

/// DFS 遍历 `IndexRootTree` 的子节点，收集所有 blob 叶子：相对路径 → (mode, oid)。
/// 供 `from_index_file` 系列测试与 index entries 逐条对拍。
pub(super) fn collect_blob_leaves_from_tree(
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

pub(super) fn collect_blob_leaves_from_root(root: &TreeNode) -> BTreeMap<PathBuf, (FileMode, ObjectSha)> {
    let mut out = BTreeMap::new();
    collect_blob_leaves_from_tree(PathBuf::new(), root, &mut out);
    out
}
