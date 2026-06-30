//！ 获取命令行参数,接入对应函数
use crate::{init,
    object::{hash_object, write_hash_object},
    staging::{stage_paths, resolve_stage_inputs},
    git_paths::discover_repo_from_cwd,
    commit::commit,
    commit_identity::identities_from_git_env,
    reference::branch,
    checkout::{CheckoutTarget, checkout},
    get_packfile_by_network::ls_remote,
    parse_packfile::{clone, dir_name_from_url},
    fetch::fetch,
    merge::{merge, MergeOutcome},
    pull::pull,
    push::push,
    status::{status, print_status},
    log::log,
};
use anyhow::Ok;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
#[derive(Subcommand, Debug)]

pub enum GiftCommand {
    /// 初始化一个新的 gift 仓库；未指定路径时在当前目录创建 `.gift`。
    Init {path: Option<String>},
    /// 计算文件内容的对象哈希；使用 `-w` 时将对象写入 `.gift/objects`。
    HashObject {
        /// 是否把计算出的对象内容写入对象库。
        #[arg(short = 'w')]
        write: bool,

        /// 需要计算哈希的文件路径。
        file: String
    },
    /// 将指定文件或目录加入暂存区。
    Add { inputs: Vec<String> },
    /// 基于当前暂存区创建一次提交。
    Commit {
        /// 提交消息；省略时由提交逻辑决定默认行为。
        #[arg(short = 'm')]
        message: Option<String>,
    },
    /// 将另一个 commit 或分支合并到当前分支。
    /// `target` 可以是 40 位 hex OID 或本地分支名（如 `feature`）。
    Merge {
        /// 要合并的本地分支名或 40 位 commit OID。
        target: String,
        /// 提交消息；省略时自动生成（"Merge branch/commit '<target>'"）
        #[arg(short = 'm')]
        message: Option<String>,
    },
    /// 从当前 HEAD 创建一个新的本地分支。
    Branch{
        /// 要创建的分支名。
        name: String
    },
    /// 切换到指定分支、提交或其它可解析的检出目标。
    Checkout{
        /// 要检出的目标。
        target: CheckoutTarget
    },

    /// 显示当前工作区和暂存区状态。
    Status,

    /// 显示从 HEAD 开始的提交历史。
    Log {
        /// 最多显示的提交数量；省略时显示所有可达提交。
        #[arg(short = 'n', long = "max-count")]
        max_count: Option<usize>,
    },

    /// 查看远端仓库公开的引用列表。
    LsRemote{
        /// 远端仓库地址。
        url: String
    },

    /// 克隆远端仓库到根据 URL 推导出的本地目录。
    Clone{
        /// 远端仓库地址。
        url: String
    },

    /// 从远端拉取对象并更新远程跟踪引用，不改动 HEAD、本地分支或工作区。
    Fetch{
        /// 远端仓库地址。
        url: String,
        /// 远端名称；省略时使用 `origin`。
        #[arg(default_value = "origin")]
        remote: String
    },

    /// fetch + merge：从远端拉取后，把对应的远程分支并入当前分支。
    /// `branch` 省略时取当前分支同名（`-b` 指定其它分支）。
    Pull{
        /// 远端仓库地址。
        url: String,
        /// 远端名称；省略时使用 `origin`。
        #[arg(default_value = "origin")]
        remote: String,
        /// 要拉取并合并的远端分支；省略时使用当前分支同名分支。
        #[arg(short = 'b', long = "branch")]
        branch: Option<String>,
        /// 合并提交消息；省略时自动生成。
        #[arg(short = 'm')]
        message: Option<String>,
    },

    /// 把本地分支推到远端同名分支。`branch` 省略时取当前分支；`-f` 强推（跳过快进检查）。
    Push{
        /// 远端仓库地址。
        url: String,
        /// 远端名称；省略时使用 `origin`。
        #[arg(default_value = "origin")]
        remote: String,
        /// 要推送的本地分支；省略时使用当前分支。
        #[arg(short = 'b', long = "branch")]
        branch: Option<String>,
        /// 是否强制推送，跳过快进检查。
        #[arg(short = 'f', long = "force")]
        force: bool,
    }

}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]

struct Args {
    // command
    // clap 需要的 trait
    #[command(subcommand)]
    command: GiftCommand,

}

pub fn get_args_and_go() -> Result<(), anyhow::Error>  {
    let args = Args::parse();

    match args.command {

        GiftCommand::Init {path} => {
            let gift_path = match path {
                Some(proj_path) => PathBuf::from(proj_path).join(".gift"),                 
                None => PathBuf::from(".gift")
            };
            init(gift_path)?;
            Ok(())
        },

        GiftCommand::HashObject {write, file}=> {
            let my_obj_path = PathBuf::from(file);
            let (obj_hash, obj_content) = hash_object(&my_obj_path).unwrap();
            println!("{}",obj_hash.to_string());
            if write{
                let root = Path::new(".gift");
                write_hash_object(&root, &obj_hash, &obj_content)?;
            }
            Ok(())
        },

        //这里暂时先假设.gift文件夹就在worktree底下，不分离
        //但是进程文件不一定就直接在worktree底下
        GiftCommand::Add {inputs} => {
            let abs_path = discover_repo_from_cwd()?;
            let inputs_path: Vec<PathBuf> = inputs.into_iter()
                .map(PathBuf::from)
                .collect();
            let resolved = resolve_stage_inputs(&inputs_path, &abs_path.worktree, &abs_path.git_abs)?;
            stage_paths(&abs_path.git_abs, &abs_path.worktree, &resolved, true)?;
            Ok(())
        },

        GiftCommand::Commit { message } => {
            let abs_path = discover_repo_from_cwd()?;
            let (auther_about, committer_about) = identities_from_git_env()?;
            let sha = commit(
                abs_path.worktree.as_path(),
                &abs_path.git_abs,
                auther_about,
                committer_about,
                message,
            )?;
            println!("the commit ID:{}", sha.to_string());
            Ok(())
        }

        GiftCommand::Merge { target, message } => {
            let abs_path = discover_repo_from_cwd()?;
            let (author, committer) = identities_from_git_env()?;
            // 分支名用 "Merge branch '...'"，hex OID 用 "Merge commit '...'"
            let is_oid = target.len() == 40 && target.chars().all(|c| c.is_ascii_hexdigit());
            let msg = message.unwrap_or_else(|| {
                if is_oid {
                    format!("Merge commit '{}'\n", target)
                } else {
                    format!("Merge branch '{}'\n", target)
                }
            });
            let outcome = merge(&abs_path.worktree, &abs_path.git_abs, &target, author, committer, &msg)?;
            report_merge_outcome(&outcome);
            Ok(())
        }

        GiftCommand::Branch {name}=> {
            let abs_path = discover_repo_from_cwd()?;
            let head_path = abs_path.git_abs.join("HEAD");
            branch(&abs_path.git_abs, head_path.as_path(), &name)?;
            Ok(())
        },
        GiftCommand::Checkout { target } =>{
            let abs_path = discover_repo_from_cwd()?;
            checkout(&abs_path.worktree, &abs_path.git_abs, target)?;
            Ok(())
        },
        GiftCommand::Status => {
            let abs_path = discover_repo_from_cwd()?;
            let repo_status = status(&abs_path.worktree, &abs_path.git_abs)?;
            print_status(&repo_status);
            Ok(())
        },
        GiftCommand::Log { max_count } => {
            let abs_path = discover_repo_from_cwd()?;
            log(&abs_path.git_abs, max_count)?;
            Ok(())
        },
        GiftCommand::LsRemote { url } =>{
            ls_remote(&url)?;
            Ok(())
        },
        GiftCommand::Clone { url }=>{
            let dir = dir_name_from_url(&url)?;
            println!("Cloning into '{dir}'...");
            clone(&url, &dir)?;
            Ok(())
        },
        GiftCommand::Fetch { url, remote }=>{
            let abs_path = discover_repo_from_cwd()?;
            fetch(&abs_path.git_abs, &url, &remote)?;
            Ok(())
        }
        GiftCommand::Pull { url, remote, branch, message }=>{
            let abs_path = discover_repo_from_cwd()?;
            let (author, committer) = identities_from_git_env()?;
            let outcome = pull(
                &abs_path.worktree,
                &abs_path.git_abs,
                &url,
                &remote,
                branch.as_deref(),
                author,
                committer,
                message,
            )?;
            report_merge_outcome(&outcome);
            Ok(())
        }
        GiftCommand::Push { url, remote, branch, force }=>{
            let abs_path = discover_repo_from_cwd()?;
            push(&abs_path.worktree, &abs_path.git_abs, &url, &remote, branch.as_deref(), force)?;
            Ok(())
        }

    }
}

/// 打印 merge / pull 的结果；冲突时打印冲突文件并以非零码退出（与 git 一致）。
fn report_merge_outcome(outcome: &MergeOutcome) {
    match outcome {
        MergeOutcome::AlreadyUpToDate => println!("Already up to date."),
        MergeOutcome::FastForward(oid) => println!("Fast-forward\n  HEAD -> {}", oid.to_string()),
        MergeOutcome::Clean(oid) => println!("Merge made by 'resolve' strategy.\n  {}", oid.to_string()),
        MergeOutcome::Conflict(paths) => {
            eprintln!("CONFLICT: automatic merge failed; fix conflicts and then commit.");
            for p in paths {
                eprintln!("  conflict: {}", p.display());
            }
            std::process::exit(1);
        }
    }
}
