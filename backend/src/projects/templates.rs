/// Templates for generating Makefile and README.md for new projects.

/// Generate a Makefile based on selected languages and app type.
/// Always includes: `dev`, `kill-ports`, `restart` targets.
pub fn generate_makefile(
    project_name: &str,
    app_type: &str,
    languages: &[String],
    frontend_port: u16,
    backend_port: u16,
) -> String {
    let mut content = String::new();

    // Determine which ports are needed
    let has_backend = languages.iter().any(|l| matches!(l.as_str(), "rust" | "golang" | "python" | "java"));
    let has_frontend = languages.iter().any(|l| matches!(l.as_str(), "react" | "vue" | "svelte" | "arkts" | "swift"));

    // Header
    content.push_str(&format!("# {} Makefile\n\n", project_name));
    if has_frontend {
        content.push_str(&format!("FRONTEND_PORT ?= {}\n", frontend_port));
    }
    if has_backend {
        content.push_str(&format!("BACKEND_PORT ?= {}\n", backend_port));
    }
    content.push('\n');

    // .PHONY declaration
    let mut phonies = vec!["dev", "kill-ports", "restart"];
    if has_frontend { phonies.insert(0, "frontend"); }
    if has_backend { phonies.insert(if has_frontend { 1 } else { 0 }, "backend"); }
    content.push_str(&format!(".PHONY: {}\n\n", phonies.join(" ")));

    // dev target
    content.push_str("## 启动所有服务\n");
    if has_frontend && has_backend {
        content.push_str("dev: frontend backend\n\n");
    } else if has_frontend {
        content.push_str("dev: frontend\n\n");
    } else if has_backend {
        content.push_str("dev: backend\n\n");
    } else {
        content.push_str("dev:\n\t@echo \"No dev servers configured.\"\n\n");
    }

    // frontend target
    if has_frontend {
        let frontend_cmd = match languages.iter().find(|l| matches!(l.as_str(), "react" | "vue" | "svelte")).map(|s| s.as_str()) {
            Some("react") | Some("vue") | Some("svelte") | None => "cd frontend && npm run dev -- --host 0.0.0.0 --port $(FRONTEND_PORT)",
            _ => "@echo \"Frontend dev command not configured.\"",
        };
        content.push_str("## 仅启动前端（绑定 0.0.0.0）\n");
        content.push_str("frontend:\n");
        content.push_str(&format!("\t{}\n\n", frontend_cmd));
    }

    // backend target
    if has_backend {
        let backend_cmd = match languages.iter().find(|l| matches!(l.as_str(), "rust" | "golang" | "python" | "java")).map(|s| s.as_str()) {
            Some("rust") => "cd backend && cargo run -- --host 127.0.0.1 --port $(BACKEND_PORT)",
            Some("golang") => "cd backend && go run . --host 127.0.0.1 --port $(BACKEND_PORT)",
            Some("python") => "cd backend && python main.py --host 127.0.0.1 --port $(BACKEND_PORT)",
            Some("java") => "cd backend && ./gradlew bootRun --args='--server.port=$(BACKEND_PORT)'",
            _ => "@echo \"Backend dev command not configured.\"",
        };
        content.push_str("## 仅启动后端（绑定 127.0.0.1）\n");
        content.push_str("backend:\n");
        content.push_str(&format!("\t{}\n\n", backend_cmd));
    }

    // kill-ports target (cross-platform: macOS + Linux)
    content.push_str("## 杀掉端口进程（兼容 macOS 和 Linux）\n");
    content.push_str("kill-ports:\n");
    if has_frontend {
        // lsof works on both macOS and Linux; fuser as fallback for minimal Linux installs
        content.push_str("\t@lsof -ti:$(FRONTEND_PORT) | xargs kill -9 2>/dev/null || fuser -k $(FRONTEND_PORT)/tcp 2>/dev/null || true\n");
    }
    if has_backend {
        content.push_str("\t@lsof -ti:$(BACKEND_PORT) | xargs kill -9 2>/dev/null || fuser -k $(BACKEND_PORT)/tcp 2>/dev/null || true\n");
    }
    if !has_frontend && !has_backend {
        content.push_str("\t@echo \"No ports to kill.\"\n");
    }
    content.push('\n');

    // restart target
    content.push_str("## 重启所有服务\n");
    content.push_str("restart: kill-ports dev\n");

    content
}

/// Generate a README.md for the project.
pub fn generate_readme(
    project_name: &str,
    app_type: &str,
    languages: &[String],
    description: Option<&str>,
) -> String {
    let mut content = String::new();

    content.push_str(&format!("# {}\n\n", project_name));

    let app_type_display = match app_type {
        "web" => "Web",
        "devops" => "DevOps",
        "harmonyos" => "HarmonyOS",
        "ios" => "iOS",
        "android" => "Android",
        _ => "Unknown",
    };

    content.push_str(&format!("{} application — {}\n\n", app_type_display, languages.join(", ")));

    if let Some(desc) = description {
        content.push_str(&format!("{}\n\n", desc));
    }

    content.push_str("## Quick Start\n\n");
    content.push_str("```bash\n");
    content.push_str("make dev\n");
    content.push_str("```\n\n");

    content.push_str("## Available Commands\n\n");
    content.push_str("| Command | Description |\n");
    content.push_str("|---------|-------------|\n");
    content.push_str("| `make dev` | Start development servers |\n");
    content.push_str("| `make kill-ports` | Kill processes on configured ports |\n");
    content.push_str("| `make restart` | Kill ports and restart all servers |\n");

    content
}

/// Generate .milletcode/project.json for the project.
pub fn generate_project_json(
    name: &str,
    app_type: &str,
    description: Option<&str>,
    backend_lang: Option<&str>,
    frontend_lang: Option<&str>,
    style: Option<&str>,
    frontend_port: u16,
    backend_port: u16,
) -> String {
    let mut json = serde_json::json!({
        "name": name,
        "app_type": app_type,
        "languages": {
            "backend": backend_lang.unwrap_or(""),
            "frontend": frontend_lang.unwrap_or(""),
            "style": style.unwrap_or("")
        },
        "ports": {
            "frontend": frontend_port,
            "backend": backend_port
        }
    });

    if let Some(desc) = description {
        json["description"] = serde_json::Value::String(desc.to_string());
    }

    serde_json::to_string_pretty(&json).unwrap_or_default()
}
