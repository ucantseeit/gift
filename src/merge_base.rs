use std::collections::{HashSet, VecDeque};
use std::path::Path;

use anyhow::Result;

use crate::object::{CommitObject, ObjectSha};

/// 返回两个 commit 的最近公共祖先（merge base）。
///
/// - 两个 commit 相同：直接返回该 OID。
/// - 一方是另一方祖先（fast-forward）：返回那个祖先。
/// - 分叉历史：返回 BFS 拓扑序中最先从 B 侧发现的 A 侧祖先。
/// - 多个 LCA（criss-cross）：返回其中之一（`resolve` 策略，不做递归虚拟合并）。
/// - 完全不相关的历史：返回 `None`。
pub fn find_merge_base(
    git_abs: &Path,
    oid_a: &ObjectSha,
    oid_b: &ObjectSha,
) -> Result<Option<ObjectSha>> {
    if oid_a == oid_b {
        return Ok(Some(oid_a.clone()));
    }

    let ancestors_of_a = collect_ancestors(git_abs, oid_a)?;

    let mut visited: HashSet<ObjectSha> = HashSet::new();
    let mut queue: VecDeque<ObjectSha> = VecDeque::new();
    queue.push_back(oid_b.clone());
    visited.insert(oid_b.clone());

    while let Some(oid) = queue.pop_front() {
        if ancestors_of_a.contains(&oid) {
            return Ok(Some(oid));
        }
        let commit = CommitObject::read_loose_commit(git_abs, &oid.to_string())?;
        for parent in commit.parents {
            if visited.insert(parent.clone()) {
                queue.push_back(parent);
            }
        }
    }

    Ok(None)
}

fn collect_ancestors(git_abs: &Path, start: &ObjectSha) -> Result<HashSet<ObjectSha>> {
    let mut visited: HashSet<ObjectSha> = HashSet::new();
    let mut queue: VecDeque<ObjectSha> = VecDeque::new();
    queue.push_back(start.clone());
    visited.insert(start.clone());

    while let Some(oid) = queue.pop_front() {
        let commit = CommitObject::read_loose_commit(git_abs, &oid.to_string())?;
        for parent in commit.parents {
            if visited.insert(parent.clone()) {
                queue.push_back(parent);
            }
        }
    }

    Ok(visited)
}
