//! `git pull` = `fetch` + `merge`。完全复用已有两块：
//!   - [`crate::fetch::fetch`]：拉对象 + 更新 `refs/remotes/<remote>/*`（不动工作区）
//!   - [`crate::merge::merge`]：把抓到的远程分支并入当前分支（FF / 三方 / 冲突）
//!
//! 简化点：本项目没有 remote / upstream 配置系统，所以远端 `url` 显式给出；
//! 要并入的远程分支默认取“当前 HEAD 所在分支的同名远程分支”，也可显式指定。
//!
//! 拆成两层是为了可测：`pull` = `fetch` + `merge_remote_tracking`，后者（fetch 之后
//! 的纯本地“并入”逻辑）不依赖网络，可离线测试。

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::fetch::fetch;
use crate::head::Head;
use crate::merge::{MergeOutcome, merge};
use crate::object::CommitIdentity;

/// 取当前 HEAD 所跟随的本地分支短名（如 `"master"`）。detached HEAD 时报错。
pub(crate) fn current_branch_name(git_abs: &Path) -> Result<String> {
    match Head::read(git_abs)? {
        Head::TargetBranch(symref) => {
            let p = symref.ref_path.to_string_lossy().replace('\\', "/"); // refs/heads/master
            Ok(p.strip_prefix("refs/heads/").unwrap_or(&p).to_string())
        }
        Head::TargetCommit(_) => {
            bail!("当前处于 detached HEAD，请用 -b/--branch 显式指定要 pull 的分支")
        }
    }
}

/// fetch 之后的“并入”部分（纯本地）：
/// 解析要并入的远程分支 → 读 `refs/remotes/<remote>/<branch>` 的 OID → 调 `merge`。
///
/// `branch` 为 `None` 时取当前分支同名。target 传 40 位 hex OID，`merge` 直接解析，
/// 无需把远程分支写进 `refs/heads/`。
pub fn merge_remote_tracking(
    worktree: &Path,
    git_abs: &Path,
    remote: &str,
    branch: Option<&str>,
    author: CommitIdentity,
    committer: CommitIdentity,
    message: Option<String>,
) -> Result<MergeOutcome> {
    let branch = match branch {
        Some(b) => b.to_string(),
        None => current_branch_name(git_abs)?,
    };

    let track = git_abs.join("refs").join("remotes").join(remote).join(&branch);
    let oid_hex = fs::read_to_string(&track)
        .with_context(|| {
            format!(
                "远程跟踪分支不存在：{}（远端有 {branch} 这个分支、且已 fetch 过吗？）",
                track.display()
            )
        })?
        .trim()
        .to_string();

    let msg = message.unwrap_or_else(|| format!("Merge branch '{remote}/{branch}'\n"));
    merge(worktree, git_abs, &oid_hex, author, committer, &msg)
}

/// 顶层 pull：先 `fetch`（拉对象 + 更新远程跟踪引用，并打印 fetch 摘要），
/// 再把对应的远程分支并入当前分支。
pub fn pull(
    worktree: &Path,
    git_abs: &Path,
    url: &str,
    remote: &str,
    branch: Option<&str>,
    author: CommitIdentity,
    committer: CommitIdentity,
    message: Option<String>,
) -> Result<MergeOutcome> {
    fetch(git_abs, url, remote)?; // 复用 fetch：拉对象 + 更新 refs/remotes/<remote>/*
    merge_remote_tracking(worktree, git_abs, remote, branch, author, committer, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn run(c: &mut Command) {
        assert!(c.status().unwrap().success(), "命令失败: {c:?}");
    }
    fn git_out(repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git").arg("-C").arg(repo).args(args).output().unwrap().stdout;
        String::from_utf8(out).unwrap().trim().to_string()
    }
    fn ident() -> CommitIdentity {
        CommitIdentity { name: "t".into(), email: "t@t".into(), unix_time: 0, tz: "+0000".into() }
    }

    /// 离线验证 fetch 之后的“并入”：本地 master 落后于 origin/master 时应快进，
    /// 并真正更新本地分支 ref 与工作区文件。
    #[test]
    fn merge_remote_tracking_fast_forwards() {
        let env = [
            ("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"),
            ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t"),
        ];

        let repo = std::env::temp_dir().join(format!("pull_{}", std::process::id()));
        let _ = fs::remove_dir_all(&repo);
        run(Command::new("git").args(["init", "-q"]).arg(&repo));

        // 提交 A（旧），再提交 B（新）
        fs::write(repo.join("data.txt"), "v1\n").unwrap();
        run(Command::new("git").arg("-C").arg(&repo).args(["add", "."]));
        run(Command::new("git").arg("-C").arg(&repo).args(["commit", "-q", "-m", "A"]).envs(env));
        let a = git_out(&repo, &["rev-parse", "HEAD"]);
        fs::write(repo.join("data.txt"), "v2\n").unwrap();
        run(Command::new("git").arg("-C").arg(&repo).args(["add", "."]));
        run(Command::new("git").arg("-C").arg(&repo).args(["commit", "-q", "-m", "B"]).envs(env));
        let b = git_out(&repo, &["rev-parse", "HEAD"]);
        let branch = git_out(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]); // master / main

        // 把本地分支退回 A（模拟“本地落后”），B 的对象仍在库中
        run(Command::new("git").arg("-C").arg(&repo).args(["reset", "-q", "--hard", &a]));
        assert_eq!(fs::read_to_string(repo.join("data.txt")).unwrap(), "v1\n");

        // 模拟 fetch 的产物：远程跟踪引用 origin/<branch> 指向 B
        let git_abs = repo.join(".git");
        let track = git_abs.join("refs/remotes/origin").join(&branch);
        fs::create_dir_all(track.parent().unwrap()).unwrap();
        fs::write(&track, format!("{b}\n")).unwrap();

        // “并入”部分：branch=None → 取当前分支 → 读 origin/<branch>=B → merge（应快进）
        let outcome = merge_remote_tracking(
            &repo, &git_abs, "origin", None, ident(), ident(), None,
        ).unwrap();

        match outcome {
            MergeOutcome::FastForward(oid) => assert_eq!(oid.to_string(), b, "应快进到 B"),
            MergeOutcome::AlreadyUpToDate => panic!("期望 FastForward，得到 AlreadyUpToDate"),
            MergeOutcome::Clean(_) => panic!("期望 FastForward，得到 Clean"),
            MergeOutcome::Conflict(_) => panic!("期望 FastForward，得到 Conflict"),
        }
        // 本地分支 ref 已前进到 B；工作区文件已更新为 v2
        assert_eq!(git_out(&repo, &["rev-parse", "HEAD"]), b, "本地分支应指向 B");
        assert_eq!(fs::read_to_string(repo.join("data.txt")).unwrap(), "v2\n", "工作区应更新为 v2");

        let _ = fs::remove_dir_all(&repo);
    }

    /// 已经是最新时应报告 Already up to date（target 即当前 commit）。
    #[test]
    fn merge_remote_tracking_already_up_to_date() {
        let env = [
            ("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"),
            ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t"),
        ];
        let repo = std::env::temp_dir().join(format!("pull2_{}", std::process::id()));
        let _ = fs::remove_dir_all(&repo);
        run(Command::new("git").args(["init", "-q"]).arg(&repo));
        fs::write(repo.join("a"), "x\n").unwrap();
        run(Command::new("git").arg("-C").arg(&repo).args(["add", "."]));
        run(Command::new("git").arg("-C").arg(&repo).args(["commit", "-q", "-m", "A"]).envs(env));
        let a = git_out(&repo, &["rev-parse", "HEAD"]);
        let branch = git_out(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]);

        let git_abs = repo.join(".git");
        let track = git_abs.join("refs/remotes/origin").join(&branch);
        fs::create_dir_all(track.parent().unwrap()).unwrap();
        fs::write(&track, format!("{a}\n")).unwrap(); // origin 与本地同一 commit

        let outcome = merge_remote_tracking(
            &repo, &git_abs, "origin", None, ident(), ident(), None,
        ).unwrap();
        assert!(matches!(outcome, MergeOutcome::AlreadyUpToDate));

        let _ = fs::remove_dir_all(&repo);
    }
}
