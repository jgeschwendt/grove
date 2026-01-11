//! grove CLI binary
//!
//! See README.md for command documentation and flow diagrams.

mod updater;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use grove_api::Server;
use grove_core::{Config, Database};
use grove_tui::{ChatApp, Command};
use std::process::Stdio;
use std::time::Duration;
use tokio::time::sleep;

/// grove - Git worktree dashboard
#[derive(Parser)]
#[command(name = "grove", version, about)]
struct Cli {
    /// Server port
    #[arg(short, long, default_value = "7777", env = "GROVE_PORT")]
    port: u16,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Clone a repository
    Clone {
        /// Repository URL (https or git@)
        url: String,
    },
    /// Create a new worktree
    Worktree {
        /// Repository ID or name
        repo: String,
        /// Branch name
        branch: String,
    },
    /// Delete a worktree
    Delete {
        /// Worktree path
        path: String,
    },
    /// Open worktree in editor
    Open {
        /// Path to open
        path: String,
    },
    /// List repositories
    List,
    /// Start server only (no TUI)
    Server,
    /// Show server status
    Status,
    /// Export repositories to seed.jsonl
    Harvest {
        /// Output file path
        file: String,
    },
    /// Import repositories from seed.jsonl
    Grow {
        /// Input file path
        file: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("grove=debug".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();

    // Initialize config and database
    let config = Config::from_env();
    let db = Database::open(&config)?;

    match cli.command {
        // No subcommand → launch interactive TUI
        None => {
            // Check for updates in background
            let (applied, update_rx) = updater::check_for_updates_background();

            // Ensure server is running
            let port = ensure_server_running(cli.port, &config, &db).await?;

            // Launch TUI with update channel
            run_tui(port, applied, update_rx).await?;
        }

        // Single commands
        Some(Commands::Clone { url }) => {
            let port = ensure_server_running(cli.port, &config, &db).await?;
            clone_repository(port, &url).await?;
        }

        Some(Commands::Worktree { repo, branch }) => {
            let port = ensure_server_running(cli.port, &config, &db).await?;
            create_worktree(port, &repo, &branch).await?;
        }

        Some(Commands::Delete { path }) => {
            let port = ensure_server_running(cli.port, &config, &db).await?;
            delete_worktree(port, &path).await?;
        }

        Some(Commands::Open { path }) => {
            open_in_editor(&path)?;
        }

        Some(Commands::List) => {
            list_repositories(&db)?;
        }

        Some(Commands::Server) => {
            // Check for updates in background (ignore receiver for headless mode)
            let (applied, _) = updater::check_for_updates_background();
            if applied {
                eprintln!("\x1b[32minfo\x1b[0m: grove updated to latest version!");
            }

            // Run server in foreground
            run_server(cli.port, config, db).await?;
        }

        Some(Commands::Status) => {
            check_status(cli.port)?;
        }

        Some(Commands::Harvest { file }) => {
            harvest_repositories(&db, &file)?;
        }

        Some(Commands::Grow { file }) => {
            let port = ensure_server_running(cli.port, &config, &db).await?;
            grow_repositories(port, &file).await?;
        }
    }

    Ok(())
}

/// Ensure server is running, start if needed
async fn ensure_server_running(port: u16, _config: &Config, _db: &Database) -> Result<u16> {
    // Check if port is already in use (server running)
    if is_server_running(port) {
        eprintln!("Server already running on port {}", port);
        return Ok(port);
    }

    // Spawn server as background daemon
    eprintln!("Starting server on port {}...", port);

    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .args(["--port", &port.to_string(), "server"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Failed to spawn server")?;

    // Wait for server to be ready
    for _ in 0..50 {
        sleep(Duration::from_millis(100)).await;
        if is_server_running(port) {
            return Ok(port);
        }
    }

    anyhow::bail!("Server failed to start within 5 seconds")
}

/// Check if server is running by trying to connect
fn is_server_running(port: u16) -> bool {
    std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
}

/// Run the TUI chat interface
async fn run_tui(
    port: u16,
    update_applied: bool,
    mut update_rx: tokio::sync::mpsc::Receiver<updater::UpdateStatus>,
) -> Result<()> {
    // Setup terminal
    let mut terminal = ratatui::init();
    terminal.clear()?;

    // Create app
    let (mut app, mut command_rx) = ChatApp::new(port);

    // Create channel for system messages to the TUI
    let (system_tx, system_rx) = tokio::sync::mpsc::channel::<String>(16);

    // If update was just applied, send message
    if update_applied {
        let _ = system_tx
            .send(format!("✓ Updated to v{}", updater::current_version()))
            .await;
    }

    // Spawn task to convert UpdateStatus to system messages
    let system_tx_clone = system_tx.clone();
    tokio::spawn(async move {
        while let Some(status) = update_rx.recv().await {
            let msg = match status {
                updater::UpdateStatus::Checking => continue, // Don't show checking message
                updater::UpdateStatus::Downloading(v) => format!("⟳ Downloading {}...", v),
                updater::UpdateStatus::Ready(v) => {
                    format!("✓ {} ready — restart to update", v)
                }
                updater::UpdateStatus::UpToDate => continue, // Don't show up-to-date
                updater::UpdateStatus::Applied(v) => format!("✓ Updated to {}", v),
            };
            let _ = system_tx_clone.send(msg).await;
        }
    });

    // Spawn command handler
    let system_tx_cmd = system_tx.clone();
    let handle = tokio::spawn(async move {
        let client = reqwest::Client::new();
        let base_url = format!("http://localhost:{}", port);

        while let Some(cmd) = command_rx.recv().await {
            match cmd {
                Command::Quit => break,
                Command::Clone(url) => {
                    let _ = system_tx_cmd.send(format!("Cloning {}...", url)).await;
                    match client
                        .post(format!("{}/api/clone", base_url))
                        .json(&serde_json::json!({ "url": url }))
                        .send()
                        .await
                    {
                        Ok(resp) if resp.status().is_success() => {
                            let _ = system_tx_cmd.send("Clone started".to_string()).await;
                        }
                        Ok(resp) => {
                            let error = resp.text().await.unwrap_or_default();
                            let _ = system_tx_cmd.send(format!("Clone failed: {}", error)).await;
                        }
                        Err(e) => {
                            let _ = system_tx_cmd.send(format!("Clone error: {}", e)).await;
                        }
                    }
                }
                Command::List => {
                    match client.get(format!("{}/api/state/snapshot", base_url)).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            if let Ok(state) = resp.json::<serde_json::Value>().await {
                                let mut output = String::new();
                                if let Some(repos) = state.get("repositories").and_then(|v| v.as_array()) {
                                    if repos.is_empty() {
                                        output.push_str("No repositories. Use /clone <url> to add one.");
                                    } else {
                                        for repo in repos {
                                            let name = repo.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                                            output.push_str(&format!("{}\n", name));
                                            if let Some(worktrees) = repo.get("worktrees").and_then(|v| v.as_array()) {
                                                for wt in worktrees {
                                                    let branch = wt.get("branch").and_then(|v| v.as_str()).unwrap_or("");
                                                    let ahead = wt.get("ahead").and_then(|v| v.as_i64()).unwrap_or(0);
                                                    let behind = wt.get("behind").and_then(|v| v.as_i64()).unwrap_or(0);
                                                    let dirty = wt.get("dirty").and_then(|v| v.as_bool()).unwrap_or(false);
                                                    let marker = if dirty { "○" } else { "●" };
                                                    output.push_str(&format!("  {} {} (+{},-{})\n", marker, branch, ahead, behind));
                                                }
                                            }
                                        }
                                    }
                                }
                                let _ = system_tx_cmd.send(output.trim_end().to_string()).await;
                            }
                        }
                        _ => {
                            let _ = system_tx_cmd.send("Failed to list repositories".to_string()).await;
                        }
                    }
                }
                Command::Harvest(file) => {
                    let _ = system_tx_cmd.send(format!("Exporting to {}...", file)).await;
                    match client.get(format!("{}/api/state/snapshot", base_url)).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            if let Ok(state) = resp.json::<serde_json::Value>().await {
                                let mut lines = Vec::new();
                                if let Some(repos) = state.get("repositories").and_then(|v| v.as_array()) {
                                    for repo in repos {
                                        let url = repo.get("clone_url").and_then(|v| v.as_str()).unwrap_or("");
                                        let branches: Vec<String> = repo
                                            .get("worktrees")
                                            .and_then(|v| v.as_array())
                                            .map(|wts| {
                                                wts.iter()
                                                    .filter_map(|wt| {
                                                        let path = wt.get("path").and_then(|v| v.as_str())?;
                                                        if path.ends_with("/.main") {
                                                            None
                                                        } else {
                                                            wt.get("branch").and_then(|v| v.as_str()).map(String::from)
                                                        }
                                                    })
                                                    .collect()
                                            })
                                            .unwrap_or_default();
                                        let entry = serde_json::json!({
                                            "url": url,
                                            "worktrees": branches
                                        });
                                        lines.push(serde_json::to_string(&entry).unwrap_or_default());
                                    }
                                    match std::fs::write(&file, lines.join("\n") + "\n") {
                                        Ok(_) => {
                                            let _ = system_tx_cmd.send(format!("Exported {} repos to {}", lines.len(), file)).await;
                                        }
                                        Err(e) => {
                                            let _ = system_tx_cmd.send(format!("Failed to write file: {}", e)).await;
                                        }
                                    }
                                }
                            }
                        }
                        _ => {
                            let _ = system_tx_cmd.send("Failed to export repositories".to_string()).await;
                        }
                    }
                }
                Command::Grow(file) => {
                    let _ = system_tx_cmd.send(format!("Importing from {}...", file)).await;
                    match std::fs::read_to_string(&file) {
                        Ok(content) => {
                            let entries: Vec<serde_json::Value> = content
                                .lines()
                                .filter(|line| !line.trim().is_empty())
                                .filter_map(|line| serde_json::from_str(line).ok())
                                .collect();

                            if entries.is_empty() {
                                let _ = system_tx_cmd.send("No entries in seed file".to_string()).await;
                                continue;
                            }

                            let _ = system_tx_cmd.send(format!("Importing {} repositories...", entries.len())).await;

                            for entry in &entries {
                                if let Some(url) = entry.get("url").and_then(|v| v.as_str()) {
                                    let _ = system_tx_cmd.send(format!("Cloning {}...", url)).await;
                                    let _ = client
                                        .post(format!("{}/api/clone", base_url))
                                        .json(&serde_json::json!({ "url": url }))
                                        .send()
                                        .await;
                                }
                            }

                            let _ = system_tx_cmd.send(format!("Started {} clones", entries.len())).await;
                        }
                        Err(e) => {
                            let _ = system_tx_cmd.send(format!("Failed to read file: {}", e)).await;
                        }
                    }
                }
            }
        }
    });

    // Run event loop
    let result = app.run(&mut terminal, system_rx).await;

    // Restore terminal
    ratatui::restore();

    // Wait for command handler
    handle.abort();

    result
}

/// Run the HTTP server
async fn run_server(port: u16, config: Config, db: Database) -> Result<()> {
    let server = Server::new(config, db);
    server.run(port).await
}

/// Clone a repository via API
async fn clone_repository(port: u16, url: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://localhost:{}/api/clone", port))
        .json(&serde_json::json!({ "url": url }))
        .send()
        .await?;

    if resp.status().is_success() {
        println!("Clone started: {}", url);
    } else {
        let error: serde_json::Value = resp.json().await?;
        eprintln!("Error: {}", error);
    }

    Ok(())
}

/// Create a worktree via API
async fn create_worktree(port: u16, repo: &str, branch: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://localhost:{}/api/worktree", port))
        .json(&serde_json::json!({ "repo_id": repo, "branch": branch }))
        .send()
        .await?;

    if resp.status().is_success() {
        println!("Worktree created: {}", branch);
    } else {
        let error: serde_json::Value = resp.json().await?;
        eprintln!("Error: {}", error);
    }

    Ok(())
}

/// Delete a worktree via API
async fn delete_worktree(port: u16, path: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let resp = client
        .delete(format!("http://localhost:{}/api/worktree/{}", port, path))
        .send()
        .await?;

    if resp.status().is_success() {
        println!("Worktree deleted: {}", path);
    } else {
        let error: serde_json::Value = resp.json().await?;
        eprintln!("Error: {}", error);
    }

    Ok(())
}

/// Open path in VS Code
fn open_in_editor(path: &str) -> Result<()> {
    std::process::Command::new("code")
        .arg(path)
        .spawn()
        .context("Failed to open VS Code")?;
    println!("Opened: {}", path);
    Ok(())
}

/// List repositories from database
fn list_repositories(db: &Database) -> Result<()> {
    let repos = db.list_repositories()?;

    if repos.is_empty() {
        println!("No repositories. Use `grove clone <url>` to add one.");
        return Ok(());
    }

    for repo in repos {
        println!("{} - {}", repo.name, repo.clone_url);
        let worktrees = db.list_worktrees(&repo.id)?;
        for (i, wt) in worktrees.iter().enumerate() {
            let marker = if i == 0 { "●" } else { "○" };
            println!("  {} {} ({})", marker, wt.branch, wt.path);
        }
    }

    Ok(())
}

/// Check server status
fn check_status(port: u16) -> Result<()> {
    if is_server_running(port) {
        println!("Server running on http://localhost:{}", port);
    } else {
        println!("Server not running");
    }
    Ok(())
}

/// Seed entry for harvest/grow
#[derive(serde::Serialize, serde::Deserialize)]
struct SeedEntry {
    url: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    worktrees: Vec<String>,
}

/// Export repositories to seed.jsonl
fn harvest_repositories(db: &Database, file: &str) -> Result<()> {
    let repos = db.list_repositories()?;

    if repos.is_empty() {
        println!("No repositories to export.");
        return Ok(());
    }

    let mut lines = Vec::new();
    for repo in &repos {
        let worktrees = db.list_worktrees(&repo.id)?;
        // Exclude .main worktree, only include additional branches
        let branches: Vec<String> = worktrees
            .iter()
            .filter(|wt| !wt.path.ends_with("/.main"))
            .map(|wt| wt.branch.clone())
            .collect();

        let entry = SeedEntry {
            url: repo.clone_url.clone(),
            worktrees: branches,
        };
        lines.push(serde_json::to_string(&entry)?);
    }

    std::fs::write(file, lines.join("\n") + "\n")?;
    println!("Exported {} repositories to {}", repos.len(), file);

    Ok(())
}

/// Wait for a repository's main worktree to be ready via SSE stream
async fn wait_for_worktree_ready(
    client: &reqwest::Client,
    base_url: &str,
    clone_url: &str,
) -> Result<Option<String>> {
    use futures_util::StreamExt;
    use tokio::time::timeout;

    let response = client
        .get(format!("{}/api/state", base_url))
        .header("Accept", "text/event-stream")
        .send()
        .await?;

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();

    // 3 minute timeout for clone
    let result = timeout(Duration::from_secs(180), async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete SSE events
            while let Some(pos) = buffer.find("\n\n") {
                let event = buffer[..pos].to_string();
                buffer = buffer[pos + 2..].to_string();

                // Parse SSE data line
                if let Some(data) = event.strip_prefix("data: ") {
                    if let Ok(state) = serde_json::from_str::<serde_json::Value>(data) {
                        if let Some(repos) = state.get("repositories").and_then(|v| v.as_array()) {
                            if let Some(repo) = repos.iter().find(|r| {
                                r.get("clone_url")
                                    .and_then(|v| v.as_str())
                                    .map(|u| u == clone_url)
                                    .unwrap_or(false)
                            }) {
                                // Check if main worktree exists and is ready
                                let has_ready_main = repo
                                    .get("worktrees")
                                    .and_then(|v| v.as_array())
                                    .map(|wts| {
                                        wts.iter().any(|wt| {
                                            wt.get("path")
                                                .and_then(|v| v.as_str())
                                                .map(|p| p.ends_with("/.main"))
                                                .unwrap_or(false)
                                                && wt
                                                    .get("status")
                                                    .and_then(|v| v.as_str())
                                                    .map(|s| s == "ready")
                                                    .unwrap_or(false)
                                        })
                                    })
                                    .unwrap_or(false);

                                if has_ready_main {
                                    return Ok::<Option<String>, anyhow::Error>(
                                        repo.get("id").and_then(|v| v.as_str()).map(String::from),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(None)
    })
    .await;

    match result {
        Ok(r) => r,
        Err(_) => {
            eprintln!("  ✗ Timeout waiting for clone (180s)");
            Ok(None)
        }
    }
}

/// Import repositories from seed.jsonl
async fn grow_repositories(port: u16, file: &str) -> Result<()> {
    let content = std::fs::read_to_string(file).context("Failed to read seed file")?;
    let entries: Vec<SeedEntry> = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line))
        .collect::<Result<Vec<_>, _>>()
        .context("Failed to parse seed file")?;

    if entries.is_empty() {
        println!("No entries in seed file.");
        return Ok(());
    }

    println!("Importing {} repositories via server API\n", entries.len());

    let client = reqwest::Client::new();
    let base_url = format!("http://localhost:{}", port);

    for (i, entry) in entries.iter().enumerate() {
        println!("[{}/{}] Cloning {}...", i + 1, entries.len(), entry.url);

        // Clone repository
        let resp = client
            .post(format!("{}/api/clone", base_url))
            .json(&serde_json::json!({ "url": entry.url }))
            .send()
            .await?;

        if !resp.status().is_success() {
            println!("  ✗ Clone request failed (HTTP error)");
            continue;
        }

        // Check response body for ok field
        let body: serde_json::Value = resp.json().await?;
        if body.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let error = body.get("error").and_then(|v| v.as_str()).unwrap_or("unknown");
            println!("  ✗ Clone failed: {}", error);
            continue;
        }

        println!("  ✓ Clone started (watch UI for progress)");

        // If worktrees specified, wait for clone to complete then create them
        if !entry.worktrees.is_empty() {
            println!("  Waiting for clone to complete...");

            // Stream SSE until main worktree is ready
            let repo_id = wait_for_worktree_ready(&client, &base_url, &entry.url).await?;

            match repo_id {
                Some(id) => {
                    println!("  ✓ Clone complete");

                    // Create worktrees
                    for branch in &entry.worktrees {
                        println!("  Creating worktree: {}...", branch);

                        let resp = client
                            .post(format!("{}/api/worktree", base_url))
                            .json(&serde_json::json!({ "repo_id": id, "branch": branch }))
                            .send()
                            .await?;

                        if resp.status().is_success() {
                            println!("    ✓ Started (watch UI for progress)");
                        } else {
                            println!("    ✗ Failed to create worktree");
                        }
                    }
                }
                None => {
                    println!("  ✗ Timeout waiting for clone");
                }
            }
        }
    }

    println!("\nDone. Watch the UI for real-time progress.");
    Ok(())
}
