use crate::{init, 
    object::{hash_object, write_hash_object}, 
    staging::stage_paths,
    git_paths::discover_repo_from_cwd,
    commit::commit,
    commit_identity::identities_from_git_env
};
use anyhow::Ok;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
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

    Status,
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
            let (obj_hash, obj_content) = hash_object(my_obj_path).unwrap();
            println!("{:?}", obj_hash);
            if write{
                let root = ".gift".to_string();
                write_hash_object(root, &obj_hash, &obj_content)?;
            }
            Ok(())
        },

        //这里暂时先假设.gift文件夹就在work_tree底下，不分离
        //但是进程文件不一定就直接在work_tree底下
        GiftCommand::Add {inputs} => { 
            //应该传入.gift和worktree相对于当前目录的路径，绝对路径也可以
            let abs_path = discover_repo_from_cwd()?;
            let inputs_path: Vec<PathBuf> = inputs.into_iter()
                .map(PathBuf::from)
                .collect();
            //让递归项先为true,这样文件和文件夹都可以正确处理
            stage_paths(abs_path.git_dir, abs_path.work_tree, &inputs_path, true)?;
            Ok(())
        },

        GiftCommand::Status => {println!("Status"); Ok(())},

        GiftCommand::Commit {message}=> {
            let abs_path = discover_repo_from_cwd()?;
            let (auther_about, committer_about) = identities_from_git_env()?;
            let git_dir_rel = abs_path
                .git_dir
                .strip_prefix(&abs_path.work_tree)?
                .to_path_buf();
            let sha = commit(
                abs_path.work_tree.as_path(), 
                git_dir_rel, 
                auther_about, 
                committer_about, message
            )?;
            println!("the commit ID:{:?}", sha);
            Ok(())
        }
    }
}
