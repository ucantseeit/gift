use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;

use crate::get_packfile_by_network::{RefAdvertisement, discover_refs, fetch_pack};
use crate::parse_packfile::{PackedObject, parse_pack_thin, write_loose_object};

/// 一条成功更新的引用，便于打印 fetch 摘要。
#[derive(Debug)]
pub struct FetchedRef {
    pub remote_name: String, // 远端原始 refname，如 refs/heads/main
    pub local_name: String,  // 本地落点，如 refs/remotes/origin/main
    pub sha: String,
}

/// 把广告里的引用映射成本地引用并写盘：
///   - `refs/heads/<x>` → `refs/remotes/<remote>/<x>`（远程跟踪分支）
///   - `refs/tags/<x>`  → `refs/tags/<x>`（标签是全局的，直接照搬）
///   - 其它命名空间（`refs/pull/...` 等）跳过
///
/// 同时（重）写 `FETCH_HEAD`，每行 `"<sha>\t\t<描述> of <url>"`。
/// `FETCH_HEAD` 不是逐字节兼容 git 的格式（我们没实现 merge 标记），
/// 仅作为“上次 fetch 了什么”的可读记录。
fn write_fetched_refs(
    git_abs: &Path,
    remote: &str,
    url: &str,
    adv: &RefAdvertisement,
) -> std::io::Result<Vec<FetchedRef>> {
    let mut fetched = Vec::new();
    let mut fetch_head = String::new();

    for r in &adv.refs {
        if r.name == "HEAD" || r.name.ends_with("^{}") {
            continue; // HEAD 不建跟踪引用；peeled(^{}) 行不写成 ref
        }

        // 决定本地落点（相对 git_abs 的路径）
        let (local_rel, desc) = if let Some(branch) = r.name.strip_prefix("refs/heads/") {
            (format!("refs/remotes/{remote}/{branch}"), format!("branch '{branch}'"))
        } else if let Some(tag) = r.name.strip_prefix("refs/tags/") {
            (r.name.clone(), format!("tag '{tag}'"))
        } else {
            continue; // 其它命名空间不跟踪
        };

        let path = git_abs.join(&local_rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, format!("{}\n", r.sha))?; // ref 文件内容就是 40 位 sha

        fetch_head.push_str(&format!("{}\t\t{} of {}\n", r.sha, desc, url));
        fetched.push(FetchedRef {
            remote_name: r.name.clone(),
            local_name: local_rel,
            sha: r.sha.clone(),
        });
    }

    fs::write(git_abs.join("FETCH_HEAD"), fetch_head)?;
    Ok(fetched)
}

/// 落盘半部（与 `clone_to_disk` 对位）：把内存里的对象写进**已存在**仓库的
/// 对象库，并更新远程跟踪引用 + FETCH_HEAD。不碰 HEAD / 本地分支 / 工作区。
///
/// `git_abs` 是已存在仓库的 git 目录绝对路径（如 `.../.gift`；测试里传 `.git`）。
pub fn fetch_to_disk(
    git_abs: &Path,
    remote: &str,
    url: &str,
    objects: &[PackedObject],
    adv: &RefAdvertisement,
) -> std::io::Result<Vec<FetchedRef>> {
    // 1) 对象落盘到现有对象库（已存在的会被 write_loose_object 跳过）
    for o in objects {
        write_loose_object(git_abs, o)?;
    }
    // 2) 远程跟踪引用 + FETCH_HEAD
    write_fetched_refs(git_abs, remote, url, adv)
}

/// 判断字符串是否是 40 位十六进制（一个 SHA1 oid）。
fn is_hex40(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 递归收集 `dir`（通常是 `refs/`）下所有直接 ref 文件里的 40 位 sha。
/// symref 文件（内容 `ref: ...`）会被 `is_hex40` 自然过滤掉。
fn collect_refs_recursive(dir: &Path, set: &mut HashSet<String>) -> io::Result<()> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for ent in rd {
        let path = ent?.path();
        if path.is_dir() {
            collect_refs_recursive(&path, set)?;
        } else if let Ok(s) = fs::read_to_string(&path) {
            let s = s.trim();
            if is_hex40(s) {
                set.insert(s.to_string());
            }
        }
    }
    Ok(())
}

/// 收集本地“已有对象”的代表集合，作为协商用的 have 列表：
/// 所有本地 ref（refs/heads、refs/remotes、refs/tags …）的 tip，外加 detached HEAD。
/// 服务器知道某个 have 后，即认为我们已拥有它及其可达的一切，从而只发新对象。
fn collect_local_haves(git_abs: &Path) -> io::Result<Vec<String>> {
    let mut set = HashSet::new();
    collect_refs_recursive(&git_abs.join("refs"), &mut set)?;
    if let Ok(head) = fs::read_to_string(git_abs.join("HEAD")) {
        let h = head.trim();
        if is_hex40(h) {
            set.insert(h.to_string()); // detached HEAD 直接是 sha
        }
    }
    Ok(set.into_iter().collect())
}

/// 顶层 fetch：复用 clone 的网络件套做增量协商，再走 fetch 专属的落盘。
///
/// 本项目没有 remote 配置系统，所以远端用 `url` 直接给出，`remote` 是要写到
/// `refs/remotes/<remote>/*` 的名字（CLI 默认 "origin"）。
pub fn fetch(git_abs: &Path, url: &str, remote: &str) -> Result<(), anyhow::Error> {
    let haves = collect_local_haves(git_abs)?; // 本地已有 → 协商，避免重复下载
    let adv = discover_refs(url)?; // 复用：远端 ref 广告（refs + caps + HEAD）
    let pack = fetch_pack(url, &adv, &haves)?; // 复用：发 want+have、side-band 解复用 → pack
    let objects = parse_pack_thin(&pack, git_abs)?; // thin-pack：外部基从本地对象库增厚

    let fetched = fetch_to_disk(git_abs, remote, url, &objects, &adv)?;

    // 打印一个朴素的 fetch 摘要
    println!("From {url}");
    for f in &fetched {
        let short = &f.sha[..f.sha.len().min(7)];
        println!("   {short}  {} -> {}", f.remote_name, f.local_name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn run(c: &mut Command) {
        assert!(c.status().unwrap().success(), "命令失败: {c:?}");
    }

    /// 离线验证落盘半部：源仓库当“服务器”，把对象 + 广告 fetch 进一个**已存在**
    /// 的目标仓库，断言：① 对象进了对象库；② refs/remotes/origin/* 正确建立；
    /// ③ HEAD 与本地分支**没有**被动过。
    #[test]
    fn fetch_to_disk_updates_tracking_refs_only() {
        let env = [
            ("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"),
            ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t"),
        ];

        // ── 源仓库（当服务器）：1 提交 + 1 标签，repack 成单 pack ──
        let src = std::env::temp_dir().join(format!("fetchsrc_{}", std::process::id()));
        let _ = fs::remove_dir_all(&src);
        run(Command::new("git").args(["init", "-q"]).arg(&src));
        fs::write(src.join("README.md"), "hello\n").unwrap();
        run(Command::new("git").arg("-C").arg(&src).args(["add", "."]));
        run(Command::new("git").arg("-C").arg(&src).args(["commit", "-q", "-m", "first"]).envs(env));
        run(Command::new("git").arg("-C").arg(&src).args(["tag", "v1"]).envs(env));
        run(Command::new("git").arg("-C").arg(&src).args(["repack", "-q", "-a", "-d"]));

        // 解析广告（复用 clone 的 parse_advertisement，经 upload-pack）
        use crate::get_packfile_by_network::parse_advertisement;
        let adv_bytes = Command::new("git").args(["upload-pack", "--advertise-refs"]).arg(&src)
            .output().unwrap().stdout;
        let mut cur: &[u8] = &adv_bytes;
        let adv = parse_advertisement(&mut cur).unwrap();
        // 默认分支名随 git 配置可能是 master/main，从广告推导而非硬编码
        let head_branch = adv.head_target.clone().expect("应有 symref=HEAD 指向默认分支");
        let branch_short = head_branch.strip_prefix("refs/heads/").unwrap().to_string();

        // 解析 pack（这里是自洽 pack，用普通 parse_pack 即可）
        let packdir = src.join(".git/objects/pack");
        let pack = fs::read_dir(&packdir).unwrap().filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().map_or(false, |x| x == "pack")).unwrap();
        let objects = crate::parse_packfile::parse_pack(&fs::read(&pack).unwrap()).unwrap();

        // ── 目标仓库：一个**已存在**的、互不相干的空仓库 ──
        let dst = std::env::temp_dir().join(format!("fetchdst_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dst);
        run(Command::new("git").args(["init", "-q"]).arg(&dst));
        let dst_git = dst.join(".git");
        let head_before = fs::read(dst_git.join("HEAD")).unwrap();

        // fetch 落盘（测试里 git 目录用 ".git" 以便直接用 git 验证）
        let fetched = fetch_to_disk(&dst_git, "origin", "../src", &objects, &adv).unwrap();
        assert!(!fetched.is_empty(), "应至少 fetch 到一条 ref");

        // ① 对象进了对象库：源的每个 ref tip 在目标里都能 cat-file -e
        for r in &adv.refs {
            if r.name.ends_with("^{}") { continue; }
            let ok = Command::new("git").arg("-C").arg(&dst)
                .args(["cat-file", "-e", &r.sha]).status().unwrap().success();
            assert!(ok, "对象 {} 应已写入目标对象库", r.sha);
        }

        // ② 远程跟踪分支建立且 sha 正确
        let head_sha = adv.sha_of(&head_branch).expect("源应有默认分支");
        let track = dst_git.join(format!("refs/remotes/origin/{branch_short}"));
        assert_eq!(fs::read_to_string(&track).unwrap().trim(), head_sha,
                   "refs/remotes/origin/{branch_short} 应指向源默认分支");
        // 标签照搬
        assert!(dst_git.join("refs/tags/v1").exists(), "标签应照搬到 refs/tags/v1");
        // FETCH_HEAD 写出来了
        assert!(dst_git.join("FETCH_HEAD").exists(), "应写出 FETCH_HEAD");

        // ③ HEAD 没被动；本地分支不存在（fetch 不建本地分支）
        assert_eq!(fs::read(dst_git.join("HEAD")).unwrap(), head_before, "fetch 不应改 HEAD");
        assert!(!dst_git.join(&head_branch).exists(), "fetch 不应创建本地分支");

        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
    }
}
