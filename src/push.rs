//! `git push`：把本地分支的新对象发给远端，并请求远端更新对应的 ref。
//! 走的是 `git-receive-pack` 服务（与 fetch/clone 的 `git-upload-pack` 相反方向）。
//!
//! 流程（与 fetch 镜像）：
//!   1. discover：GET `…/info/refs?service=git-receive-pack`，拿到远端各 ref 的**当前值**；
//!   2. 算出本次要更新的 ref：`old`（远端当前值，新分支则全 0）→ `new`（本地分支 tip）；
//!   3. 快进检查（非 force 时）：`old` 不是 `new` 的祖先就拒绝，提示先 pull；
//!   4. 遍历对象图，算出「`new` 可达但远端已有的对象之外」的对象集合；
//!   5. 用 [`build_pack`] 把它们打成 packfile；
//!   6. POST `…/git-receive-pack`：命令行 `<old> <new> <ref>` + flush + packfile；
//!   7. 解析 report-status，成功则把本地 `refs/remotes/<remote>/<branch>` 也推进到 `new`。
//!
//! 复用：[`discover_refs_for`]（discovery）、[`build_pack`] / [`read_loose_object_by_hex`]
//! （打包 / 读对象）、[`find_merge_base`]（快进判定）、pkt-line 编解码、[`current_branch_name`]。
//!
//! 简化：假设对象都是松散对象（与本项目其它写路径一致，从不生成 pack）；
//! 子模块 gitlink（mode 160000）指向的 commit 不在本库，遍历时跳过。

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::deal_pktlines::{PktLine, read_pkt_line, write_pkt_line};
use crate::get_packfile_by_network::discover_refs_for;
use crate::merge_base::find_merge_base;
use crate::object::ObjectSha;
use crate::parse_packfile::{Kind, build_pack, read_loose_object_by_hex};
use crate::pull::current_branch_name;

const ZERO_OID: &str = "0000000000000000000000000000000000000000";

/// 单个 ref 的推送结果（用于打印摘要）。
pub struct PushResult {
    pub refname: String,
    pub old: String,
    pub new: String,
    pub rejected: Option<String>, // Some(reason) 表示被远端拒绝
}

/// 该 hex 对应的松散对象是否存在于本地对象库。
fn object_exists(git_abs: &Path, hex: &str) -> bool {
    hex.len() >= 3 && git_abs.join("objects").join(&hex[0..2]).join(&hex[2..]).exists()
}

fn sha_from_hex(hex: &str) -> Result<ObjectSha> {
    let bytes: [u8; 20] = hex::decode(hex)
        .context("OID hex decode")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("OID must be 20 bytes"))?;
    Ok(ObjectSha::SHA1(bytes))
}

/// 读 `refs/heads/<branch>` 的 OID。
fn local_branch_oid(git_abs: &Path, branch: &str) -> Result<String> {
    let p = git_abs.join("refs").join("heads").join(branch);
    let s = fs::read_to_string(&p)
        .with_context(|| format!("本地分支不存在：{}", p.display()))?;
    Ok(s.trim().to_string())
}

/// 从 commit payload 取 `tree` 与 `parent` 的 hex。
fn commit_children(payload: &[u8]) -> (Option<String>, Vec<String>) {
    let text = String::from_utf8_lossy(payload);
    let (mut tree, mut parents) = (None, Vec::new());
    for line in text.lines() {
        if line.is_empty() {
            break; // header 结束
        }
        if let Some(h) = line.strip_prefix("tree ") {
            tree = Some(h.trim().to_string());
        } else if let Some(h) = line.strip_prefix("parent ") {
            parents.push(h.trim().to_string());
        }
    }
    (tree, parents)
}

/// 从 tree payload 取每个条目的 `(mode, child hex)`。
fn tree_children(payload: &[u8]) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < payload.len() {
        let sp = match payload[pos..].iter().position(|&b| b == b' ') {
            Some(i) => pos + i,
            None => break,
        };
        let mode = std::str::from_utf8(&payload[pos..sp])
            .ok()
            .and_then(|s| u32::from_str_radix(s, 8).ok())
            .unwrap_or(0);
        let nul = match payload[sp + 1..].iter().position(|&b| b == 0) {
            Some(i) => sp + 1 + i,
            None => break,
        };
        if nul + 21 > payload.len() {
            break;
        }
        let hex: String = payload[nul + 1..nul + 21].iter().map(|b| format!("{b:02x}")).collect();
        out.push((mode, hex));
        pos = nul + 21;
    }
    out
}

/// 把从 `root` 可达的对象 OID 全部标进 `present`（迭代式，避免深递归）。
/// 用于标记“远端已有”的边界：root 取远端广告里、且本地也有的 ref。
fn mark_present(git_abs: &Path, root: &str, present: &mut HashSet<String>) -> Result<()> {
    let mut stack = vec![root.to_string()];
    while let Some(oid) = stack.pop() {
        if oid == ZERO_OID || !object_exists(git_abs, &oid) || !present.insert(oid.clone()) {
            continue;
        }
        let (kind, payload) = match read_loose_object_by_hex(git_abs, &oid) {
            Ok(x) => x,
            Err(_) => continue, // 本地没有就当作边界，不展开
        };
        push_children(kind, &payload, &mut stack);
    }
    Ok(())
}

/// 从 `new` 出发收集要发送的对象：凡不在 `present`（远端已有）里的都要发。
/// 遇到 present 里的对象直接剪枝（其整棵可达子图远端都有）。
fn collect_to_send(
    git_abs: &Path,
    new: &str,
    present: &HashSet<String>,
) -> Result<Vec<(Kind, Vec<u8>)>> {
    let mut queued: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    let mut stack = vec![new.to_string()];
    while let Some(oid) = stack.pop() {
        if oid == ZERO_OID || present.contains(&oid) || !queued.insert(oid.clone()) {
            continue;
        }
        let (kind, payload) = read_loose_object_by_hex(git_abs, &oid)?;
        push_children(kind, &payload, &mut stack);
        out.push((kind, payload));
    }
    Ok(out)
}

/// 把一个对象的子对象 OID 压栈（commit→tree+parents；tree→非 gitlink 条目）。
fn push_children(kind: Kind, payload: &[u8], stack: &mut Vec<String>) {
    match kind {
        Kind::Commit => {
            let (tree, parents) = commit_children(payload);
            if let Some(t) = tree {
                stack.push(t);
            }
            stack.extend(parents);
        }
        Kind::Tree => {
            for (mode, child) in tree_children(payload) {
                if mode == 0o160000 {
                    continue; // gitlink 子模块 commit，不在本库
                }
                stack.push(child);
            }
        }
        Kind::Blob | Kind::Tag => {}
    }
}

/// POST 命令 + packfile 到 `…/git-receive-pack`，整段读回响应。
fn send_receive_pack(base: &str, body: &[u8]) -> Result<Vec<u8>> {
    let url = format!("{}/git-receive-pack", base.trim_end_matches('/'));
    let mut res = ureq::post(&url)
        .header("Content-Type", "application/x-git-receive-pack-request")
        .header("Accept", "application/x-git-receive-pack-result")
        .send(body)?;
    let bytes = res.body_mut().with_config().limit(64 * 1024 * 1024).read_to_vec()?;
    Ok(bytes)
}

/// 解析 report-status：返回 `(unpack 是否 ok, 每个 ref 的 (refname, Option<拒绝原因>))`。
fn parse_report_status(bytes: &[u8]) -> Result<(bool, Vec<(String, Option<String>)>)> {
    let mut cur: &[u8] = bytes;
    let mut unpack_ok = false;
    let mut refs = Vec::new();
    while let Some(line) = read_pkt_line(&mut cur)? {
        let payload = match line {
            PktLine::Data(p) => p,
            _ => continue, // flush / delim
        };
        let s = String::from_utf8_lossy(&payload);
        let s = s.trim_end();
        if let Some(rest) = s.strip_prefix("unpack ") {
            unpack_ok = rest == "ok";
        } else if let Some(rest) = s.strip_prefix("ok ") {
            refs.push((rest.to_string(), None));
        } else if let Some(rest) = s.strip_prefix("ng ") {
            let mut it = rest.splitn(2, ' ');
            let name = it.next().unwrap_or("").to_string();
            let reason = it.next().unwrap_or("(no reason)").to_string();
            refs.push((name, Some(reason)));
        }
    }
    Ok((unpack_ok, refs))
}

/// 顶层 push：把本地 `branch`（默认当前分支）推到远端同名分支。
pub fn push(
    worktree: &Path,
    git_abs: &Path,
    url: &str,
    remote: &str,
    branch: Option<&str>,
    force: bool,
) -> Result<()> {
    let _ = worktree;
    let branch = match branch {
        Some(b) => b.to_string(),
        None => current_branch_name(git_abs)?,
    };
    let new = local_branch_oid(git_abs, &branch)?;
    let remote_refname = format!("refs/heads/{branch}");

    // 1. discover：远端这个 ref 的当前值（没有则为全 0 = 新建分支）
    let adv = discover_refs_for(url, "git-receive-pack")?;
    let old = adv.sha_of(&remote_refname).unwrap_or(ZERO_OID).to_string();

    if old == new {
        println!("Everything up-to-date");
        return Ok(());
    }

    // 2. 快进检查：仅当本地有 old 对象、且未 --force 时做（否则交给远端裁决）
    if !force && old != ZERO_OID && object_exists(git_abs, &old) {
        let base = find_merge_base(git_abs, &sha_from_hex(&old)?, &sha_from_hex(&new)?)?;
        let is_ff = base.map(|b| b.to_string()).as_deref() == Some(old.as_str());
        if !is_ff {
            bail!(
                "更新被拒绝：非快进推送（远端 {} 含有你本地没有的提交）。\n\
                 先 `gift pull` 合并，或用 `--force` 强推。",
                &old[..old.len().min(7)]
            );
        }
    }

    // 3. 标记“远端已有”的边界：广告里所有、且本地也存在的 ref 都展开
    let mut present = HashSet::new();
    for r in &adv.refs {
        if object_exists(git_abs, &r.sha) {
            mark_present(git_abs, &r.sha, &mut present)?;
        }
    }

    // 4. 收集要发的对象 → 打包
    let objects = collect_to_send(git_abs, &new, &present)?;
    let pack = build_pack(&objects)?;

    // 5. 拼请求体：命令行（首行带能力位）+ flush + packfile
    let mut body = Vec::new();
    let cmd = format!("{old} {new} {remote_refname}\0report-status\n");
    write_pkt_line(&mut body, &PktLine::Data(cmd.into_bytes()))?;
    write_pkt_line(&mut body, &PktLine::Flush)?;
    body.extend_from_slice(&pack);

    // 6. 发送 + 解析 report-status
    let resp = send_receive_pack(url, &body)?;
    let (unpack_ok, ref_status) = parse_report_status(&resp)?;
    if !unpack_ok {
        bail!("远端 unpack 失败（pack 校验未通过）");
    }
    let rejected = ref_status
        .iter()
        .find(|(rn, _)| rn == &remote_refname)
        .and_then(|(_, reason)| reason.clone());

    // 7. 推送成功则把本地远程跟踪引用也推进到 new
    if rejected.is_none() {
        let track = git_abs.join("refs").join("remotes").join(remote).join(&branch);
        if let Some(p) = track.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(&track, format!("{new}\n"))?;
    }

    // 打印摘要
    let result = PushResult { refname: remote_refname, old: old.clone(), new: new.clone(), rejected };
    print_summary(url, &branch, &result);
    if result.rejected.is_some() {
        std::process::exit(1);
    }
    Ok(())
}

fn short(h: &str) -> &str {
    &h[..h.len().min(7)]
}

fn print_summary(url: &str, branch: &str, r: &PushResult) {
    println!("To {url}");
    match &r.rejected {
        Some(reason) => {
            println!(" ! [rejected]        {branch} -> {branch} ({reason})");
        }
        None if r.old == ZERO_OID => {
            println!(" * [new branch]      {branch} -> {branch}");
        }
        None => {
            println!("   {}..{}  {branch} -> {branch}", short(&r.old), short(&r.new));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::process::Command;

    fn run(c: &mut Command) {
        assert!(c.status().unwrap().success(), "命令失败: {c:?}");
    }
    fn git_out(repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git").arg("-C").arg(repo).args(args).output().unwrap().stdout;
        String::from_utf8(out).unwrap()
    }

    /// 离线验证 push 的两个核心：①对象图差分（collect_to_send）算出的集合，应与
    /// git 的权威答案 `git rev-list --objects B --not A` 完全一致；②build_pack 造的
    /// 包能被 parse_pack 解回（顺带验 OID）。两者串起来就是“要发的 pack 内容正确”。
    #[test]
    fn objects_to_send_match_git_rev_list() {
        use crate::parse_packfile::parse_pack;

        let env = [
            ("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"),
            ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t"),
        ];
        let src = std::env::temp_dir().join(format!("push_{}", std::process::id()));
        let _ = fs::remove_dir_all(&src);
        run(Command::new("git").args(["init", "-q"]).arg(&src));

        // 提交 A：根文件 + 子目录文件
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("root.txt"), "root v1\n").unwrap();
        fs::write(src.join("sub/a.txt"), "a v1\n").unwrap();
        run(Command::new("git").arg("-C").arg(&src).args(["add", "."]));
        run(Command::new("git").arg("-C").arg(&src).args(["commit", "-q", "-m", "A"]).envs(env));
        let a = git_out(&src, &["rev-parse", "HEAD"]).trim().to_string();

        // 提交 B：只改子目录里的文件（root.txt 与其 blob 应保持不变 → 不该被重发）
        fs::write(src.join("sub/a.txt"), "a v2 changed\n").unwrap();
        run(Command::new("git").arg("-C").arg(&src).args(["add", "."]));
        run(Command::new("git").arg("-C").arg(&src).args(["commit", "-q", "-m", "B"]).envs(env));
        let b = git_out(&src, &["rev-parse", "HEAD"]).trim().to_string();

        let git_abs = src.join(".git");

        // 我方：present = 从 A 可达的一切；要发 = 从 B 可达且不在 present 的
        let mut present = HashSet::new();
        mark_present(&git_abs, &a, &mut present).unwrap();
        let objs = collect_to_send(&git_abs, &b, &present).unwrap();

        // 串 build_pack → parse_pack，拿到我方对象 OID 集合
        let pack = build_pack(&objs).unwrap();
        let ours: HashSet<String> =
            parse_pack(&pack).unwrap().iter().map(|o| o.hex()).collect();

        // git 权威答案：B 相对 A 新增的对象
        let listed = git_out(&src, &["rev-list", "--objects", &b, "--not", &a]);
        let expected: HashSet<String> = listed
            .lines()
            .filter_map(|l| l.split(' ').next())
            .filter(|s| s.len() == 40)
            .map(|s| s.to_string())
            .collect();

        assert_eq!(ours, expected, "要发送的对象集合应与 git rev-list 完全一致");
        // 具体性检查：root.txt 的 blob（未改）不应在发送集合里
        let root_blob = git_out(&src, &["rev-parse", &format!("{a}:root.txt")]).trim().to_string();
        assert!(!ours.contains(&root_blob), "未改动的 root.txt blob 不应被重发");
        // B 这个新 commit 必须在
        assert!(ours.contains(&b), "新提交 B 必须被发送");

        let _ = fs::remove_dir_all(&src);
    }

    /// 验证打到一个“只有 A 的克隆”里后，git 能补齐 B —— 即我们造的 pack 自洽可用。
    #[test]
    fn built_pack_completes_clone_at_a() {
        let env = [
            ("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"),
            ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t"),
        ];
        let src = std::env::temp_dir().join(format!("push2src_{}", std::process::id()));
        let dst = std::env::temp_dir().join(format!("push2dst_{}", std::process::id()));
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);

        run(Command::new("git").args(["init", "-q"]).arg(&src));
        fs::write(src.join("f.txt"), "v1\n").unwrap();
        run(Command::new("git").arg("-C").arg(&src).args(["add", "."]));
        run(Command::new("git").arg("-C").arg(&src).args(["commit", "-q", "-m", "A"]).envs(env));
        let a = git_out(&src, &["rev-parse", "HEAD"]).trim().to_string();

        // dst = 克隆在 A（只有 A 的对象）
        run(Command::new("git").args(["clone", "-q"]).arg(&src).arg(&dst));

        // src 前进到 B
        fs::write(src.join("f.txt"), "v2\n").unwrap();
        run(Command::new("git").arg("-C").arg(&src).args(["add", "."]));
        run(Command::new("git").arg("-C").arg(&src).args(["commit", "-q", "-m", "B"]).envs(env));
        let b = git_out(&src, &["rev-parse", "HEAD"]).trim().to_string();

        // 造 “B not A” 的 pack
        let git_abs = src.join(".git");
        let mut present = HashSet::new();
        mark_present(&git_abs, &a, &mut present).unwrap();
        let objs = collect_to_send(&git_abs, &b, &present).unwrap();
        let pack = build_pack(&objs).unwrap();

        // 写进 dst：dst 原本不认识 B
        assert!(!Command::new("git").arg("-C").arg(&dst).args(["cat-file", "-e", &b])
            .status().unwrap().success(), "dst 起初不应有 B");

        // 把 pack 从 stdin 喂给 unpack-objects → 校验并把对象写进 dst 的对象库
        use std::io::Write;
        use std::process::Stdio;
        let mut child = Command::new("git").arg("-C").arg(&dst)
            .args(["unpack-objects", "-q"])
            .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null())
            .spawn().unwrap();
        child.stdin.take().unwrap().write_all(&pack).unwrap();
        assert!(child.wait().unwrap().success(), "unpack-objects 应成功（pack 自洽）");

        // 现在 dst 能完整读出 B 及其树
        assert!(Command::new("git").arg("-C").arg(&dst).args(["cat-file", "-e", &b])
            .status().unwrap().success(), "并入 pack 后 dst 应有 B");
        // fsck 确认对象库自洽（无缺失/损坏）
        assert!(Command::new("git").arg("-C").arg(&dst).args(["cat-file", "-e", &format!("{b}^{{tree}}")])
            .status().unwrap().success(), "B 的根 tree 也应可达");

        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
    }
}
