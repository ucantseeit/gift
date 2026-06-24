use std::fs;
use std::path::Path;

pub mod get_args;
pub mod git_paths;
pub mod object;
pub mod symbolic_ref;
pub mod reference;
pub mod head;
pub mod index;
pub mod commit;
pub mod commit_identity;
pub mod staging;
pub mod checkout;
pub mod deal_pktlines;
pub mod get_packfile_by_network;
pub mod parse_packfile;
pub mod fetch;
use get_args::get_args_and_go;
pub fn run() -> anyhow::Result<()> {
    get_args_and_go()
}

pub mod status;

#[cfg(test)]
mod tests;

pub fn init<P: AsRef<Path>>(root: P) -> Result<(), std::io::Error> {
    let root = root.as_ref();
    let dirs = [
        root.join("branches"),
        root.join("hooks"),
        root.join("info"),
        root.join("objects/info"),
        root.join("objects/pack"),
        root.join("refs/heads"),
        root.join("refs/tags"),
    ];

    let files = [
        root.join("config"),
        root.join("description"),
        root.join("HEAD"),
    ];

    for dir in dirs {
        fs::create_dir_all(dir)?;
    }

    for file in files {
        fs::File::create(file)?;
    }

    let head_path = root.join("HEAD");
    fs::write(&head_path, b"ref: refs/heads/master\n")?;

    Ok(())
}
