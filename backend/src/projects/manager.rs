use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio_util::sync::CancellationToken;

use crate::config::ConfigManager;
use crate::projects::discovery;
use crate::projects::types::{Project, ProjectPath};

/// Description marker for synthetic skills-installer projects (local +
/// per-remote-node). list() hides anything carrying it — these workspaces
/// back third-party install sessions and are not user projects.
pub const INTERNAL_SKILLS_DESC: &str = "Internal skills installer workspace";

pub struct ProjectManager {
    projects: Arc<DashMap<ProjectPath, Project>>,
    config: Arc<ConfigManager>,
}

impl ProjectManager {
    pub fn new(config: Arc<ConfigManager>) -> Self {
        Self {
            projects: Arc::new(DashMap::new()),
            config,
        }
    }

    /// Load ports from YAML file and merge into project map.
    /// This is called on startup to restore port data without expensive scanning.
    pub fn load_ports_from_yaml(&self) {
        let home = match dirs::home_dir() {
            Some(h) => h,
            None => return,
        };
        let yaml_path = home.join(".milletcode").join("ports-in-dev.yaml");

        if !yaml_path.exists() {
            tracing::debug!("Ports YAML file does not exist: {:?}", yaml_path);
            return;
        }

        let content = match std::fs::read_to_string(&yaml_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to read ports YAML: {}", e);
                return;
            }
        };

        // Parse YAML (skip comment lines)
        let yaml_content: String = content
            .lines()
            .filter(|line| !line.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");

        #[derive(serde::Deserialize)]
        struct ProjectPort {
            path: String,
            frontend_port: Option<u16>,
            backend_port: Option<u16>,
        }

        #[derive(serde::Deserialize)]
        struct PortsYaml {
            projects: Vec<ProjectPort>,
        }

        let ports_data: PortsYaml = match serde_yaml::from_str(&yaml_content) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("Failed to parse ports YAML: {}", e);
                return;
            }
        };

        // Merge ports into existing projects
        for port_entry in ports_data.projects {
            // Find project by path
            let project_path = ProjectPath::from_path(std::path::Path::new(&port_entry.path));
            if let Some(mut project) = self.projects.get_mut(&project_path) {
                // Only update if YAML has port data and project doesn't already have it
                if port_entry.frontend_port.is_some() && project.frontend_port.is_none() {
                    project.frontend_port = port_entry.frontend_port;
                }
                if port_entry.backend_port.is_some() && project.backend_port.is_none() {
                    project.backend_port = port_entry.backend_port;
                }
            }
        }

        tracing::debug!("Loaded ports from YAML: {:?}", yaml_path);
    }

    /// Run a full scan and replace the project map.
    pub fn refresh(&self) {
        let paths = self.config.resolve_project_discovery_paths();
        let discovered = discovery::scan_projects(&paths);
        self.projects.clear();
        for project in discovered {
            self.projects.insert(project.path.clone(), project);
        }
        tracing::info!("Project scan complete: found {} projects", self.projects.len());

        // Load ports from YAML file (restores previously detected ports)
        self.load_ports_from_yaml();

        // Write ports to YAML file (updates with any newly detected ports)
        if let Err(e) = self.write_ports_yaml() {
            tracing::warn!("Failed to write ports YAML: {}", e);
        }

        // refresh() rebuilds the table from scratch — re-register the internal
        // skills-installer project so its sessions keep working across scans.
        self.register_internal_skills_project();
    }

    /// Register the internal skills-installer project backing third-party skill
    /// review sessions. Not visible in /api/projects (filtered by keyword).
    pub fn register_internal_skills_project(&self) {
        let workspace = match crate::runtime::skills::skills_workspace() {
            Ok(p) => p,
            Err(_) => return,
        };
        self.register_internal_skills_project_at(&workspace);
    }

    /// Same as above, but with an explicit working dir — install runs point the
    /// session cwd at a per-run scratch dir (no project semantics: no SKILL.md,
    /// no .claude), so agent CLIs can't mistake it for an install target.
    pub fn register_internal_skills_project_at(&self, dir: &std::path::Path) {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!("Failed to create skills scratch dir: {}", e);
            return;
        }
        let project = Project {
            id: "skills/installer".to_string(),
            path: ProjectPath(format!(
                "skills:{}",
                dir.to_string_lossy()
            )),
            name: "installer".to_string(),
            hostname: String::new(),
            remote_mode: String::new(),
            is_mobile_home: false,
            description: INTERNAL_SKILLS_DESC.to_string(),
            last_modified: String::new(),
            keyword: "skills".to_string(),
            frontend_port: None,
            backend_port: None,
            archived: false,
        };
        self.projects.insert(project.path.clone(), project);
    }

    /// Register a synthetic skills-installer workspace on a remote discovery
    /// node so third-party install sessions can run there via remote_mode=ssh.
    /// Same lifecycle caveat as the local one: refresh() wipes it, so callers
    /// re-register right before creating sessions.
    pub fn register_remote_skills_project(&self, keyword: &str, hostname: &str, remote_dir: &str) {
        let project = Project {
            id: format!("{keyword}/skills"),
            path: ProjectPath(format!("{keyword}:{remote_dir}")),
            name: "skills".to_string(),
            hostname: hostname.to_string(),
            remote_mode: "ssh".to_string(),
            is_mobile_home: false,
            description: INTERNAL_SKILLS_DESC.to_string(),
            last_modified: String::new(),
            keyword: keyword.to_string(),
            frontend_port: None,
            backend_port: None,
            archived: false,
        };
        self.projects.insert(project.path.clone(), project);
    }

    /// Write all project ports to ~/.milletcode/ports-in-dev.yaml
    fn write_ports_yaml(&self) -> Result<(), Box<dyn std::error::Error>> {
        use serde::Serialize;
        use std::io::Write;

        #[derive(Serialize)]
        struct ProjectPort {
            name: String,
            path: String,
            frontend_port: Option<u16>,
            backend_port: Option<u16>,
        }

        #[derive(Serialize)]
        struct PortsYaml {
            projects: Vec<ProjectPort>,
        }

        let projects: Vec<ProjectPort> = self
            .projects
            .iter()
            .filter(|p| p.frontend_port.is_some() || p.backend_port.is_some())
            .map(|p| ProjectPort {
                name: p.name.clone(),
                path: p.path.0.clone(),
                frontend_port: p.frontend_port,
                backend_port: p.backend_port,
            })
            .collect();

        if projects.is_empty() {
            return Ok(());
        }

        let yaml_data = PortsYaml { projects };
        let yaml_content = serde_yaml::to_string(&yaml_data)?;

        let home = dirs::home_dir().ok_or("Cannot find home directory")?;
        let config_dir = home.join(".milletcode");
        std::fs::create_dir_all(&config_dir)?;

        let yaml_path = config_dir.join("ports-in-dev.yaml");
        let mut file = std::fs::File::create(&yaml_path)?;
        file.write_all(b"# Auto-generated by MilletCode project scanner\n")?;
        file.write_all(b"# Last updated: ")?;
        file.write_all(chrono::Utc::now().to_rfc3339().as_bytes())?;
        file.write_all(b"\n")?;
        file.write_all(yaml_content.as_bytes())?;

        tracing::debug!("Wrote ports YAML to {:?}", yaml_path);
        Ok(())
    }

    pub fn list(&self) -> Vec<Project> {
        // Synthetic skills-installer workspaces (local keyword "skills" + the
        // per-remote-node "<keyword>/skills" variants) are never exposed.
        self.projects
            .iter()
            .map(|e| e.value().clone())
            .filter(|p| p.keyword != "skills" && p.description != INTERNAL_SKILLS_DESC)
            .collect()
    }

    pub fn get(&self, path: &ProjectPath) -> Option<Project> {
        self.projects.get(path).map(|r| r.value().clone())
    }

    /// Get project by id (format: "keyword/name")
    pub fn get_by_id(&self, id: &str) -> Option<Project> {
        self.projects.iter().find(|p| p.id == id).map(|p| p.value().clone())
    }

    /// Get the absolute filesystem path for a project.
    /// ProjectPath now stores only the absolute path (no hostname prefix).
    pub fn get_path(&self, path: &ProjectPath) -> Option<String> {
        self.projects.get(path).map(|r| r.value().path.0.clone())
    }

    /// Get the hostname for a project (empty for local projects).
    pub fn get_hostname(&self, path: &ProjectPath) -> Option<String> {
        self.projects.get(path).map(|r| r.value().hostname.clone())
    }

    /// Get all keywords that are configured as remote (SSH) projects.
    /// Returns a vector of keyword strings from the discovery paths config.
    pub fn remote_keywords(&self) -> Vec<String> {
        let paths = self.config.resolve_project_discovery_paths();
        paths
            .into_iter()
            .filter(|(_, (_, _, remote_mode, _, _))| remote_mode == "ssh")
            .map(|(keyword, _)| keyword)
            .collect()
    }

    /// Spawn a background task that periodically calls refresh().
    /// Takes an Arc<ProjectManager> so the scanner shares the same DashMap.
    pub fn spawn_scanner(
        manager: Arc<ProjectManager>,
        interval: Duration,
        cancel: CancellationToken,
    ) {
        tokio::spawn(async move {
            // initial scan
            manager.refresh();
            tracing::info!("Initial project scan: {} projects", manager.projects.len());

            loop {
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {
                        manager.refresh();
                        tracing::debug!("Periodic project scan: {} projects", manager.projects.len());
                    }
                    _ = cancel.cancelled() => {
                        tracing::info!("Project scanner shutting down");
                        break;
                    }
                }
            }
        });
    }
}
