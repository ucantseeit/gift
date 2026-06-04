use std::fs;
use std::path::Path;
use std::process::Command;

use crate::diff::{diff_blobs, diff_commits, diff_dirs, subtree_at, BlobDiff, BlobSide};
use crate::object::{CommitObject, ObjectSha, TreeObject};

use super::{make_test_repo, run_git, git_stdout};

/// unified diff 字符串中的实际内容行（`+`/`-`/空格开头），去掉 `---`/`+++` 文件名头。
/// `@@` hunk 头不参与比较，使结果不受文件名、行号格式影响。
fn hunk_lines(diff: &str) -> Vec<String> {
    diff.lines()
        .filter(|l| {
            (l.starts_with('+') || l.starts_with('-') || l.starts_with(' '))
                && !l.starts_with("---")
                && !l.starts_with("+++")
        })
        .map(|l| l.to_string())
        .collect()
}

/// `git diff --no-index --no-color old new`（文件有差异时 exit 1，不视为失败）
fn git_diff_no_index(dir: &Path, old: &str, new: &str) -> String {
    let out = Command::new("git")
        .args(["diff", "--no-index", "--no-color", old, new])
        .current_dir(dir)
        .output()
        .expect("git diff --no-index");
    String::from_utf8(out.stdout).expect("utf-8")
}

// ── diff_blobs ────────────────────────────────────────────────────────────────

/// 流程：写两个文本文件 → git hash-object -w 写入 blob
///       → diff_blobs(Stored, Stored) 得到 unified diff
///       → git diff --no-index 得到参考 diff
///       → 对比 hunk 内容行（去掉文件名头，两侧应完全一致）
#[test]
fn diff_blobs_text_matches_git() {
    let repo = make_test_repo("diff_blobs_text");
    run_git(&repo.worktree, &["init"]);

    let old_content = "hello\nworld\nfoo\n";
    let new_content = "hello\nearth\nfoo\nbar\n";
    fs::write(repo.worktree.join("old.txt"), old_content).unwrap();
    fs::write(repo.worktree.join("new.txt"), new_content).unwrap();

    let old_hex = git_stdout(&repo.worktree, &["hash-object", "-w", "old.txt"]).trim().to_string();
    let new_hex = git_stdout(&repo.worktree, &["hash-object", "-w", "new.txt"]).trim().to_string();
    let old_oid = ObjectSha::SHA1(hex::decode(&old_hex).unwrap().try_into().unwrap());
    let new_oid = ObjectSha::SHA1(hex::decode(&new_hex).unwrap().try_into().unwrap());

    let gift_diff = match diff_blobs(&repo.git_abs, BlobSide::Stored(&old_oid), BlobSide::Stored(&new_oid)).unwrap() {
        BlobDiff::Text(s) => s,
        BlobDiff::Binary => panic!("expected text diff"),
    };
    let git_diff = git_diff_no_index(&repo.worktree, "old.txt", "new.txt");

    assert_eq!(hunk_lines(&gift_diff), hunk_lines(&git_diff));
}

/// 含 `\0` 字节的 blob：diff_blobs 应返回 Binary
#[test]
fn diff_blobs_binary_detected() {
    let repo = make_test_repo("diff_blobs_binary");
    run_git(&repo.worktree, &["init"]);

    fs::write(repo.worktree.join("old.bin"), b"hello\x00binary").unwrap();
    fs::write(repo.worktree.join("new.bin"), b"world\x00binary").unwrap();
    let old_hex = git_stdout(&repo.worktree, &["hash-object", "-w", "old.bin"]).trim().to_string();
    let new_hex = git_stdout(&repo.worktree, &["hash-object", "-w", "new.bin"]).trim().to_string();
    let old_oid = ObjectSha::SHA1(hex::decode(&old_hex).unwrap().try_into().unwrap());
    let new_oid = ObjectSha::SHA1(hex::decode(&new_hex).unwrap().try_into().unwrap());

    let result = diff_blobs(&repo.git_abs, BlobSide::Stored(&old_oid), BlobSide::Stored(&new_oid)).unwrap();
    assert!(matches!(result, BlobDiff::Binary));
}

/// BlobSide::Worktree：直接从磁盘读，与 Stored 侧对比
#[test]
fn diff_blobs_worktree_side() {
    let repo = make_test_repo("diff_blobs_worktree");
    run_git(&repo.worktree, &["init"]);

    let old_content = "version one\n";
    let new_content = "version two\n";
    fs::write(repo.worktree.join("old.txt"), old_content).unwrap();
    fs::write(repo.worktree.join("new.txt"), new_content).unwrap();

    let old_hex = git_stdout(&repo.worktree, &["hash-object", "-w", "old.txt"]).trim().to_string();
    let old_oid = ObjectSha::SHA1(hex::decode(&old_hex).unwrap().try_into().unwrap());
    let new_path = repo.worktree.join("new.txt");

    // old 从 object db 读，new 直接从磁盘读
    let gift_diff = match diff_blobs(&repo.git_abs, BlobSide::Stored(&old_oid), BlobSide::Worktree(&new_path)).unwrap() {
        BlobDiff::Text(s) => s,
        BlobDiff::Binary => panic!("expected text diff"),
    };
    let git_diff = git_diff_no_index(&repo.worktree, "old.txt", "new.txt");

    assert_eq!(hunk_lines(&gift_diff), hunk_lines(&git_diff));
}

// ── diff_commits ──────────────────────────────────────────────────────────────

/// 流程：git init → 写三个文件 → commit c1
///       → 修改 a.txt、删除 b.txt、新增 d.txt → commit c2
///       → diff_commits(c1, c2) 收集全部 hunk 行
///       → git diff c1 c2 收集全部 hunk 行
///       → 断言完全一致
#[test]
fn diff_commits_matches_git() {
    let repo = make_test_repo("diff_commits");
    run_git(&repo.worktree, &["init"]);

    fs::write(repo.worktree.join("a.txt"), "line1\nline2\nline3\n").unwrap();
    fs::write(repo.worktree.join("b.txt"), "alpha\nbeta\n").unwrap();
    fs::write(repo.worktree.join("c.txt"), "unchanged\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "c1"]);
    let c1 = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    fs::write(repo.worktree.join("a.txt"), "line1\nLINE2\nline3\n").unwrap();
    fs::remove_file(repo.worktree.join("b.txt")).unwrap();
    fs::write(repo.worktree.join("d.txt"), "brand new\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "c2"]);
    let c2 = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    let diffs = diff_commits(&repo.git_abs, &c1, &c2).unwrap();
    let gift_hunks: Vec<String> = diffs.iter()
        .filter_map(|d| if let BlobDiff::Text(s) = &d.diff { Some(hunk_lines(s)) } else { None })
        .flatten()
        .collect();

    let git_out = git_stdout(&repo.worktree, &["diff", "--no-color", &c1, &c2]);
    let git_hunks = hunk_lines(&git_out);

    assert_eq!(gift_hunks, git_hunks);
}

// ── diff 子目录（两个 commit 间） ─────────────────────────────────────────────

/// 流程：git init → src/ 下建两个文件 + 根目录 README → commit c1
///       → 修改 src/main.rs 和 README（两处都改，但只 diff src/）→ commit c2
///       → subtree_at 导航到 src/ 子树 → diff_dirs
///       → git diff c1 c2 -- src/ 得到参考
///       → 断言 hunk 行一致，且 README 的变化不出现在结果里
#[test]
fn diff_subdir_matches_git() {
    let repo = make_test_repo("diff_subdir");
    run_git(&repo.worktree, &["init"]);

    fs::create_dir_all(repo.worktree.join("src")).unwrap();
    fs::write(repo.worktree.join("src").join("main.rs"), "fn main() {}\n").unwrap();
    fs::write(repo.worktree.join("src").join("lib.rs"), "pub fn hello() {}\n").unwrap();
    fs::write(repo.worktree.join("README.md"), "readme\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "c1"]);
    let c1 = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    fs::write(repo.worktree.join("src").join("main.rs"), "fn main() { println!(\"hi\"); }\n").unwrap();
    fs::write(repo.worktree.join("README.md"), "readme updated\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "c2"]);
    let c2 = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    // 用 subtree_at 导航到 src/ 子树
    let old_commit = CommitObject::read_loose_commit(&repo.git_abs, &c1).unwrap();
    let new_commit = CommitObject::read_loose_commit(&repo.git_abs, &c2).unwrap();
    let old_root = TreeObject::read_loose_tree(&repo.git_abs, &old_commit.tree.to_string()).unwrap();
    let new_root = TreeObject::read_loose_tree(&repo.git_abs, &new_commit.tree.to_string()).unwrap();
    let old_src = subtree_at(&repo.git_abs, &old_root, "src".as_ref()).unwrap().expect("src in c1");
    let new_src = subtree_at(&repo.git_abs, &new_root, "src".as_ref()).unwrap().expect("src in c2");

    let diffs = diff_dirs(&repo.git_abs, &old_src, &new_src).unwrap();

    // README.md 不应出现
    assert!(diffs.iter().all(|d| !d.entry.path().ends_with("README.md")));

    let gift_hunks: Vec<String> = diffs.iter()
        .filter_map(|d| if let BlobDiff::Text(s) = &d.diff { Some(hunk_lines(s)) } else { None })
        .flatten()
        .collect();
    let git_out = git_stdout(&repo.worktree, &["diff", "--no-color", &c1, &c2, "--", "src/"]);
    let git_hunks = hunk_lines(&git_out);

    assert_eq!(gift_hunks, git_hunks);
}

/// subtree_at 对不存在的路径返回 None
#[test]
fn subtree_at_missing_returns_none() {
    let repo = make_test_repo("subtree_at_missing");
    run_git(&repo.worktree, &["init"]);

    fs::write(repo.worktree.join("file.txt"), "hello\n").unwrap();
    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "init"]);
    let head = git_stdout(&repo.worktree, &["rev-parse", "HEAD"]).trim().to_string();

    let commit = CommitObject::read_loose_commit(&repo.git_abs, &head).unwrap();
    let root = TreeObject::read_loose_tree(&repo.git_abs, &commit.tree.to_string()).unwrap();

    // 不存在的目录
    let result = subtree_at(&repo.git_abs, &root, "nonexistent".as_ref()).unwrap();
    assert!(result.is_none());

    // 文件路径（不是目录）
    let result = subtree_at(&repo.git_abs, &root, "file.txt".as_ref()).unwrap();
    assert!(result.is_none());
}
