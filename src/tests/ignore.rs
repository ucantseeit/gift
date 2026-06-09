use std::fs;
use super::{make_test_repo};
use crate::staging::stage_paths;
use crate::index::index_file::parse_index_file;

#[test]
fn test_gitignore_simple() {
    let repo = make_test_repo("ignore_simple");
    
    // 创建 .gitignore
    fs::write(repo.worktree.join(".gitignore"), "ignored.txt\n").unwrap();
    
    // 创建文件
    fs::write(repo.worktree.join("ignored.txt"), "should be ignored").unwrap();
    fs::write(repo.worktree.join("kept.txt"), "should be tracked").unwrap();
    
    // 执行 git add .
    let inputs = vec![repo.worktree.to_path_buf()];
    stage_paths(&repo.git_abs, &repo.worktree, &inputs, true).unwrap();
    
    // 检查 index
    let index_path = repo.git_abs.join("index");
    let index = parse_index_file(&index_path).unwrap();
    
    // 验证 ignored.txt 不在 index 中
    let has_ignored = index.entries().any(|e| 
        String::from_utf8_lossy(e.path()).contains("ignored.txt")
    );
    assert!(!has_ignored, "ignored.txt should NOT be in index");
    
    // 验证 kept.txt 在 index 中
    let has_kept = index.entries().any(|e| 
        String::from_utf8_lossy(e.path()).contains("kept.txt")
    );
    assert!(has_kept, "kept.txt should be in index");
    
    // 注意：.gitignore 本身也会被添加，所以 index 条目数可能是 2 或更多
}