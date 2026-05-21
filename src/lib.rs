use std::fs;
use std::path::Path;

pub mod git_paths;
pub mod object;
pub mod symbolic_ref;
pub mod reference;
pub mod head;
pub mod index;
pub mod commit;
pub mod commit_identity;
pub mod staging;

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

    Ok(())
}
