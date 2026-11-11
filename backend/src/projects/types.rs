use serde::{Deserialize, Serialize};
use std::path::Path;

/// Absolute filesystem path to a project.
/// Stores keyword:path to uniquely identify projects from different discovery_paths
/// even if they point to the same filesystem location.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ProjectPath(pub String);

impl ProjectPath {
    /// Create a ProjectPath from keyword and path
    pub fn from_keyword_path(keyword: &str, path: &Path) -> Self {
        Self(format!("{}:{}", keyword, path.to_string_lossy()))
    }

    pub fn from_path(path: &Path) -> Self {
        Self(path.to_string_lossy().to_string())
    }

    /// Get the path string (keyword:path format)
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Extract the actual filesystem path (without keyword prefix)
    pub fn filesystem_path(&self) -> String {
        if let Some(idx) = self.0.find(':') {
            self.0[idx + 1..].to_string()
        } else {
            self.0.clone()
        }
    }

    /// Extract the keyword from the path (if in keyword:path format)
    pub fn keyword(&self) -> Option<String> {
        if let Some(idx) = self.0.find(':') {
            Some(self.0[..idx].to_string())
        } else {
            None
        }
    }
}

impl std::fmt::Display for ProjectPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A project discovered from the filesystem
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    /// Unique identifier (same as path for local projects)
    pub id: String,
    /// Absolute path to the project directory
    pub path: ProjectPath,
    /// Project name (directory basename)
    pub name: String,
    /// Hostname for remote projects (empty for local)
    pub hostname: String,
    /// Remote mode: "" = local, "ssh" = SSH remote (future: "s3"/"grpc"/"nfs")
    pub remote_mode: String,
    /// Whether this path is a mobile project root (HarmonyOS/iOS/Android)
    pub is_mobile_home: bool,
    /// Project description (from README.md)
    pub description: String,
    /// Last modified date
    pub last_modified: String,
    /// Keyword (tag) from project_discovery_paths configuration
    pub keyword: String,
    /// Frontend dev server port (if detected)
    pub frontend_port: Option<u16>,
    /// Backend dev server port (if detected)
    pub backend_port: Option<u16>,
    /// Whether the project is archived
    pub archived: bool,
}
