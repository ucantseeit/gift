use anyhow::{Context, bail, ensure};
use hex;
use log::debug;
use sha1::{Digest, Sha1};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use crate::git_paths;
use crate::object::*;
use crate::index::index_tree::IndexRootTree;

use flate2::bufread::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::{BufReader, prelude::*};
use std::fs::File;



