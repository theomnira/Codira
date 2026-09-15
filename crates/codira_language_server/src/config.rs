//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use codira_paths::AbsPathBuf;
use codira_project::ProjectManifest;

/// The configuration used by the language server.
#[derive(Debug, Clone)]
pub struct Config {
    pub watcher: FilesWatcher,

    /// The root directory of the workspace
    pub root_dir: AbsPathBuf,

    /// A collection of projects discovered within the workspace
    pub discovered_projects: Option<Vec<ProjectManifest>>,
}

impl Config {
    /// Constructs a new instance of a `Config`
    pub fn new(root_path: AbsPathBuf) -> Self {
        Self {
            watcher: FilesWatcher::Notify,
            root_dir: root_path,
            discovered_projects: None,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum FilesWatcher {
    Client,
    Notify,
}
