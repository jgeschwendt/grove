//! Chat application state and event handling

use chrono::{DateTime, Local};
use crossterm::event::{self, Event, KeyCode, KeyModifiers, MouseEventKind};
use ratatui::prelude::*;
use std::fs::OpenOptions;
use std::io::Write;
use tokio::sync::mpsc;
use tui_textarea::TextArea;

/// Log to TUI debug file
fn tui_log(msg: &str) {
    if let Some(home) = std::env::var("HOME").ok() {
        let log_path = format!("{}/.grove/data/tui.log", home);
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&log_path) {
            let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
            let _ = writeln!(file, "[{}] {}", timestamp, msg);
        }
    }
}

/// Available slash commands with descriptions
pub const COMMANDS: &[(&str, &str)] = &[
    ("/clone", "Clone a repository"),
    ("/list", "List repositories"),
    ("/harvest", "Export repos to seed file"),
    ("/grow", "Import repos from seed file"),
    ("/help", "Show available commands"),
    ("/exit", "Exit grove"),
];

/// Chat message
#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub content: String,
    pub timestamp: DateTime<Local>,
}

/// Message role
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
}

/// Input mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Insert,
    Normal,
}

/// Commands to execute
#[derive(Debug, Clone)]
pub enum Command {
    /// Clone a repository
    Clone(String),
    /// List repositories
    List,
    /// Export repositories to seed file
    Harvest(String),
    /// Import repositories from seed file
    Grow(String),
    /// Exit the application
    Quit,
}

/// Server status
#[derive(Debug, Clone)]
pub enum ServerStatus {
    Starting,
    Running { port: u16 },
    Error(String),
}

/// Chat application
pub struct ChatApp {
    /// Chat messages
    pub messages: Vec<Message>,
    /// Text input
    pub input: TextArea<'static>,
    /// Scroll offset (from bottom)
    pub scroll_offset: usize,
    /// Current mode
    pub mode: Mode,
    /// Server status
    pub server_status: ServerStatus,
    /// Server port
    pub port: u16,
    /// Command sender
    command_tx: mpsc::Sender<Command>,
    /// Autocomplete selection index
    pub autocomplete_index: usize,
    /// Locked autocomplete height (set when autocomplete opens)
    pub autocomplete_height: Option<usize>,
    /// Index of the update status message (to update in place)
    update_message_index: Option<usize>,
}

impl ChatApp {
    /// Create new chat app
    pub fn new(port: u16) -> (Self, mpsc::Receiver<Command>) {
        let (command_tx, command_rx) = mpsc::channel(32);

        let mut input = TextArea::default();
        input.set_cursor_line_style(Style::default());
        input.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));

        let app = Self {
            messages: vec![Message {
                role: Role::System,
                content: "Welcome to grove. Type /help for available commands.".to_string(),
                timestamp: Local::now(),
            }],
            input,
            scroll_offset: 0,
            mode: Mode::Insert,
            server_status: ServerStatus::Running { port },
            port,
            command_tx,
            autocomplete_index: 0,
            autocomplete_height: None,
            update_message_index: None,
        };

        (app, command_rx)
    }

    /// Run the TUI event loop
    pub async fn run(
        &mut self,
        terminal: &mut Terminal<impl Backend>,
        mut system_rx: mpsc::Receiver<String>,
    ) -> anyhow::Result<()> {
        // Enable mouse capture
        crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)?;

        let result = self.event_loop(terminal, &mut system_rx).await;

        // Disable mouse capture
        crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture)?;

        result
    }

    async fn event_loop(
        &mut self,
        terminal: &mut Terminal<impl Backend>,
        system_rx: &mut mpsc::Receiver<String>,
    ) -> anyhow::Result<()> {
        let mut event_stream = crossterm::event::EventStream::new();
        use futures::StreamExt;

        tui_log("Event loop started");
        let mut loop_count = 0u64;

        loop {
            loop_count += 1;
            if loop_count % 100 == 0 {
                tui_log(&format!("Loop iteration {}", loop_count));
            }

            // Draw UI
            tui_log("Drawing UI...");
            terminal.draw(|frame| crate::ui::render(frame, self))?;
            tui_log("UI drawn");

            // Wait for either terminal event or system message
            tui_log("Waiting for event...");
            tokio::select! {
                // Terminal events
                maybe_event = event_stream.next() => {
                    tui_log(&format!("Terminal event: {:?}", maybe_event.as_ref().map(|r| r.as_ref().map(|e| match e {
                        Event::Key(_) => "Key",
                        Event::Mouse(_) => "Mouse",
                        Event::Resize(_, _) => "Resize",
                        _ => "Other",
                    }))));
                    if let Some(Ok(event)) = maybe_event {
                        match event {
                            Event::Key(key) => {
                                tui_log(&format!("Key event: {:?}", key.code));
                                if self.handle_key(key).await? {
                                    tui_log("Quit signal received");
                                    break;
                                }
                            }
                            Event::Mouse(mouse) => {
                                self.handle_mouse(mouse);
                            }
                            _ => {}
                        }
                    } else if maybe_event.is_none() {
                        tui_log("Event stream ended!");
                    }
                }
                // System messages (from updater, commands, etc.)
                Some(msg) = system_rx.recv() => {
                    tui_log(&format!("System message received: {} chars", msg.len()));
                    self.handle_system_message(msg);
                    tui_log("System message handled");
                }
            }
            tui_log("Event processed, continuing loop");
        }
        tui_log("Event loop exited");
        Ok(())
    }

    /// Handle a system message (e.g., from updater)
    fn handle_system_message(&mut self, msg: String) {
        // Check if this is an update message that should replace the previous one
        if msg.starts_with("⟳") || msg.starts_with("✓") {
            if let Some(idx) = self.update_message_index {
                // Update existing message
                if idx < self.messages.len() {
                    self.messages[idx].content = msg;
                    self.messages[idx].timestamp = Local::now();
                }
            } else {
                // Add new message and track its index
                self.update_message_index = Some(self.messages.len());
                self.messages.push(Message {
                    role: Role::System,
                    content: msg,
                    timestamp: Local::now(),
                });
            }
        } else {
            // Regular system message
            self.messages.push(Message {
                role: Role::System,
                content: msg,
                timestamp: Local::now(),
            });
        }
        self.scroll_to_bottom();
    }

    /// Handle key event, returns true if should quit
    async fn handle_key(&mut self, key: event::KeyEvent) -> anyhow::Result<bool> {
        match self.mode {
            Mode::Insert => {
                let showing_autocomplete = self.show_autocomplete();
                let filtered_count = if showing_autocomplete {
                    self.filtered_commands().len()
                } else {
                    0
                };

                match (key.code, key.modifiers) {
                    // Tab to complete
                    (KeyCode::Tab, KeyModifiers::NONE) if showing_autocomplete && filtered_count > 0 => {
                        self.apply_autocomplete();
                    }
                    // Navigate autocomplete up
                    (KeyCode::Up, KeyModifiers::NONE) if showing_autocomplete && filtered_count > 0 => {
                        if self.autocomplete_index > 0 {
                            self.autocomplete_index -= 1;
                        } else {
                            self.autocomplete_index = filtered_count - 1;
                        }
                    }
                    // Navigate autocomplete down
                    (KeyCode::Down, KeyModifiers::NONE) if showing_autocomplete && filtered_count > 0 => {
                        if self.autocomplete_index < filtered_count - 1 {
                            self.autocomplete_index += 1;
                        } else {
                            self.autocomplete_index = 0;
                        }
                    }
                    // Submit
                    (KeyCode::Enter, KeyModifiers::NONE) => {
                        let content: String = self.input.lines().join("\n");
                        if !content.trim().is_empty() {
                            self.submit_message(content).await?;
                        }
                    }
                    // Multi-line
                    (KeyCode::Enter, KeyModifiers::SHIFT) => {
                        self.input.insert_newline();
                    }
                    // Clear or switch mode
                    (KeyCode::Esc, _) => {
                        if self.input.is_empty() {
                            self.mode = Mode::Normal;
                        } else {
                            self.input.select_all();
                            self.input.cut();
                        }
                        self.autocomplete_index = 0;
                        self.autocomplete_height = None;
                    }
                    // Quit
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        return Ok(true);
                    }
                    // Scroll (half page)
                    (KeyCode::Up, KeyModifiers::CONTROL) | (KeyCode::PageUp, _) => {
                        self.scroll_up(15);
                    }
                    (KeyCode::Down, KeyModifiers::CONTROL) | (KeyCode::PageDown, _) => {
                        self.scroll_down(15);
                    }
                    // Pass to textarea
                    _ => {
                        self.input.input(key);
                        // Reset autocomplete index when typing
                        self.autocomplete_index = 0;
                        // Update autocomplete state
                        self.update_autocomplete_state();
                    }
                }
            }
            Mode::Normal => match key.code {
                KeyCode::Char('i') => self.mode = Mode::Insert,
                KeyCode::Char('q') => return Ok(true),
                KeyCode::Char('j') => self.scroll_down(1),
                KeyCode::Char('k') => self.scroll_up(1),
                KeyCode::Char('G') => self.scroll_to_bottom(),
                KeyCode::Char('g') => self.scroll_to_top(),
                _ => {}
            },
        }
        Ok(false)
    }

    /// Submit a message
    async fn submit_message(&mut self, content: String) -> anyhow::Result<()> {
        // Add user message
        self.messages.push(Message {
            role: Role::User,
            content: content.clone(),
            timestamp: Local::now(),
        });

        // Clear input and reset autocomplete
        self.input.select_all();
        self.input.cut();
        self.autocomplete_index = 0;
        self.autocomplete_height = None;
        self.scroll_to_bottom();

        // Handle command or natural language
        if content.starts_with('/') {
            self.handle_command(&content).await?;
        } else {
            // For now, just echo back
            self.messages.push(Message {
                role: Role::System,
                content: "Natural language commands coming soon. Use /help for available commands."
                    .to_string(),
                timestamp: Local::now(),
            });
        }

        Ok(())
    }

    /// Handle slash command
    async fn handle_command(&mut self, input: &str) -> anyhow::Result<()> {
        let parts: Vec<&str> = input.split_whitespace().collect();
        let cmd = parts.first().map(|s| *s).unwrap_or("");

        tui_log(&format!("handle_command: input={:?}, cmd={:?}, parts={:?}", input, cmd, parts));

        match cmd {
            "/help" | "/?" => {
                self.messages.push(Message {
                    role: Role::System,
                    content: r#"Commands:
  /clone <url>           Clone a repository
  /list                  List repositories
  /harvest <file>        Export repos to seed file
  /grow <file>           Import repos from seed file
  /exit                  Exit grove

Navigation:
  Ctrl+↑/↓, PgUp/PgDn    Scroll
  Esc                    Clear / Normal mode
  Ctrl+C                 Quit"#
                        .to_string(),
                    timestamp: Local::now(),
                });
            }
            "/clone" => {
                if let Some(url) = parts.get(1) {
                    self.command_tx.send(Command::Clone(url.to_string())).await?;
                } else {
                    self.messages.push(Message {
                        role: Role::System,
                        content: "Usage: /clone <url>".to_string(),
                        timestamp: Local::now(),
                    });
                }
            }
            "/list" => {
                self.command_tx.send(Command::List).await?;
            }
            "/harvest" => {
                let file = parts.get(1).map(|s| s.to_string()).unwrap_or_else(|| "seed.jsonl".to_string());
                self.command_tx.send(Command::Harvest(file)).await?;
            }
            "/grow" => {
                if let Some(file) = parts.get(1) {
                    self.command_tx.send(Command::Grow(file.to_string())).await?;
                } else {
                    self.messages.push(Message {
                        role: Role::System,
                        content: "Usage: /grow <file>".to_string(),
                        timestamp: Local::now(),
                    });
                }
            }
            "/exit" => {
                self.command_tx.send(Command::Quit).await?;
            }
            _ => {
                tui_log(&format!("Unknown command branch hit: cmd={:?}", cmd));
                self.messages.push(Message {
                    role: Role::System,
                    content: format!("Unknown command: {}. Type /help for commands.", cmd),
                    timestamp: Local::now(),
                });
            }
        }

        Ok(())
    }

    /// Handle mouse event
    fn handle_mouse(&mut self, mouse: event::MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_up(2),
            MouseEventKind::ScrollDown => self.scroll_down(2),
            _ => {}
        }
    }

    fn scroll_up(&mut self, n: usize) {
        // Increase offset from bottom (shows older content)
        self.scroll_offset = self.scroll_offset.saturating_add(n);
    }

    fn scroll_down(&mut self, n: usize) {
        // Decrease offset from bottom (shows newer content)
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    fn scroll_to_top(&mut self) {
        // Large value will be clamped in render
        self.scroll_offset = usize::MAX / 2;
    }

    fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    /// Get current input text
    pub fn input_text(&self) -> String {
        self.input.lines().join("\n")
    }

    /// Check if autocomplete should be shown
    pub fn show_autocomplete(&self) -> bool {
        let text = self.input_text();
        text.starts_with('/') && !text.contains(' ') && !self.filtered_commands().is_empty()
    }

    /// Get filtered commands matching current input
    pub fn filtered_commands(&self) -> Vec<(&'static str, &'static str)> {
        let text = self.input_text();
        if !text.starts_with('/') {
            return vec![];
        }
        COMMANDS
            .iter()
            .filter(|(cmd, _)| cmd.starts_with(&text))
            .copied()
            .collect()
    }

    /// Apply autocomplete selection
    fn apply_autocomplete(&mut self) {
        let filtered = self.filtered_commands();
        if let Some((cmd, _)) = filtered.get(self.autocomplete_index) {
            self.input.select_all();
            self.input.cut();
            self.input.insert_str(cmd);
            self.input.insert_char(' ');
            self.autocomplete_index = 0;
            self.autocomplete_height = None; // Close autocomplete
        }
    }

    /// Update autocomplete height state
    pub fn update_autocomplete_state(&mut self) {
        if self.show_autocomplete() {
            // Lock height when first showing autocomplete
            if self.autocomplete_height.is_none() {
                let count = self.filtered_commands().len();
                self.autocomplete_height = Some(count.min(6));
            }
        } else {
            // Reset when autocomplete closes
            self.autocomplete_height = None;
        }
    }

    /// Get the display height for autocomplete
    pub fn get_autocomplete_display_height(&self) -> usize {
        self.autocomplete_height.unwrap_or(0)
    }
}
