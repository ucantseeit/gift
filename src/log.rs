//! 显示提交历史（类似 `git log`）

use anyhow::{Context, Result};
use std::path::Path;
use crate::head::Head;
use crate::object::{CommitObject, ObjectSha};
/// # 参数
///
/// - `git_abs`: Git 目录的绝对路径（例如 `.git/` 目录的完整路径）。
/// - `max_count`: 最多显示的提交数量。如果为 `None`，则显示所有可达的提交。
/// 打印提交历史，从 HEAD 开始沿着 parent 链遍历。
pub fn log(git_abs: &Path, max_count: Option<usize>) -> Result<()> {
    let head = Head::read(git_abs).context("failed to read HEAD")?;
    let mut oid = head.current_commit(git_abs).context("no commits yet")?;

    let mut count = 0;
    loop {
        if let Some(limit) = max_count {
            if count >= limit {
                break;
            }
        }

        let commit = CommitObject::read_loose_commit(git_abs, &oid.to_string())
            .with_context(|| format!("read commit {}", hex::encode(oid.as_bytes())))?;

        print_commit(&oid, &commit);
        count += 1;

        match commit.parents.first() {
            Some(parent) => oid = parent.clone(),
            None => break,
        }
    }

    Ok(())
}

fn print_commit(oid: &ObjectSha, commit: &CommitObject) {
    let hash = hex::encode(oid.as_bytes());
    println!("commit {}", hash);
    println!();

    let message = extract_message(&commit.message);
    for line in message.lines() {
        println!("    {}", line);
    }
    println!();
}

fn extract_message(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    let mut lines = s.lines();
    let mut found_empty = false;
    let mut msg_lines = Vec::new();

    for line in lines.by_ref() {
        if !found_empty {
            if line.is_empty() {
                found_empty = true;
            }
            continue;
        }
        msg_lines.push(line);
    }

    if msg_lines.is_empty() && !found_empty {
        s.trim().to_string()
    } else {
        msg_lines.join("\n").trim().to_string()
    }
}

