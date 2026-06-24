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
};
use anyhow::Ok;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
#[derive(Subcommand, Debug)]

enum GiftCommand {
    Init {path: Option<String>},
    HashObject {
        #[arg(short = 'w')]
        write: bool,

        file: String
    },
    Add { inputs: Vec<String> },
    Commit {
        #[arg(short = 'm')]
        message: Option<String>,
    },
    /// 将另一个 commit 或分支合并到当前分支。
    /// `target` 可以是 40 位 hex OID 或本地分支名（如 `feature`）。
    Merge {
        target: String,
        /// 提交消息；省略时自动生成（"Merge branch/commit '<target>'"）
        #[arg(short = 'm')]
        message: Option<String>,
    },
    //此时实现的是必须要加name的，即创建新分支功能
    Branch{
        name: String
    },
    Checkout{
        target: CheckoutTarget
    },

    Status,

    LsRemote{
        url: String
    },

    Clone{
        url: String
    },

    //从远端拉取对象并更新远程跟踪引用 refs/remotes/<remote>/*；
    //不动 HEAD / 本地分支 / 工作区。remote 默认 "origin"。
    Fetch{
        url: String,
        #[arg(default_value = "origin")]
        remote: String
    },

    /// fetch + merge：从远端拉取后，把对应的远程分支并入当前分支。
    /// `branch` 省略时取当前分支同名（`-b` 指定其它分支）。
    Pull{
        url: String,
        #[arg(default_value = "origin")]
        remote: String,
        #[arg(short = 'b', long = "branch")]
        branch: Option<String>,
        #[arg(short = 'm')]
        message: Option<String>,
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
        GiftCommand::Status => {println!("Status"); Ok(())},
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
