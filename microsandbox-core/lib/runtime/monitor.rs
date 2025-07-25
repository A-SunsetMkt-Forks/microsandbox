use std::{
    io::{Read, Write},
    os::fd::BorrowedFd,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use microsandbox_utils::{
    ChildIo, MicrosandboxUtilsError, MicrosandboxUtilsResult, ProcessMonitor, RotatingLog,
    LOG_SUFFIX,
};
use sqlx::{Pool, Sqlite};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{management::db, vm::Rootfs, MicrosandboxResult};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// The status of a sandbox when it is running
pub const SANDBOX_STATUS_RUNNING: &str = "RUNNING";

/// The status of a sandbox when it is stopped
pub const SANDBOX_STATUS_STOPPED: &str = "STOPPED";

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// A process monitor for MicroVMs
pub struct MicroVmMonitor {
    /// The database for tracking sandbox metrics and metadata
    sandbox_db: Pool<Sqlite>,

    /// The name of the sandbox
    sandbox_name: String,

    /// The config file for the sandbox
    config_file: String,

    /// The last modified timestamp of the config file
    config_last_modified: DateTime<Utc>,

    /// The supervisor PID
    supervisor_pid: u32,

    /// The MicroVM log path
    log_path: Option<PathBuf>,

    /// The log directory
    log_dir: PathBuf,

    /// The root filesystem
    rootfs: Rootfs,

    /// original terminal settings for STDIN (set in TTY mode)
    original_term: Option<nix::sys::termios::Termios>,

    /// Whether to forward output to stdout/stderr
    forward_output: bool,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl MicroVmMonitor {
    /// Create a new MicroVM monitor
    pub async fn new(
        supervisor_pid: u32,
        sandbox_db_path: impl AsRef<Path>,
        sandbox_name: String,
        config_file: String,
        config_last_modified: DateTime<Utc>,
        log_dir: impl Into<PathBuf>,
        rootfs: Rootfs,
        forward_output: bool,
    ) -> MicrosandboxResult<Self> {
        Ok(Self {
            supervisor_pid,
            sandbox_db: db::get_pool(sandbox_db_path.as_ref()).await?,
            sandbox_name,
            config_file,
            config_last_modified,
            log_path: None,
            log_dir: log_dir.into(),
            rootfs,
            original_term: None,
            forward_output,
        })
    }

    fn restore_terminal_settings(&mut self) {
        if let Some(original_term) = self.original_term.take() {
            if let Err(e) = nix::sys::termios::tcsetattr(
                unsafe { BorrowedFd::borrow_raw(libc::STDIN_FILENO) },
                nix::sys::termios::SetArg::TCSANOW,
                &original_term,
            ) {
                tracing::warn!(error = %e, "failed to restore terminal settings in restore_terminal_settings");
            }
        }
    }

    /// Generate a hierarchical log path with the format: <log_dir>/<config_file>/<sandbox_name>.<LOG_SUFFIX>
    /// This creates a directory structure that namespaces logs by config file and sandbox name.
    fn generate_log_path(&self) -> PathBuf {
        // Create a directory for the config file
        let config_dir = self.log_dir.join(&self.config_file);
        // Place the log file inside that directory with the sandbox name
        config_dir.join(format!("{}.{}", self.sandbox_name, LOG_SUFFIX))
    }
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

#[async_trait]
impl ProcessMonitor for MicroVmMonitor {
    async fn start(&mut self, pid: u32, child_io: ChildIo) -> MicrosandboxUtilsResult<()> {
        // Generate the log path with directory-level separation
        let log_path = self.generate_log_path();

        // Ensure the parent directory exists
        if let Some(parent) = log_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let microvm_log =
            std::sync::Arc::new(tokio::sync::Mutex::new(RotatingLog::new(&log_path).await?));
        let microvm_pid = pid;

        self.log_path = Some(log_path);

        // Get rootfs paths
        let rootfs_paths = match &self.rootfs {
            Rootfs::Native(path) => format!("native:{}", path.to_string_lossy().into_owned()),
            Rootfs::Overlayfs(paths) => format!(
                "overlayfs:{}",
                paths
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect::<Vec<String>>()
                    .join(":")
            ),
        };

        // Insert sandbox entry into database
        db::save_or_update_sandbox(
            &self.sandbox_db,
            &self.sandbox_name,
            &self.config_file,
            &self.config_last_modified,
            SANDBOX_STATUS_RUNNING,
            self.supervisor_pid,
            microvm_pid,
            &rootfs_paths,
        )
        .await
        .map_err(MicrosandboxUtilsError::custom)?;

        match child_io {
            ChildIo::Piped {
                stdin,
                stdout,
                stderr,
            } => {
                // Handle stdout logging
                if let Some(mut stdout) = stdout {
                    let log = microvm_log.clone();
                    let forward_output = self.forward_output;
                    tokio::spawn(async move {
                        let mut buf = [0u8; 8192]; // NOTE(appcypher): Using 8192 as buffer size because ChatGPT recommended it lol
                        while let Ok(n) = stdout.read(&mut buf).await {
                            if n == 0 {
                                break;
                            }
                            // Write to log file
                            let mut log_guard = log.lock().await;
                            if let Err(e) = log_guard.write_all(&buf[..n]).await {
                                tracing::error!(microvm_pid = microvm_pid, error = %e, "failed to write to microvm stdout log");
                            }
                            if let Err(e) = log_guard.flush().await {
                                tracing::error!(microvm_pid = microvm_pid, error = %e, "failed to flush microvm stdout log");
                            }

                            // Also forward to parent's stdout if enabled
                            if forward_output {
                                print!("{}", String::from_utf8_lossy(&buf[..n]));
                                // Flush stdout in case data is buffered
                                if let Err(e) = std::io::stdout().flush() {
                                    tracing::warn!(error = %e, "failed to flush parent stdout");
                                }
                            }
                        }
                    });
                }

                // Handle stderr logging
                if let Some(mut stderr) = stderr {
                    let log = microvm_log.clone();
                    let forward_output = self.forward_output;
                    tokio::spawn(async move {
                        let mut buf = [0u8; 8192]; // NOTE(appcypher): Using 8192 as buffer size because ChatGPT recommended it lol
                        while let Ok(n) = stderr.read(&mut buf).await {
                            if n == 0 {
                                break;
                            }
                            // Write to log file
                            let mut log_guard = log.lock().await;
                            if let Err(e) = log_guard.write_all(&buf[..n]).await {
                                tracing::error!(microvm_pid = microvm_pid, error = %e, "failed to write to microvm stderr log");
                            }
                            if let Err(e) = log_guard.flush().await {
                                tracing::error!(microvm_pid = microvm_pid, error = %e, "failed to flush microvm stderr log");
                            }

                            // Also forward to parent's stderr if enabled
                            if forward_output {
                                eprint!("{}", String::from_utf8_lossy(&buf[..n]));
                                // Flush stderr in case data is buffered
                                if let Err(e) = std::io::stderr().flush() {
                                    tracing::warn!(error = %e, "failed to flush parent stderr");
                                }
                            }
                        }
                    });
                }

                // Handle stdin streaming from parent to child
                if let Some(mut child_stdin) = stdin {
                    tokio::spawn(async move {
                        let mut parent_stdin = tokio::io::stdin();
                        if let Err(e) = tokio::io::copy(&mut parent_stdin, &mut child_stdin).await {
                            tracing::warn!(error = %e, "failed to copy parent stdin to child stdin");
                        }
                    });
                }
            }
            ChildIo::TTY {
                master_read,
                mut master_write,
            } => {
                // Handle TTY I/O
                // Put terminal in raw mode
                let term = nix::sys::termios::tcgetattr(unsafe {
                    BorrowedFd::borrow_raw(libc::STDIN_FILENO)
                })?;
                self.original_term = Some(term.clone());
                let mut raw_term = term.clone();
                nix::sys::termios::cfmakeraw(&mut raw_term);
                nix::sys::termios::tcsetattr(
                    unsafe { BorrowedFd::borrow_raw(libc::STDIN_FILENO) },
                    nix::sys::termios::SetArg::TCSANOW,
                    &raw_term,
                )?;

                // Spawn async task to read from the master
                let log = microvm_log.clone();
                let forward_output = self.forward_output;
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    loop {
                        let mut read_guard = match master_read.readable().await {
                            Ok(guard) => guard,
                            Err(e) => {
                                tracing::warn!(error = %e, "error waiting for master fd to become readable");
                                break;
                            }
                        };

                        match read_guard.try_io(|inner| inner.get_ref().read(&mut buf)) {
                            Ok(Ok(0)) => break, // EOF reached.
                            Ok(Ok(n)) => {
                                // Write to log file
                                let mut log_guard = log.lock().await;
                                if let Err(e) = log_guard.write_all(&buf[..n]).await {
                                    tracing::error!(microvm_pid = microvm_pid, error = %e, "failed to write to microvm tty log");
                                }
                                if let Err(e) = log_guard.flush().await {
                                    tracing::error!(microvm_pid = microvm_pid, error = %e, "failed to flush microvm tty log");
                                }

                                // Print the output from the child process if enabled
                                if forward_output {
                                    print!("{}", String::from_utf8_lossy(&buf[..n]));
                                    // flush stdout in case data is buffered
                                    std::io::stdout().flush().ok();
                                }
                            }
                            Ok(Err(e)) => {
                                tracing::warn!(error = %e, "error reading from master fd");
                                break;
                            }
                            Err(_) => continue,
                        }
                    }
                });

                // Spawn async task to copy parent's stdin to the master
                tokio::spawn(async move {
                    let mut stdin = tokio::io::stdin();
                    if let Err(e) = tokio::io::copy(&mut stdin, &mut master_write).await {
                        tracing::warn!(error = %e, "error copying stdin to master fd");
                    }
                });
            }
        }

        Ok(())
    }

    async fn stop(&mut self) -> MicrosandboxUtilsResult<()> {
        // Restore terminal settings if they were modified
        self.restore_terminal_settings();

        // Update sandbox status to stopped
        db::update_sandbox_status(
            &self.sandbox_db,
            &self.sandbox_name,
            &self.config_file,
            SANDBOX_STATUS_STOPPED,
        )
        .await
        .map_err(MicrosandboxUtilsError::custom)?;

        // Reset the log path
        self.log_path = None;

        Ok(())
    }
}

impl Drop for MicroVmMonitor {
    fn drop(&mut self) {
        self.restore_terminal_settings();
    }
}
