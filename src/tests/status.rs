use std::fs;
use std::path::PathBuf;
use crate::object::{CommitObject, TreeObject};
use crate::status;
use super::{make_test_repo, run_git};

/// 流程：git init → 写文件 → git add → git commit（建立初始 HEAD tree）
///       → 修改文件（不 add）→ gift status
///       → 断言：staged 为空、unstaged 有 1 条 Modified、untracked 为空
///
/// 最小场景：验证工作区修改（未暂存）能被正确识别。
#[test]
fn test_status_basic() {
    let repo = make_test_repo("status_basic");

    run_git(&repo.worktree, &["init"]);

    fs::write(repo.worktree.join("file.txt"), "v1\n").unwrap();
    run_git(&repo.worktree, &["add", "file.txt"]);
    run_git(&repo.worktree, &["commit", "-m", "initial"]);

    // 修改但不 add，期望出现在 unstaged
    fs::write(repo.worktree.join("file.txt"), "v2\n").unwrap();

    let result = status::status(&repo.worktree, &repo.git_abs).expect("status failed");

    assert_eq!(result.staged.len(), 0, "should have no staged changes");
    assert_eq!(result.unstaged.len(), 1, "should have 1 unstaged change");
    assert_eq!(result.untracked.len(), 0, "should have no untracked files");

    if let Some(entry) = result.unstaged.first() {
        assert_eq!(entry.change_type, status::ChangeType::Modified);
        assert!(entry.path.ends_with("file.txt"));
    }
}

/// 流程：git init → 写 3 个文件 → git add → git commit
///       → 逐步调用 load_head_tree，打印中间状态（HEAD → commit → tree）
///       → 断言返回的 HeadTree 包含正确的 3 条路径
///
/// 这是一个诊断性测试（含 eprintln! 输出），用于验证 load_head_tree 能正确解析
/// HEAD → commit OID → tree OID → 扁平路径映射的完整调用链。
#[test]
fn test_load_head_tree_debug() {
    let repo = make_test_repo("load_head_tree_debug");

    run_git(&repo.worktree, &["init"]);

    fs::write(repo.worktree.join("file1.txt"), "content 1\n").unwrap();
    fs::write(repo.worktree.join("file2.txt"), "content 2\n").unwrap();
    fs::create_dir_all(repo.worktree.join("subdir")).unwrap();
    fs::write(repo.worktree.join("subdir").join("file3.txt"), "content 3\n").unwrap();

    run_git(&repo.worktree, &["add", "."]);
    run_git(&repo.worktree, &["commit", "-m", "initial commit"]);

    eprintln!("\n=== Testing load_head_tree ===");
    eprintln!("worktree: {:?}", repo.worktree);
    eprintln!("git_abs: {:?}", repo.git_abs);

    // Step 1：读取 HEAD，得到当前所在分支或 detached OID
    eprintln!("\n--- Step 1: Reading HEAD ---");
    let head = match crate::head::Head::read(&repo.git_abs) {
        Ok(h) => { eprintln!("✓ HEAD read successfully: {:?}", h); h }
        Err(e) => { eprintln!("✗ Failed to read HEAD: {}", e); panic!("Cannot proceed without HEAD"); }
    };

    // Step 2：解析当前 commit OID（symbolic HEAD 需先读分支 ref）
    eprintln!("\n--- Step 2: Getting current commit OID ---");
    let commit_oid = match head.current_commit(&repo.git_abs) {
        Ok(oid) => { eprintln!("✓ Current commit OID: {}", oid.to_string()); oid }
        Err(e) => { eprintln!("✗ Failed to get current commit: {}", e); panic!("Cannot proceed without commit OID"); }
    };

    // Step 3：读取 commit loose object，取出 tree OID
    eprintln!("\n--- Step 3: Reading commit object ---");
    let commit_hex = commit_oid.to_string();
    let commit_obj_path = repo.git_abs.join("objects").join(&commit_hex[0..2]).join(&commit_hex[2..]);
    eprintln!("Commit object path: {:?}", commit_obj_path);
    eprintln!("Commit object exists: {}", commit_obj_path.exists());

    let commit_obj = CommitObject::read_loose_commit(&repo.git_abs, &commit_hex);
    eprintln!("✓ Commit object read successfully");
    eprintln!("  - Tree OID: {}", commit_obj.tree.to_string());
    eprintln!("  - Parent count: {}", commit_obj.parents.len());

    // Step 4：读取 tree loose object，确认条目数
    eprintln!("\n--- Step 4: Reading tree object ---");
    let tree_oid = commit_obj.tree;
    let tree_hex = tree_oid.to_string();
    let tree_obj_path = repo.git_abs.join("objects").join(&tree_hex[0..2]).join(&tree_hex[2..]);
    eprintln!("Tree object path: {:?}", tree_obj_path);
    eprintln!("Tree object exists: {}", tree_obj_path.exists());

    let tree_obj = TreeObject::read_loose_tree(&repo.git_abs, &tree_hex);
    eprintln!("✓ Tree object read successfully");
    eprintln!("  - Entries count: {}", tree_obj.entries().len());

    // Step 5：通过 load_head_tree 一步取出 HeadTree（路径 → OID 的扁平映射）
    eprintln!("\n--- Step 5: Calling load_head_tree ---");
    let head_tree_result = crate::status::load_head_tree(&repo.worktree, &repo.git_abs);

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
        Ok(None) => { eprintln!("⚠ load_head_tree returned None (no HEAD commit)"); }
        Err(e) => { eprintln!("✗ load_head_tree returned error: {}", e); }
    }

    // Step 6：断言路径映射包含预期的 3 条记录
    eprintln!("\n--- Step 6: Verification ---");
    assert!(head_tree_result.is_ok(), "load_head_tree should not error");

    let head_tree = head_tree_result.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(head_tree.entries().len(), 3, "Should have 3 files");
    assert!(head_tree.contains_path(b"file1.txt"), "file1.txt should be in HEAD tree");
    assert!(head_tree.contains_path(b"file2.txt"), "file2.txt should be in HEAD tree");
    assert!(head_tree.contains_path(b"subdir/file3.txt"), "subdir/file3.txt should be in HEAD tree");

    eprintln!("✓ All verifications passed");
    eprintln!("=== End test ===\n");
}

/// 流程：git init → git add → git commit（初始 3 个文件）→ 逐步模拟各种工作区状态，每步调用 gift status
///
/// 覆盖 11 个场景：
///   1. 干净状态（无任何改动）
///   2. 修改已追踪文件（unstaged Modified）
///   3. git add 暂存修改（staged Modified）
///   4. 新建未追踪文件（untracked）
///   5. 删除已追踪文件（unstaged Deleted）
///   6. git add 暂存删除（staged Deleted）
///   7. git add 未追踪文件（从 untracked 移入 staged）
///   8. 子目录中的未追踪文件
///   9. 提交后状态清空（只剩 untracked）
///  10. 删除 untracked 文件后恢复干净
///  11. staged + unstaged + untracked 混合场景
///
/// 注意：staged 检测依赖 HEAD vs index 的对比（目前部分功能尚未完整实现）。
#[test]
fn test_status_functionality() {
    let repo = make_test_repo("status_test");

    eprintln!("\n=== Testing git status functionality ===\n");

    run_git(&repo.worktree, &["init"]);

    fs::write(repo.worktree.join("file1.txt"), "initial content\n").unwrap();
    fs::write(repo.worktree.join("file2.txt"), "initial content\n").unwrap();
    fs::create_dir_all(repo.worktree.join("subdir")).unwrap();
    fs::write(repo.worktree.join("subdir").join("file3.txt"), "initial content\n").unwrap();

    run_git(&repo.worktree, &["add", "file1.txt", "file2.txt", "subdir/file3.txt"]);
    run_git(&repo.worktree, &["commit", "-m", "initial commit"]);
    eprintln!("✓ Initial commit created");

    // 场景 1：干净状态
    let result = crate::status::status(&repo.worktree, &repo.git_abs).expect("status");
    assert_eq!(result.staged.len(), 0);
    assert_eq!(result.unstaged.len(), 0);
    assert_eq!(result.untracked.len(), 0);
    eprintln!("✓ Test 1 passed: Clean state");

    // 场景 2：修改已追踪文件（未暂存）
    fs::write(repo.worktree.join("file1.txt"), "modified content\n").unwrap();
    let result = crate::status::status(&repo.worktree, &repo.git_abs).expect("status");
    assert_eq!(result.unstaged.len(), 1);
    assert_eq!(result.unstaged[0].path, PathBuf::from("file1.txt"));
    assert_eq!(result.unstaged[0].change_type, crate::status::ChangeType::Modified);
    eprintln!("✓ Test 2 passed: Modified tracked file (unstaged)");

    // 场景 3：git add 暂存修改
    run_git(&repo.worktree, &["add", "file1.txt"]);
    let result = crate::status::status(&repo.worktree, &repo.git_abs).expect("status");
    assert_eq!(result.staged.len(), 1);
    assert_eq!(result.staged[0].path, PathBuf::from("file1.txt"));
    assert_eq!(result.staged[0].change_type, crate::status::ChangeType::Modified);
    assert_eq!(result.unstaged.len(), 0);
    eprintln!("✓ Test 3 passed: Staged modification");

    // 场景 4：新建未追踪文件
    fs::write(repo.worktree.join("new_file.txt"), "new file\n").unwrap();
    let result = crate::status::status(&repo.worktree, &repo.git_abs).expect("status");
    assert_eq!(result.untracked.len(), 1);
    assert_eq!(result.untracked[0], PathBuf::from("new_file.txt"));
    eprintln!("✓ Test 4 passed: Untracked file");

    // 场景 5：删除已追踪文件（未暂存）
    fs::remove_file(repo.worktree.join("file2.txt")).unwrap();
    let result = crate::status::status(&repo.worktree, &repo.git_abs).expect("status");
    let deleted: Vec<_> = result.unstaged.iter()
        .filter(|e| e.change_type == crate::status::ChangeType::Deleted)
        .collect();
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0].path, PathBuf::from("file2.txt"));
    eprintln!("✓ Test 5 passed: Deleted file (unstaged)");

    // 场景 6：git add 暂存删除
    run_git(&repo.worktree, &["add", "file2.txt"]);
    let result = crate::status::status(&repo.worktree, &repo.git_abs).expect("status");
    let staged_deleted: Vec<_> = result.staged.iter()
        .filter(|e| e.change_type == crate::status::ChangeType::Deleted)
        .collect();
    assert_eq!(staged_deleted.len(), 1);
    assert_eq!(staged_deleted[0].path, PathBuf::from("file2.txt"));
    eprintln!("✓ Test 6 passed: Staged deletion");

    // 场景 7：git add 未追踪文件（从 untracked 移入 staged）
    run_git(&repo.worktree, &["add", "new_file.txt"]);
    let result = crate::status::status(&repo.worktree, &repo.git_abs).expect("status");
    assert_eq!(result.untracked.len(), 0);
    assert_eq!(result.staged.len(), 3); // file1 modified + file2 deleted + new_file added
    eprintln!("✓ Test 7 passed: New file staged");

    // 场景 8：子目录中的未追踪文件
    fs::write(repo.worktree.join("subdir").join("file4.txt"), "new file in subdir\n").unwrap();
    let result = crate::status::status(&repo.worktree, &repo.git_abs).expect("status");
    assert_eq!(result.untracked.len(), 1);
    assert_eq!(result.untracked[0], PathBuf::from("subdir/file4.txt"));
    eprintln!("✓ Test 8 passed: Untracked file in subdirectory");

    // 场景 9：提交后 staged/unstaged 清空，subdir/file4.txt 仍为 untracked
    run_git(&repo.worktree, &["commit", "-m", "second commit"]);
    let result = crate::status::status(&repo.worktree, &repo.git_abs).expect("status");
    assert_eq!(result.staged.len(), 0);
    assert_eq!(result.unstaged.len(), 0);
    assert_eq!(result.untracked.len(), 1);
    assert_eq!(result.untracked[0], PathBuf::from("subdir/file4.txt"));
    eprintln!("✓ Test 9 passed: After commit, only untracked file4.txt remains");

    // 场景 10：删除 untracked 文件后恢复干净
    fs::remove_file(repo.worktree.join("subdir").join("file4.txt")).unwrap();
    let result = crate::status::status(&repo.worktree, &repo.git_abs).expect("status");
    assert_eq!(result.untracked.len(), 0);
    eprintln!("✓ Test 10 passed: Clean state after removing untracked file");

    // 场景 11：staged + unstaged + untracked 混合
    fs::write(repo.worktree.join("file1.txt"), "third version\n").unwrap();    // unstaged modify
    fs::write(repo.worktree.join("file2.txt"), "new content\n").unwrap();      // untracked（已从上次 commit 中删除）
    fs::write(repo.worktree.join("new_file2.txt"), "another new file\n").unwrap(); // untracked
    run_git(&repo.worktree, &["add", "file1.txt"]);                             // stage file1

    let result = crate::status::status(&repo.worktree, &repo.git_abs).expect("status");
    let staged_modified: Vec<_> = result.staged.iter()
        .filter(|e| e.change_type == crate::status::ChangeType::Modified)
        .collect();
    assert_eq!(staged_modified.len(), 1);
    assert_eq!(staged_modified[0].path, PathBuf::from("file1.txt"));
    assert!(result.untracked.len() >= 2, "Should have at least 2 untracked files");
    eprintln!("✓ Test 11 passed: Mixed scenario");

    eprintln!("\n=== All status tests passed! ===\n");
}
