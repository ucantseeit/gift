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
    parse_packfile::{clone, dir_name_from_url}
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
        message: String
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

        GiftCommand::Commit {message}=> {
            let abs_path = discover_repo_from_cwd()?;
            let (auther_about, committer_about) = identities_from_git_env()?;
            let sha = commit(
                abs_path.worktree.as_path(), 
                &abs_path.git_abs, 
                auther_about, 
                committer_about, message
            )?;
            println!("the commit ID:{}", sha.to_string());
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
        }

    }
}
