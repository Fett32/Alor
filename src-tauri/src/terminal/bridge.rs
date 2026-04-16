/// Terminal bridge — PTY relay connecting xterm.js to alor-main.
///
/// Architecture:
///   Spawns `tmux attach -t alor-main` inside a real PTY.
///   Reads PTY output and emits it as Tauri events (incremental, live).
///   Writes xterm.js input to the PTY stdin.
///   Handles resize when xterm.js dimensions change.
///
///   No polling, no capture-pane. Real terminal data, real-time.

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::sync::Arc;
use tokio::sync::Mutex;

const MAIN_SESSION: &str = "alor-main";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A chunk of terminal output sent to the frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalChunk {
    pub data: String,
}

// ---------------------------------------------------------------------------
// Bridge
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct TerminalBridge {
    /// PTY writer — sends user input to tmux.
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    /// PTY master — kept alive so the PTY doesn't close.
    master: Arc<Mutex<Option<Box<dyn portable_pty::MasterPty + Send>>>>,
    /// Whether the relay is running.
    running: Arc<Mutex<bool>>,
}

impl Default for TerminalBridge {
    fn default() -> Self {
        Self {
            writer: Arc::new(Mutex::new(None)),
            master: Arc::new(Mutex::new(None)),
            running: Arc::new(Mutex::new(false)),
        }
    }
}

impl TerminalBridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start the PTY relay. Spawns `tmux attach` in a PTY and begins
    /// reading output. Returns a reader that the polling loop can use.
    ///
    /// Call this once at startup. The relay runs until the PTY closes.
    pub fn start_relay(&self) -> Result<Box<dyn Read + Send>> {
        let pty_system = native_pty_system();

        // Start with a reasonable default size; xterm.js will send the real
        // dimensions via terminal_resize once it measures its container.
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open PTY")?;

        let mut cmd = CommandBuilder::new("tmux");
        cmd.args(["attach", "-t", MAIN_SESSION]);
        // Clear TMUX env var to avoid "sessions should be nested" error.
        cmd.env("TMUX", "");
        // Tell tmux what terminal we're emulating so it sends correct escapes.
        cmd.env("TERM", "xterm-256color");

        let _child = pair
            .slave
            .spawn_command(cmd)
            .context("failed to spawn tmux attach")?;

        // Get reader and writer from the master side.
        let reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone PTY reader")?;

        let writer = pair
            .master
            .take_writer()
            .context("failed to take PTY writer")?;

        // Store writer and master (keep master alive).
        {
            let mut w = self.writer.blocking_lock();
            *w = Some(writer);
        }
        {
            let mut m = self.master.blocking_lock();
            *m = Some(pair.master);
        }
        {
            let mut r = self.running.blocking_lock();
            *r = true;
        }

        tracing::info!("PTY relay started for {MAIN_SESSION}");

        Ok(reader)
    }

    /// Send keystrokes from xterm.js to the PTY.
    pub async fn send_keys(&self, keys: &str) -> Result<()> {
        let mut writer_guard = self.writer.lock().await;
        if let Some(ref mut writer) = *writer_guard {
            writer
                .write_all(keys.as_bytes())
                .context("write to PTY failed")?;
            writer.flush().context("flush PTY failed")?;
            Ok(())
        } else {
            anyhow::bail!("PTY relay not started")
        }
    }

    /// Resize the PTY to match xterm.js dimensions.
    pub async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let master_guard = self.master.lock().await;
        if let Some(ref master) = *master_guard {
            master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("PTY resize failed")?;
            tracing::debug!(cols, rows, "PTY resized");
            Ok(())
        } else {
            anyhow::bail!("PTY relay not started")
        }
    }

    /// Check if the relay is running.
    pub async fn is_running(&self) -> bool {
        *self.running.lock().await
    }
}
