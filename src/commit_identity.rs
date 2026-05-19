//! 可选的 author / committer 解析（例如从 Git 风格环境变量读取），供 CLI 等上层组合 config / env 后调用 [`crate::commit::commit`]。
//!
//! 与 Git 行为接近的规则：`GIT_COMMITTER_*` 均未设置时，committer 与 author 相同；否则分别读取（未设置的 name/email 回退到 author）。

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use crate::object::CommitIdentity;

/// 解析 `GIT_AUTHOR_DATE` / `GIT_COMMITTER_DATE`：`<unix 秒> <时区>`，例如 `1700000000 +0800`。
fn parse_git_identity_date(s: &str) -> Result<(i64, String)> {
    let s = s.trim();
    let mut it = s.split_whitespace();
    let unix: i64 = it
        .next()
        .context("date string empty")?
        .parse()
        .context("bad unix timestamp in date")?;
    let tz = it
        .next()
        .context("date string missing timezone after unix time")?
        .to_string();
    if it.next().is_some() {
        bail!("date string has extra fields (expected `<unix> <tz>`)");
    }
    Ok((unix, tz))
}

fn now_unix_and_tz() -> (i64, String) {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    (unix, "+0000".into())
}

fn author_from_git_env() -> Result<CommitIdentity> {
    let name = std::env::var("GIT_AUTHOR_NAME").unwrap_or_else(|_| "gift".into());
    let email = std::env::var("GIT_AUTHOR_EMAIL").unwrap_or_else(|_| "gift@localhost".into());
    let (unix_time, tz) = match std::env::var("GIT_AUTHOR_DATE") {
        Ok(s) => parse_git_identity_date(&s).context("GIT_AUTHOR_DATE")?,
        Err(_) => now_unix_and_tz(),
    };
    Ok(CommitIdentity {
        name,
        email,
        unix_time,
        tz,
    })
}

fn committer_from_git_env(author: &CommitIdentity) -> Result<CommitIdentity> {
    let any_committer_env = std::env::var("GIT_COMMITTER_NAME").is_ok()
        || std::env::var("GIT_COMMITTER_EMAIL").is_ok()
        || std::env::var("GIT_COMMITTER_DATE").is_ok();
    if !any_committer_env {
        return Ok(author.clone());
    }
    let name = std::env::var("GIT_COMMITTER_NAME").unwrap_or_else(|_| author.name.clone());
    let email = std::env::var("GIT_COMMITTER_EMAIL").unwrap_or_else(|_| author.email.clone());
    let (unix_time, tz) = match std::env::var("GIT_COMMITTER_DATE") {
        Ok(s) => parse_git_identity_date(&s).context("GIT_COMMITTER_DATE")?,
        Err(_) => now_unix_and_tz(),
    };
    Ok(CommitIdentity {
        name,
        email,
        unix_time,
        tz,
    })
}

/// 从当前进程环境变量构造 author / committer（`GIT_AUTHOR_*`、`GIT_COMMITTER_*`），与原先 `commit` 内建逻辑一致。
pub fn identities_from_git_env() -> Result<(CommitIdentity, CommitIdentity)> {
    let author = author_from_git_env().context("GIT_AUTHOR_*")?;
    let committer = committer_from_git_env(&author).context("GIT_COMMITTER_*")?;
    Ok((author, committer))
}
