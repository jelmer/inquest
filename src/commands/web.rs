//! `inq web` command: thin CLI wrapper over the [`crate::web`] HTTP server.
//!
//! This file owns just the command surface: parsing CLI args, validating the
//! repository up front, printing the listening URL, and (optionally) opening
//! the browser. Everything else — HTTP handlers, state, SSE plumbing, child
//! process management — lives under [`crate::web`].

use crate::commands::Command;
use crate::error::{Error, Result};
use crate::ui::UI;
use std::path::PathBuf;

/// Command to start the web UI server.
pub struct WebCommand {
    base_path: Option<String>,
    bind: String,
    port: u16,
    open: bool,
}

impl WebCommand {
    /// Create a new web command.
    pub fn new(base_path: Option<String>, bind: String, port: u16, open: bool) -> Self {
        WebCommand {
            base_path,
            bind,
            port,
            open,
        }
    }
}

impl Command for WebCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        // Validate the repository up front so we fail fast with a clear error
        // before we bring up the listener — once the server is running, errors
        // would surface as HTTP 500s instead.
        let _ = super::utils::open_repository(self.base_path.as_deref())?;

        let base = self.base_path.clone().unwrap_or_else(|| ".".to_string());
        let base = std::path::Path::new(&base)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(&base));

        let addr = format!("{}:{}", self.bind, self.port);
        let url = format!("http://{}/", addr);

        ui.output(&format!("inq web listening on {}", url))?;
        ui.output("Press Ctrl-C to stop.")?;

        if self.open {
            // Best-effort browser launch. Failure is intentionally silent —
            // the URL is already printed above so the user can open it
            // manually if `xdg-open`/`open` isn't available.
            let _ = open_browser(&url);
        }

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::Other(format!("Failed to start tokio runtime: {}", e)))?;

        runtime.block_on(crate::web::serve(base, addr))?;
        Ok(0)
    }

    fn name(&self) -> &str {
        "web"
    }

    fn help(&self) -> &str {
        "Start a web UI for browsing tests and runs"
    }
}

#[cfg(target_os = "linux")]
fn open_browser(url: &str) -> std::io::Result<()> {
    std::process::Command::new("xdg-open").arg(url).spawn()?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_browser(url: &str) -> std::io::Result<()> {
    std::process::Command::new("open").arg(url).spawn()?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_browser(url: &str) -> std::io::Result<()> {
    std::process::Command::new("cmd")
        .args(["/C", "start", url])
        .spawn()?;
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn open_browser(_url: &str) -> std::io::Result<()> {
    Ok(())
}
