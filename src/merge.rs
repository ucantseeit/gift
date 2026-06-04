// 三方树归并（resolve 策略，不做 rename 检测）。
//
// 当前范围：base/ours/theirs 三棵树均在 object db 中。
// 不做的事：不读 blob 内容、不调 diffy、不写任何 object。
// 冲突条目只记录三方 OID，内容合并（diffy::merge）由上层负责。
// Rename：one-side delete + other-side add，不做相似度配对，视为独立的删除和新增。

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

use anyhow::Result;

use crate::object::{FileMode, ObjectSha, TreeEntry, TreeObject};

/// 三方归并对单个路径条目的决策结果。
#[derive(Debug)]
pub enum MergeEntry {
    /// 直接采用（不变 / 一侧干净获胜 / 双方改成相同内容）
    Take {
        name: OsString,
        mode: FileMode,
        oid: ObjectSha,
    },
    /// 该条目在合并结果中不存在（一方删除另一方未动，或双方都删除）
    Delete { name: OsString },
    /// 两侧做了不兼容改动，需内容级处理
    Conflict {
        name: OsString,
        base: Option<(FileMode, ObjectSha)>,
        ours: Option<(FileMode, ObjectSha)>,
        theirs: Option<(FileMode, ObjectSha)>,
    },
    /// 三侧都是目录，递归决策结果
    Subtree {
        name: OsString,
        entries: Vec<MergeEntry>,
    },
}

type Side = Option<(FileMode, ObjectSha)>;

/// 三方树归并入口。
///
/// 输入三棵 TreeObject（LCA / HEAD / 要合并的分支），返回逐条目的决策列表。
pub fn merge_trees(
    git_abs: &Path,
    base: &TreeObject,
    ours: &TreeObject,
    theirs: &TreeObject,
) -> Result<Vec<MergeEntry>> {
    let mut out = Vec::new();
    merge_recursive(git_abs, Some(base), ours, theirs, &mut out)?;
    Ok(out)
}

fn merge_recursive(
    git_abs: &Path,
    base: Option<&TreeObject>,
    ours: &TreeObject,
    theirs: &TreeObject,
    out: &mut Vec<MergeEntry>,
) -> Result<()> {
    let empty: BTreeMap<OsString, TreeEntry> = BTreeMap::new();
    let base_entries = base.map(|t| t.entries()).unwrap_or(&empty);

    let mut base_iter = base_entries.iter().peekable();
    let mut our_iter = ours.entries().iter().peekable();
    let mut their_iter = theirs.entries().iter().peekable();

    loop {
        // Clone names up front to avoid holding iterator borrows across the advance calls.
        let b_name = base_iter.peek().map(|(n, _)| (*n).clone());
        let o_name = our_iter.peek().map(|(n, _)| (*n).clone());
        let t_name = their_iter.peek().map(|(n, _)| (*n).clone());

        let name = match [b_name, o_name, t_name].into_iter().flatten().min() {
            None => break,
            Some(n) => n,
        };

        let base_e = advance_if_eq(&mut base_iter, &name);
        let our_e = advance_if_eq(&mut our_iter, &name);
        let their_e = advance_if_eq(&mut their_iter, &name);

        out.push(decide(git_abs, name, base_e, our_e, their_e)?);
    }

    Ok(())
}

fn advance_if_eq<'a, I>(iter: &mut std::iter::Peekable<I>, name: &OsString) -> Side
where
    I: Iterator<Item = (&'a OsString, &'a TreeEntry)>,
{
    if iter.peek().map(|(n, _)| *n) == Some(name) {
        let (_, entry) = iter.next().unwrap();
        Some((entry.file_mode, entry.object_name.clone()))
    } else {
        None
    }
}

fn sides_equal(a: &Side, b: &Side) -> bool {
    match (a, b) {
        (Some((am, ao)), Some((bm, bo))) => am == bm && ao == bo,
        (None, None) => true,
        _ => false,
    }
}

fn decide(git_abs: &Path, name: OsString, base: Side, ours: Side, theirs: Side) -> Result<MergeEntry> {
    // 先借用做比较，再 move 进返回值。
    let ours_eq_base = sides_equal(&ours, &base);
    let theirs_eq_base = sides_equal(&theirs, &base);
    let ours_eq_theirs = sides_equal(&ours, &theirs);
    let our_is_dir = ours.as_ref().map(|(m, _)| *m) == Some(FileMode::Directory);
    let their_is_dir = theirs.as_ref().map(|(m, _)| *m) == Some(FileMode::Directory);
    let base_oid_str = base.as_ref().map(|(_, o)| o.to_string());
    let our_oid_str = ours.as_ref().map(|(_, o)| o.to_string());
    let their_oid_str = theirs.as_ref().map(|(_, o)| o.to_string());

    // 双方都删了（或均不存在）
    if ours.is_none() && theirs.is_none() {
        return Ok(MergeEntry::Delete { name });
    }

    // 双方结果一致（含双方都是新增同内容）
    if ours_eq_theirs {
        return Ok(match ours {
            Some((m, o)) => MergeEntry::Take { name, mode: m, oid: o },
            None => MergeEntry::Delete { name },
        });
    }

    // 只有 ours 改动了（theirs 与 base 相同）
    if theirs_eq_base {
        return Ok(match ours {
            Some((m, o)) => MergeEntry::Take { name, mode: m, oid: o },
            None => MergeEntry::Delete { name },
        });
    }

    // 只有 theirs 改动了（ours 与 base 相同）
    if ours_eq_base {
        return Ok(match theirs {
            Some((m, o)) => MergeEntry::Take { name, mode: m, oid: o },
            None => MergeEntry::Delete { name },
        });
    }

    // 双方都改了，且结果不同。
    if our_is_dir && their_is_dir {
        // 两侧均为目录 → 递归归并子树。
        let base_sub = match base_oid_str {
            Some(ref oid) => Some(TreeObject::read_loose_tree(git_abs, oid)?),
            None => None,
        };
        let our_sub = TreeObject::read_loose_tree(git_abs, &our_oid_str.unwrap())?;
        let their_sub = TreeObject::read_loose_tree(git_abs, &their_oid_str.unwrap())?;
        let mut sub_entries = Vec::new();
        merge_recursive(git_abs, base_sub.as_ref(), &our_sub, &their_sub, &mut sub_entries)?;
        Ok(MergeEntry::Subtree { name, entries: sub_entries })
    } else {
        // 其他情形（blob vs blob / blob vs dir）→ 冲突，交给上层处理。
        Ok(MergeEntry::Conflict { name, base, ours, theirs })
    }
}
