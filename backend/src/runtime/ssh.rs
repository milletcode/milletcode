//! SSH remote process runtime implementation
//!
//! Uses russh to execute processes on remote hosts via SSH.

use anyhow::Result;
use async_trait::async_trait;
use russh::*;
use russh_keys::PublicKeyBase64;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex as TokioMutex, mpsc};

use super::{Process, Runtime};
use crate::agents::RuntimeContext;

/// SSH config entry parsed from ~/.ssh/config
#[derive(Debug, Clone, Default)]
pub struct SshHostConfig {
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_files: Vec<String>, // Support multiple IdentityFile entries
    pub server_alive_interval: Option<u64>,
    pub server_alive_count_max: Option<u32>,
    // New fields for enhanced SSH support
    pub proxy_jump: Option<String>,
    pub forward_agent: Option<bool>,
    pub strict_host_key_checking: Option<String>, // yes/no/ask
    pub password_authentication: Option<bool>,
    pub user_known_hosts_file: Option<String>,
}

/// Parse SSH config file and return a map of Host -> SshHostConfig
pub fn parse_ssh_config(config_path: &str) -> Result<HashMap<String, SshHostConfig>> {
    // If config_path is empty, default to ~/.ssh/config
    let effective_path = if config_path.is_empty() {
        "~/.ssh/config".to_string()
    } else {
        config_path.to_string()
    };

    let path = PathBuf::from(shellexpand::tilde(&effective_path).to_string());
    if !path.exists() {
        tracing::debug!("SSH config file not found: {}", path.display());
        return Ok(HashMap::new());
    }

    let content = std::fs::read_to_string(&path)?;
    let mut hosts = HashMap::new();
    let mut current_host: Option<String> = None;
    let mut current_config = SshHostConfig::default();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let parts: Vec<&str> = line.splitn(2, char::is_whitespace).collect();
        if parts.len() < 2 {
            continue;
        }

        let key = parts[0].to_lowercase();
        let value = parts[1].trim().to_string();

        match key.as_str() {
            "host" => {
                // Save previous host if exists
                if let Some(host) = current_host.take() {
                    hosts.insert(host, current_config);
                }
                current_host = Some(value);
                current_config = SshHostConfig::default();
            }
            "hostname" => current_config.hostname = Some(value),
            "user" => current_config.user = Some(value),
            "port" => current_config.port = value.parse().ok(),
            "identityfile" => current_config.identity_files.push(value),
            "serveraliveinterval" => current_config.server_alive_interval = value.parse().ok(),
            "serveralivecountmax" => current_config.server_alive_count_max = value.parse().ok(),
            // Enhanced SSH config fields
            "proxyjump" => current_config.proxy_jump = Some(value),
            "forwardagent" => current_config.forward_agent = Some(value.to_lowercase() == "yes"),
            "stricthostkeychecking" => current_config.strict_host_key_checking = Some(value.to_lowercase()),
            "passwordauthentication" => current_config.password_authentication = Some(value.to_lowercase() == "yes"),
            "userknownhostsfile" => current_config.user_known_hosts_file = Some(value),
            _ => {}
        }
    }

    // Save last host
    if let Some(host) = current_host.take() {
        hosts.insert(host, current_config);
    }

    tracing::debug!("Parsed {} SSH host configurations", hosts.len());
    Ok(hosts)
}

/// Resolve SSH configuration from hostname using ~/.ssh/config
/// Implements SSH config Host matching: merge Host * defaults with specific host config
pub fn resolve_ssh_config(hostname: &str, ssh_config_path: &str) -> Result<SshHostConfig> {
    let hosts = parse_ssh_config(ssh_config_path)?;

    // Start with wildcard defaults if present
    let mut config = hosts.get("*").cloned().unwrap_or_default();

    // Merge specific host config on top (specific settings override wildcard)
    if let Some(host_config) = hosts.get(hostname) {
        if host_config.hostname.is_some() {
            config.hostname = host_config.hostname.clone();
        }
        if host_config.port.is_some() {
            config.port = host_config.port;
        }
        if host_config.user.is_some() {
            config.user = host_config.user.clone();
        }
        if !host_config.identity_files.is_empty() {
            config.identity_files = host_config.identity_files.clone();
        }
        if host_config.server_alive_interval.is_some() {
            config.server_alive_interval = host_config.server_alive_interval;
        }
        if host_config.server_alive_count_max.is_some() {
            config.server_alive_count_max = host_config.server_alive_count_max;
        }
        if host_config.proxy_jump.is_some() {
            config.proxy_jump = host_config.proxy_jump.clone();
        }
        if host_config.forward_agent.is_some() {
            config.forward_agent = host_config.forward_agent;
        }
        if host_config.strict_host_key_checking.is_some() {
            config.strict_host_key_checking = host_config.strict_host_key_checking.clone();
        }
        if host_config.password_authentication.is_some() {
            config.password_authentication = host_config.password_authentication;
        }
        if host_config.user_known_hosts_file.is_some() {
            config.user_known_hosts_file = host_config.user_known_hosts_file.clone();
        }
    }

    Ok(config)
}

/// Known hosts entry
#[derive(Debug, Clone)]
pub struct KnownHostEntry {
    pub hostname: String,
    pub key_type: String,
    pub key_data: String,
}

/// Parse known_hosts file
pub fn parse_known_hosts(known_hosts_path: &str) -> Result<Vec<KnownHostEntry>> {
    let path = PathBuf::from(shellexpand::tilde(known_hosts_path).to_string());
    if !path.exists() {
        tracing::debug!("Known hosts file not found: {}", known_hosts_path);
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(&path)?;
    let mut entries = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 {
            entries.push(KnownHostEntry {
                hostname: parts[0].to_string(),
                key_type: parts[1].to_string(),
                key_data: parts[2].to_string(),
            });
        }
    }

    tracing::debug!("Parsed {} known hosts entries", entries.len());
    Ok(entries)
}

/// Check if a server key matches known hosts
pub fn verify_server_key(
    hostname: &str,
    port: u16,
    server_key: &russh::keys::PublicKey,
    known_hosts_path: &str,
) -> Result<bool> {
    let entries = parse_known_hosts(known_hosts_path)?;

    // Get the key type and base64 data
    let key_type = server_key.algorithm().to_string();
    let key_data = server_key.public_key_base64();

    // Check for exact hostname match or hostname:port match
    // In known_hosts, non-standard ports use [host]:port format
    let hostname_with_port = format!("[{}]:{}", hostname, port);

    tracing::debug!(
        hostname = hostname,
        port = port,
        key_type = key_type.as_str(),
        key_data = key_data.as_str(),
        hostname_with_port = hostname_with_port.as_str(),
        "Verifying server key against known_hosts"
    );

    // Track if we found any matching hostname (regardless of key type)
    let mut found_matching_host = false;

    for entry in &entries {
        // Check if hostname matches
        // For hashed hostnames, we need to verify the hash matches
        let matches = if entry.hostname.starts_with("|1|") {
            // Hashed hostname format: |1|salt|hash
            // We need to compute HMAC-SHA1(salt, hostname_with_port) and compare
            // For now, skip hashed hostnames since they require special handling
            // The plain hostname entries should be sufficient
            false
        } else {
            // Plain or [host]:port format
            entry.hostname == hostname || entry.hostname == hostname_with_port
        };

        if matches {
            found_matching_host = true;
            tracing::debug!(
                known_host = entry.hostname.as_str(),
                known_key_type = entry.key_type.as_str(),
                "Found matching hostname in known_hosts"
            );

            // OpenSSH behavior: accept any key type for a host
            // Only reject if same key type has different key data
            if entry.key_type == key_type {
                // Same key type - compare key data
                if entry.key_data == key_data {
                    tracing::debug!("Server key verified against known_hosts for {}", hostname);
                    return Ok(true);
                } else {
                    tracing::warn!(
                        hostname = hostname,
                        known_key_data = entry.key_data.as_str(),
                        server_key_data = key_data.as_str(),
                        "Key data mismatch in known_hosts"
                    );
                }
            } else {
                // Different key type - this is OK, OpenSSH stores multiple keys per host
                tracing::debug!(
                    hostname = hostname,
                    expected_key_type = key_type.as_str(),
                    found_key_type = entry.key_type.as_str(),
                    "Different key type in known_hosts (acceptable)"
                );
            }
        }
    }

    // If we found a matching hostname but no exact key match, accept it
    // This follows OpenSSH behavior: if a host is known, accept any key type
    if found_matching_host {
        tracing::debug!(
            hostname = hostname,
            "Accepted server key for known host (different key type)"
        );
        return Ok(true);
    }

    tracing::warn!(
        hostname = hostname,
        port = port,
        known_hosts_path = known_hosts_path,
        "Server key not found in known_hosts"
    );
    Ok(false)
}

/// SSH handler that processes server events
pub struct SshHandler {
    pub strict_host_key_checking: bool,
    pub known_hosts_path: String,
    pub hostname: String,
    pub port: u16,
}

#[async_trait]
impl client::Handler for SshHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool> {
        if !self.strict_host_key_checking {
            // StrictHostKeyChecking=no, accept all keys
            tracing::debug!("StrictHostKeyChecking disabled, accepting server key");
            return Ok(true);
        }

        // Verify against known_hosts
        match verify_server_key(
            &self.hostname,
            self.port,
            server_public_key,
            &self.known_hosts_path,
        ) {
            Ok(true) => {
                tracing::info!("Server key verified for {}:{}", self.hostname, self.port);
                Ok(true)
            }
            Ok(false) => {
                tracing::error!(
                    "Server key verification failed for {}:{}. Add to known_hosts or set StrictHostKeyChecking=no",
                    self.hostname,
                    self.port
                );
                Ok(false)
            }
            Err(e) => {
                tracing::error!("Error verifying server key: {}", e);
                Ok(false)
            }
        }
    }
}

/// Shared SSH connection handle (cloneable, thread-safe)
pub type SharedSshHandle = Arc<TokioMutex<client::Handle<SshHandler>>>;

/// Run a one-shot command on an existing shared handle and drain its output
/// until the channel closes. Fire-and-forget is not enough when the caller
/// must observe the command's effect (e.g. pkill) before continuing.
pub(crate) async fn exec_one_shot(handle: &SharedSshHandle, cmd: &str) -> Result<()> {
    let mut channel = handle.lock().await.channel_open_session().await?;
    channel.exec(false, cmd.to_string()).await?;
    while let Some(msg) = channel.wait().await {
        if matches!(msg, ChannelMsg::Close) {
            break;
        }
    }
    Ok(())
}

/// Establish an SSH connection and return a shared handle
pub async fn connect_ssh(ctx: &RuntimeContext) -> Result<SharedSshHandle> {
    connect_ssh_with_retry(ctx, 3).await
}

/// Establish an SSH connection with retry logic
pub async fn connect_ssh_with_retry(ctx: &RuntimeContext, max_retries: u32) -> Result<SharedSshHandle> {
    let mut last_error = None;

    for attempt in 0..=max_retries {
        if attempt > 0 {
            let delay = std::time::Duration::from_secs(2u64.pow(attempt - 1));
            tracing::info!(attempt, max_retries, ?delay, "Retrying SSH connection");
            tokio::time::sleep(delay).await;
        }

        match connect_ssh_inner(ctx).await {
            Ok(handle) => return Ok(handle),
            Err(e) => {
                tracing::warn!(attempt, error = %e, "SSH connection attempt failed");
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("SSH connection failed after {} retries", max_retries)))
}

/// Inner SSH connection logic (single attempt)
async fn connect_ssh_inner(ctx: &RuntimeContext) -> Result<SharedSshHandle> {
    // Resolve SSH config from SSH config file using hostname
    let ssh_host_config = if ctx.remote_mode != "ssh" {
        anyhow::bail!("connect_ssh_inner requires remote_mode = \"ssh\", got {:?}", ctx.remote_mode);
    } else if ctx.hostname.is_empty() {
        anyhow::bail!("hostname is required when remote_mode = \"ssh\"");
    } else {
        resolve_ssh_config(&ctx.hostname, &ctx.ssh_config_path)?
    };

    // All connection details come from SSH config
    let ssh_host = ssh_host_config.hostname
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Hostname not found in SSH config for host: {}", ctx.hostname))?;
    let ssh_port = ssh_host_config.port.unwrap_or(22);
    let ssh_username = ssh_host_config.user
        .clone()
        .ok_or_else(|| anyhow::anyhow!("User not found in SSH config for host: {}", ctx.hostname))?;
    let ssh_private_key_paths = ssh_host_config.identity_files.clone();

    if ssh_host.is_empty() {
        anyhow::bail!("hostname is required in SSH config for host: {}", ctx.hostname);
    }
    if ssh_username.is_empty() {
        anyhow::bail!("user is required in SSH config for host: {}", ctx.hostname);
    }

    // Determine host key verification settings
    let strict_host_key_checking = ssh_host_config.strict_host_key_checking
        .as_deref()
        .map(|v| v == "yes" || v == "true")
        .unwrap_or(true); // Default to strict checking

    let known_hosts_path = ssh_host_config.user_known_hosts_file
        .clone()
        .unwrap_or_else(|| "~/.ssh/known_hosts".to_string());

    tracing::info!(
        host = ssh_host,
        port = ssh_port,
        user = ssh_username,
        strict_host_key_checking,
        "Connecting to SSH host"
    );

    // Handle ProxyJump: connect through jump host first
    let handler = SshHandler {
        strict_host_key_checking,
        known_hosts_path,
        hostname: ssh_host.clone(),
        port: ssh_port,
    };

    let mut ssh_config = russh::client::Config::default();

    // Configure keepalive from SSH config
    if let Some(interval) = ssh_host_config.server_alive_interval {
        ssh_config.keepalive_interval = Some(std::time::Duration::from_secs(interval));
    }
    if let Some(count_max) = ssh_host_config.server_alive_count_max {
        ssh_config.keepalive_max = count_max as usize;
    }

    let ssh_config = Arc::new(ssh_config);

    let handle = if let Some(ref proxy_jump) = ssh_host_config.proxy_jump {
        // Connect via jump host
        connect_via_proxy_jump(ctx, &ssh_host_config, proxy_jump, ssh_config, handler).await?
    } else {
        // Direct connection
        let mut handle = client::connect(ssh_config, (ssh_host.as_str(), ssh_port), handler).await?;

        // Authenticate
        authenticate_ssh(&mut handle, &ssh_username, &ssh_private_key_paths, ctx, &ssh_host_config).await?;
        handle
    };

    tracing::info!("SSH connection established to {}:{}", ssh_host, ssh_port);

    Ok(Arc::new(TokioMutex::new(handle)))
}

/// Authenticate SSH connection using available methods
async fn authenticate_ssh(
    handle: &mut client::Handle<SshHandler>,
    ssh_username: &str,
    ssh_private_key_paths: &[String],
    ctx: &RuntimeContext,
    ssh_host_config: &SshHostConfig,
) -> Result<()> {
    let actual_host = ssh_host_config.hostname.as_deref().unwrap_or(&ctx.hostname);
    let actual_port = ssh_host_config.port.unwrap_or(22);

    // Collect all candidate key paths: explicit IdentityFile entries + defaults
    let mut candidate_paths: Vec<String> = Vec::new();

    for p in ssh_private_key_paths {
        let expanded = shellexpand::tilde(p).to_string();
        if !candidate_paths.contains(&expanded) {
            candidate_paths.push(expanded);
        }
    }

    // Add default private key paths if not already listed
    if let Ok(home) = std::env::var("HOME") {
        for default_name in &["id_rsa", "id_ed25519", "id_ecdsa"] {
            let default_path = format!("{}/.ssh/{}", home, default_name);
            if !candidate_paths.contains(&default_path) {
                candidate_paths.push(default_path);
            }
        }
    }

    tracing::debug!(
        host = actual_host,
        port = actual_port,
        user = ssh_username,
        key_paths = ?candidate_paths,
        "Trying SSH authentication"
    );

    // Try 0: SSH agent authentication (preferred - correctly negotiates signature algorithms)
    // This fixes the issue where RSA keys loaded from files use deprecated ssh-rsa algorithm
    // while the agent correctly negotiates rsa-sha2-256/512
    if let Ok(agent_socket) = std::env::var("SSH_AUTH_SOCK") {
        tracing::debug!(socket = %agent_socket, "Attempting SSH agent authentication");

        match russh_keys::agent::client::AgentClient::connect_env().await {
            Ok(mut agent) => {
                match agent.request_identities().await {
                    Ok(identities) => {
                        tracing::debug!(count = identities.len(), "SSH agent has {} keys loaded", identities.len());

                        // Try each key from the agent
                        for agent_key in identities {
                            let pub_key_bytes = agent_key.to_bytes().unwrap_or_default();
                            let algo_str = format!("{}", agent_key.algorithm());
                            tracing::debug!(
                                algorithm = %algo_str,
                                public_key_len = pub_key_bytes.len(),
                                "Trying SSH agent key"
                            );

                            match handle.authenticate_publickey_with(ssh_username, agent_key.clone(), &mut agent).await {
                                Ok(true) => {
                                    tracing::info!(
                                        host = actual_host,
                                        user = ssh_username,
                                        algorithm = %algo_str,
                                        "Authenticated via SSH agent"
                                    );
                                    return Ok(());
                                }
                                Ok(false) => {
                                    tracing::debug!(
                                        algorithm = %algo_str,
                                        "SSH agent key rejected by server"
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        algorithm = %algo_str,
                                        error = %e,
                                        "SSH agent authentication error"
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to list identities from SSH agent");
                    }
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "Failed to connect to SSH agent");
            }
        }
    } else {
        tracing::debug!("SSH_AUTH_SOCK not set, skipping SSH agent authentication");
    }

    // Try 1: Private key authentication (try every candidate path)
    for key_path in &candidate_paths {
        if !std::path::Path::new(key_path).exists() {
            tracing::trace!(key_path, "Skipping non-existent identity file");
            continue;
        }
        tracing::debug!(key_path, "Attempting to load SSH key");

        // Check file permissions and format before loading
        if let Ok(metadata) = std::fs::metadata(key_path) {
            use std::os::unix::fs::PermissionsExt;
            let perms = metadata.permissions().mode();
            tracing::debug!(key_path, permissions = format!("{:o}", perms), "Key file permissions");
        }

        // Read first few bytes to check format
        if let Ok(content) = std::fs::read(key_path) {
            let first_line = String::from_utf8_lossy(&content[..content.len().min(100)]);
            tracing::debug!(key_path, format = %first_line.lines().next().unwrap_or(""), "Key file format");
        }

        match russh_keys::load_secret_key(key_path, ctx.ssh_passphrase.as_deref()) {
            Ok(key) => {
                let key_type = format!("{:?}", key);
                let pub_key_bytes = key.public_key().to_bytes().unwrap_or_default();
                let algo_str = format!("{}", key.algorithm());
                tracing::info!(
                    key_path,
                    key_type = %key_type,
                    algorithm = %algo_str,
                    public_key_len = pub_key_bytes.len(),
                    "Successfully loaded SSH key, attempting authentication"
                );

                // For RSA keys, we need to use a custom signer that forces rsa-sha2-512
                // because the default implementation uses deprecated ssh-rsa
                if matches!(key.algorithm(), russh_keys::Algorithm::Rsa { hash: None }) {
                    tracing::debug!(key_path, "RSA key detected, using custom signer with rsa-sha2-512");

                    // Create a custom signer that forces rsa-sha2-512
                    struct RsaSha512Signer {
                        private_key: russh_keys::PrivateKey,
                    }

                    #[async_trait::async_trait]
                    impl russh::Signer for RsaSha512Signer {
                        type Error = russh::AgentAuthError;

                        async fn auth_publickey_sign(
                            &mut self,
                            key: &russh_keys::PublicKey,
                            to_sign: russh::CryptoVec,
                        ) -> Result<russh::CryptoVec, Self::Error> {
                            // Sign with rsa-sha2-512 using the workaround from russh-keys
                            use russh_keys::helpers::sign_workaround;
                            use russh_keys::ssh_encoding::Encode;

                            let signature = sign_workaround(&self.private_key, &to_sign)
                                .map_err(|e| russh::AgentAuthError::Key(russh_keys::Error::from(e)))?;

                            // Re-emit the full to-sign buffer, then append the signature as a
                            // single length-prefixed blob, exactly like the ssh-agent path
                            // (AgentClient::write_signature): to_sign + string(string("rsa-sha2-512") + string(sig)).
                            let mut result = russh::CryptoVec::new();
                            result.extend(&to_sign[..]);
                            let sig = signature.as_bytes().to_vec();
                            let outer_len = 4 + "rsa-sha2-512".len() + 4 + sig.len();
                            (outer_len as u32)
                                .encode(&mut result)
                                .map_err(|_| russh::AgentAuthError::Key(russh_keys::Error::KeyIsEncrypted))?;
                            "rsa-sha2-512"
                                .encode(&mut result)
                                .map_err(|_| russh::AgentAuthError::Key(russh_keys::Error::KeyIsEncrypted))?;
                            sig.as_slice()
                                .encode(&mut result)
                                .map_err(|_| russh::AgentAuthError::Key(russh_keys::Error::KeyIsEncrypted))?;

                            Ok(result)
                        }
                    }

                    // Build an rsa-sha2-512 public key to offer. ssh-key derives the
                    // algorithm as "ssh-rsa" from RSA KeyData, but modern OpenSSH servers
                    // disable ssh-rsa. Wrap the raw e+n bytes in an opaque key carrying the
                    // rsa-sha2-512 algorithm name, so the auth probe offers rsa-sha2-512
                    // (matching the rsa-sha2-512 signature the RsaSha512Signer produces).
                    let pub_key_for_auth = {
                        use russh_keys::ssh_encoding::Encode;

                        let rsa_pub = match key.public_key().key_data() {
                            russh_keys::ssh_key::public::KeyData::Rsa(rsa_pub) => rsa_pub,
                            _ => unreachable!("RSA key must have Rsa key data"),
                        };
                        // Build the full standard RSA public key blob
                        // (string("rsa-sha2-512") + mpint(e) + mpint(n)) and carry it as
                        // the raw bytes of an opaque key. The russh patch sends these raw
                        // bytes verbatim in the auth probe (no extra length-prefixed
                        // wrapper), so the server parses the two consecutive mpints.
                        let mut blob = Vec::new();
                        "ssh-rsa".encode(&mut blob)?;
                        rsa_pub.e.encode(&mut blob)?;
                        rsa_pub.n.encode(&mut blob)?;
                        russh_keys::PublicKey::new(
                            russh_keys::ssh_key::public::KeyData::Other(
                                russh_keys::ssh_key::public::OpaquePublicKey::new(
                                    blob,
                                    russh_keys::Algorithm::Rsa {
                                        hash: Some(russh_keys::HashAlg::Sha512),
                                    },
                                ),
                            ),
                            "",
                        )
                    };
                    tracing::debug!(
                        algorithm = %pub_key_for_auth.algorithm(),
                        "Offering rsa-sha2-512 public key to server"
                    );
                    let mut signer = RsaSha512Signer { private_key: key };

                    match handle.authenticate_publickey_with(ssh_username, pub_key_for_auth, &mut signer).await {
                        Ok(true) => {
                            tracing::info!(
                                key_path,
                                host = actual_host,
                                user = ssh_username,
                                "Authenticated via private key with rsa-sha2-512"
                            );
                            return Ok(());
                        }
                        Ok(false) => {
                            tracing::warn!(
                                key_path,
                                host = actual_host,
                                port = actual_port,
                                user = ssh_username,
                                "Permission denied for key - server rejected this key"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                key_path,
                                error = %e,
                                host = actual_host,
                                user = ssh_username,
                                "Key authentication error"
                            );
                        }
                    }
                } else {
                    // Non-RSA keys or RSA keys with explicit hash algorithm
                    match handle.authenticate_publickey(ssh_username, Arc::new(key)).await {
                        Ok(true) => {
                            tracing::info!(
                                key_path,
                                host = actual_host,
                                user = ssh_username,
                                "Authenticated via private key"
                            );
                            return Ok(());
                        }
                        Ok(false) => {
                            tracing::warn!(
                                key_path,
                                host = actual_host,
                                port = actual_port,
                                user = ssh_username,
                                "Permission denied for key - server rejected this key"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                key_path,
                                error = %e,
                                host = actual_host,
                                user = ssh_username,
                                "Key authentication error"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(key_path, error = %e, "Failed to load private key");
            }
        }
    }

    // Try 2: Password authentication
    let password_auth = ssh_host_config.password_authentication.unwrap_or(true);
    if password_auth {
        if let Some(ref password) = ctx.ssh_password {
            match handle.authenticate_password(ssh_username, password).await {
                Ok(true) => {
                    tracing::info!(host = actual_host, user = ssh_username, "Authenticated via password");
                    return Ok(());
                }
                Ok(false) => {
                    tracing::debug!(host = actual_host, "Password authentication denied");
                }
                Err(e) => {
                    tracing::debug!(host = actual_host, error = %e, "Password authentication error");
                }
            }
        }
    }

    anyhow::bail!(
        "No SSH authentication method succeeded for user '{}' on host '{}:{}' (tried {} key paths)",
        ssh_username,
        actual_host,
        actual_port,
        candidate_paths.len()
    )
}

/// Connect to target host via ProxyJump (supports multi-hop chains)
fn connect_via_proxy_jump<'a>(
    ctx: &'a RuntimeContext,
    target_config: &'a SshHostConfig,
    proxy_jump: &'a str,
    ssh_config: Arc<russh::client::Config>,
    handler: SshHandler,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<client::Handle<SshHandler>>> + Send + 'a>> {
    Box::pin(async move {
        // Parse proxy jump: could be "user@host:port" or just "host"
        let (jump_host, jump_user, jump_port) = parse_proxy_jump(proxy_jump);

        tracing::info!(jump_host, ?jump_user, jump_port, "Connecting via ProxyJump");

        // Resolve jump host config from SSH config
        let jump_config = resolve_ssh_config(&jump_host, &ctx.ssh_config_path)?;
        let jump_hostname = jump_config.hostname.as_deref().unwrap_or(&jump_host);
        let jump_port = jump_config.port.unwrap_or(jump_port);
        let default_user = std::env::var("USER").unwrap_or_else(|_| "root".to_string());
        let jump_username = jump_config.user.as_deref().or(jump_user.as_deref()).unwrap_or(&default_user);

        tracing::info!(
            jump_host,
            jump_hostname,
            jump_port,
            jump_username,
            jump_identity_files = ?jump_config.identity_files,
            jump_proxy_jump = ?jump_config.proxy_jump,
            "Resolved jump host config"
        );

        // Check if jump host itself has ProxyJump (multi-hop)
        let jump_handle = if let Some(ref nested_proxy) = jump_config.proxy_jump {
            // Recursively connect to jump host via its own ProxyJump
            // Use resolved hostname (IP) for known_hosts verification, not the alias
            let jump_handler = SshHandler {
                strict_host_key_checking: handler.strict_host_key_checking,
                known_hosts_path: handler.known_hosts_path.clone(),
                hostname: jump_hostname.to_string(),
                port: jump_port,
            };
            connect_via_proxy_jump(ctx, &jump_config, nested_proxy, ssh_config.clone(), jump_handler).await?
        } else {
            // Direct connection to jump host
            // Use resolved hostname (IP) for known_hosts verification, not the alias
            let jump_handler = SshHandler {
                strict_host_key_checking: handler.strict_host_key_checking,
                known_hosts_path: handler.known_hosts_path.clone(),
                hostname: jump_hostname.to_string(),
                port: jump_port,
            };

            let mut handle = client::connect(ssh_config.clone(), (jump_hostname, jump_port), jump_handler).await?;

            // Authenticate to jump host
            authenticate_ssh(&mut handle, jump_username, &jump_config.identity_files, ctx, &jump_config).await?;

            tracing::info!("Connected to jump host {}:{}", jump_hostname, jump_port);
            handle
        };

        // Open a direct-tcpip channel through the jump host to the target
        let target_host = target_config.hostname.as_ref().unwrap();
        let target_port = target_config.port.unwrap_or(22);

        let channel = jump_handle.channel_open_direct_tcpip(
            target_host.as_str(),
            target_port as u32,
            "127.0.0.1",
            0,
        ).await?;

        tracing::info!("Opened direct-tcpip channel to {}:{}", target_host, target_port);

        // Now connect to the target through this channel
        // Convert the channel into a stream that implements AsyncRead + AsyncWrite
        let stream = channel.into_stream();
        let mut handle = client::connect_stream(ssh_config, stream, handler).await?;

        // Authenticate to target host
        let target_user = target_config.user.as_deref().unwrap_or(jump_username);
        authenticate_ssh(&mut handle, target_user, &target_config.identity_files, ctx, target_config).await?;

        // Keep jump_handle alive by leaking it (it will be cleaned up when the process exits)
        // In a more sophisticated implementation, we'd store it alongside the target handle
        std::mem::forget(jump_handle);

        Ok(handle)
    })
}

/// Parse ProxyJump value: "user@host:port" or "host" or "host:port"
fn parse_proxy_jump(proxy_jump: &str) -> (String, Option<String>, u16) {
    let mut user = None;
    let mut host = proxy_jump.to_string();
    let mut port = 22u16;

    // Check for user@host
    if let Some(at_pos) = host.find('@') {
        user = Some(host[..at_pos].to_string());
        host = host[at_pos + 1..].to_string();
    }

    // Check for host:port
    if let Some(colon_pos) = host.rfind(':') {
        if let Ok(p) = host[colon_pos + 1..].parse::<u16>() {
            port = p;
            host = host[..colon_pos].to_string();
        }
    }

    (host, user, port)
}

/// SSH remote runtime (uses a shared connection handle)
pub struct SshRuntime {
    handle: SharedSshHandle,
}

impl SshRuntime {
    /// Connect to a remote host via SSH (creates a new connection)
    pub async fn connect(ctx: &RuntimeContext) -> Result<Self> {
        let handle = connect_ssh(ctx).await?;
        Ok(Self { handle })
    }

    /// Get the shared handle (for connection pooling)
    pub fn shared_handle(&self) -> SharedSshHandle {
        self.handle.clone()
    }

}

/// SSH process wrapper
///
/// The actual SSH Channel is owned by a single background task (spawned in `Runtime::spawn`).
/// This struct holds mpsc endpoints that the background task uses to relay data.
pub struct SshProcess {
    channel_id: u32,
    stdin_tx: Option<mpsc::Sender<Vec<u8>>>,
    stdout_rx: Option<mpsc::Receiver<Vec<u8>>>,
    stderr_rx: Option<mpsc::Receiver<Vec<u8>>>,
    exit_rx: Option<mpsc::Receiver<i32>>,
    signal_tx: Option<mpsc::Sender<Sig>>,
}

#[async_trait]
impl Process for SshProcess {
    fn take_stdin(&mut self) -> Option<Box<dyn AsyncWrite + Send + Unpin>> {
        let tx = self.stdin_tx.take()?;
        Some(Box::new(SshStdinWriter { tx }))
    }

    fn take_stdout(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>> {
        let rx = self.stdout_rx.take()?;
        Some(Box::new(SshStdoutReader {
            rx,
            buffer: Vec::new(),
            pos: 0,
        }))
    }

    fn take_stderr(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>> {
        let rx = self.stderr_rx.take()?;
        Some(Box::new(SshStdoutReader {
            rx,
            buffer: Vec::new(),
            pos: 0,
        }))
    }

    async fn wait(&mut self) -> Result<i32> {
        if let Some(mut rx) = self.exit_rx.take() {
            Ok(rx.recv().await.unwrap_or(-1))
        } else {
            Ok(-1)
        }
    }

    async fn kill(&mut self) -> Result<()> {
        // Send SIGTERM to the remote process
        if let Some(signal_tx) = self.signal_tx.take() {
            let _ = signal_tx.send(Sig::TERM).await;
        }
        // Drop stdin to signal EOF to the remote process
        self.stdin_tx.take();
        // The background task will close the channel when stdin is dropped
        Ok(())
    }

    fn id(&self) -> u32 {
        self.channel_id
    }
}

#[async_trait]
impl Runtime for SshRuntime {
    async fn spawn(
        &self,
        command: &str,
        args: &[&str],
        working_dir: Option<&str>,
        env: Vec<(&str, &str)>,
    ) -> Result<Box<dyn Process>> {
        // Build the remote command string
        let mut cmd_parts = Vec::new();

        // Source nvm to ensure node/npm/claude are in PATH
        // This is needed because SSH sessions don't load .bashrc/.zshrc by default
        cmd_parts.push("source ~/.nvm/nvm.sh 2>/dev/null || true".to_string());

        // Export environment variables to the remote shell
        for (k, v) in env {
            cmd_parts.push(format!("export {}={}", shell_escape(k), shell_escape(v)));
        }

        if let Some(dir) = working_dir {
            cmd_parts.push(format!("cd {}", escape_remote_dir(dir)));
        }

        // Expand ~ to $HOME for command path (SSH doesn't expand ~ inside quotes)
        // Don't shell_escape the command path when it contains $HOME — single quotes
        // would prevent variable expansion. Use double quotes instead.
        let escaped_command = if command.starts_with("~/") {
            let expanded = format!("$HOME{}", &command[1..]);
            format!("\"{}\"", expanded.replace('"', "\\\""))
        } else if command == "~" {
            "\"$HOME\"".to_string()
        } else {
            shell_escape(command).to_string()
        };

        // Build the full command with arguments
        let mut full_cmd = escaped_command;
        for arg in args {
            full_cmd.push(' ');
            full_cmd.push_str(&shell_escape(arg));
        }
        cmd_parts.push(full_cmd);

        let remote_cmd = cmd_parts.join(" && ");
        tracing::info!(cmd = %remote_cmd, "Spawning remote process via SSH");

        // Open a new SSH channel
        let handle = self.handle.lock().await;
        let channel = handle.channel_open_session().await?;
        drop(handle); // Release lock so other channels can be opened

        // Execute the command on the remote host
        channel.exec(false, remote_cmd).await?;

        // Use a unique channel ID based on pointer address (since ChannelId doesn't expose inner u32)
        let channel_id = format!("{:p}", &channel).bytes()
            .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));

        // Create mpsc channels for communication with the background task
        let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>(256);
        let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(256);
        let (stderr_tx, stderr_rx) = mpsc::channel::<Vec<u8>>(256);
        let (exit_tx, exit_rx) = mpsc::channel::<i32>(1);
        let (signal_tx, mut signal_rx) = mpsc::channel::<Sig>(16);

        // Spawn a single background task that owns the Channel and handles:
        // 1. Reading from channel.wait() → forward to stdout_tx
        // 2. Reading from stdin_rx → write to channel.data()
        // 3. Handling exit status → forward to exit_tx
        // 4. Handling signal_rx → send signal to remote process
        tokio::spawn(async move {
            let mut stdin_rx = stdin_rx;
            let mut channel = channel;

            loop {
                tokio::select! {
                    // Read from stdin channel and write to SSH channel
                    stdin_data = stdin_rx.recv() => {
                        match stdin_data {
                            Some(data) => {
                                if let Err(e) = channel.data(std::io::Cursor::new(data)).await {
                                    tracing::debug!(error = %e, "SSH stdin write error");
                                    break;
                                }
                            }
                            None => {
                                // stdin closed → send EOF to remote
                                let _ = channel.eof().await;
                                let _ = channel.close().await;
                                // Continue reading until channel closes
                            }
                        }
                    }

                    // Handle signal requests
                    signal = signal_rx.recv() => {
                        if let Some(sig) = signal {
                            if let Err(e) = channel.signal(sig).await {
                                tracing::debug!(error = %e, "SSH signal send error");
                            }
                        }
                    }

                    // Read from SSH channel and forward to stdout
                    msg = channel.wait() => {
                        match msg {
                            Some(ChannelMsg::Data { data, .. }) => {
                                if stdout_tx.send(data.to_vec()).await.is_err() {
                                    break;
                                }
                            }
                            // SSH stderr（remote 的 ExtendedData）：转发给独立 stderr 通道；
                            // 无消费者（claude 等不取 stderr 的适配器）时静默丢弃
                            Some(ChannelMsg::ExtendedData { data, .. }) => {
                                let _ = stderr_tx.send(data.to_vec()).await;
                            }
                            Some(ChannelMsg::ExitStatus { exit_status, .. }) => {
                                let _ = exit_tx.send(exit_status as i32).await;
                                break;
                            }
                            Some(ChannelMsg::ExitSignal { .. }) => {
                                let _ = exit_tx.send(-1).await;
                                break;
                            }
                            Some(ChannelMsg::Eof { .. }) | Some(ChannelMsg::Close { .. }) | None => {
                                let _ = exit_tx.send(0).await;
                                break;
                            }
                            _ => {
                                // Ignore other messages
                                continue;
                            }
                        }
                    }
                }
            }
        });

        Ok(Box::new(SshProcess {
            channel_id,
            stdin_tx: Some(stdin_tx),
            stdout_rx: Some(stdout_rx),
            stderr_rx: Some(stderr_rx),
            exit_rx: Some(exit_rx),
            signal_tx: Some(signal_tx),
        }))
    }
}

/// AsyncWrite adapter that forwards writes to an mpsc sender
struct SshStdinWriter {
    tx: mpsc::Sender<Vec<u8>>,
}

impl AsyncWrite for SshStdinWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.tx.try_send(buf.to_vec()) {
            Ok(()) => std::task::Poll::Ready(Ok(buf.len())),
            Err(mpsc::error::TrySendError::Full(_)) => {
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "stdin channel closed",
                )))
            }
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// AsyncRead adapter that reads from an mpsc receiver
struct SshStdoutReader {
    rx: mpsc::Receiver<Vec<u8>>,
    buffer: Vec<u8>,
    pos: usize,
}

impl AsyncRead for SshStdoutReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();

        // If we have buffered data, return it first
        if this.pos < this.buffer.len() {
            let remaining = &this.buffer[this.pos..];
            let to_copy = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..to_copy]);
            this.pos += to_copy;
            return std::task::Poll::Ready(Ok(()));
        }

        // Try to receive more data
        match this.rx.poll_recv(cx) {
            std::task::Poll::Ready(Some(data)) => {
                let to_copy = data.len().min(buf.remaining());
                buf.put_slice(&data[..to_copy]);
                if to_copy < data.len() {
                    this.buffer = data;
                    this.pos = to_copy;
                }
                std::task::Poll::Ready(Ok(()))
            }
            std::task::Poll::Ready(None) => {
                // Channel closed = EOF
                std::task::Poll::Ready(Ok(()))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// Escape a remote directory for `cd <dir>`: expand `~` to `$HOME` because
/// shell_escape's single quotes would suppress tilde expansion (SSH shells
/// don't expand `~` inside quotes).
pub fn escape_remote_dir(dir: &str) -> String {
    if dir == "~" {
        "\"$HOME\"".to_string()
    } else if let Some(rest) = dir.strip_prefix("~/") {
        format!("\"$HOME/{}\"", rest.replace('"', "\\\""))
    } else {
        shell_escape(dir).to_string()
    }
}

/// Simple shell escaping for SSH command construction
pub(crate) fn shell_escape(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    // If the string contains no special characters, return as-is
    if s.chars().all(|c| c.is_alphanumeric() || c == '/' || c == '.' || c == '-' || c == '_') {
        return s.to_string();
    }
    // Otherwise, wrap in single quotes and escape any single quotes
    format!("'{}'", s.replace('\'', "'\\''"))
}
