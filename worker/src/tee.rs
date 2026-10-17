use nix::unistd::{dup, dup2};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{BorrowedFd, FromRawFd, OwnedFd};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

/// Only one `CopyOutput` may hold the process-wide descriptors at a time.
/// Two overlapping instances leave stdout pointing at the wrong place, and
/// in tee mode the process then waits on a `tee` that cannot see end of file.
static REDIRECT_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Safe wrapper for capturing stdout/stderr to a file
///
/// This struct redirects stdout and stderr to either a file directly
/// or through the `tee` command for simultaneous console and file output.
/// File descriptors are properly managed to prevent leaks.
#[derive(Debug)]
pub struct CopyOutput {
    old_stdout: Option<OwnedFd>,
    old_stderr: Option<OwnedFd>,
    tee: bool,
    process: Option<std::process::Child>,
    newfd: Option<File>,
    holds_guard: bool,
}

impl CopyOutput {
    /// Create a new CopyOutput that redirects stdout/stderr to the specified file
    ///
    /// # Arguments
    /// * `output_log` - Path to the output log file
    /// * `tee` - If true, use `tee` command to show output on console and write to file
    ///
    /// # Safety
    /// This function manipulates file descriptors. If the process panics or exits
    /// unexpectedly, the original stdout/stderr may not be restored.
    pub fn new(output_log: &std::path::Path, tee: bool) -> io::Result<Self> {
        // Validate the output path
        if let Some(parent) = output_log.parent() {
            if !parent.exists() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Parent directory does not exist: {}", parent.display()),
                ));
            }
        }

        // Safely duplicate file descriptors
        let old_stdout =
            dup(unsafe { BorrowedFd::borrow_raw(nix::libc::STDOUT_FILENO) }).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("Failed to duplicate stdout: {}", e),
                )
            })?;

        let old_stderr =
            dup(unsafe { BorrowedFd::borrow_raw(nix::libc::STDERR_FILENO) }).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("Failed to duplicate stderr: {}", e),
                )
            })?;

        if REDIRECT_ACTIVE
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "stdout and stderr are already redirected",
            ));
        }

        let mut copy_output = Self {
            old_stdout: Some(old_stdout),
            old_stderr: Some(old_stderr),
            tee,
            process: None,
            newfd: None,
            holds_guard: true,
        };

        // Set up redirection
        if tee {
            copy_output.setup_tee_redirection(output_log)?;
        } else {
            copy_output.setup_file_redirection(output_log)?;
        }

        Ok(copy_output)
    }

    /// Set up redirection through the `tee` command
    fn setup_tee_redirection(&mut self, output_log: &std::path::Path) -> io::Result<()> {
        let process = Command::new("tee")
            .arg(output_log)
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("Failed to spawn tee process: {}", e),
                )
            })?;

        let stdin_ref = process
            .stdin
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Tee process has no stdin"))?;

        // Redirect stdout and stderr to tee's stdin
        let mut stdout_fd = unsafe { OwnedFd::from_raw_fd(nix::libc::STDOUT_FILENO) };
        let mut stderr_fd = unsafe { OwnedFd::from_raw_fd(nix::libc::STDERR_FILENO) };
        dup2(stdin_ref, &mut stdout_fd).map_err(|e| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Failed to redirect stdout to tee: {}", e),
            )
        })?;
        dup2(stdin_ref, &mut stderr_fd).map_err(|e| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Failed to redirect stderr to tee: {}", e),
            )
        })?;
        std::mem::forget(stdout_fd);
        std::mem::forget(stderr_fd);

        self.process = Some(process);
        Ok(())
    }

    /// Set up direct redirection to a file
    fn setup_file_redirection(&mut self, output_log: &std::path::Path) -> io::Result<()> {
        let file = File::create(output_log)?;

        // Redirect stdout and stderr to the file
        let mut stdout_fd = unsafe { OwnedFd::from_raw_fd(nix::libc::STDOUT_FILENO) };
        let mut stderr_fd = unsafe { OwnedFd::from_raw_fd(nix::libc::STDERR_FILENO) };
        dup2(&file, &mut stdout_fd).map_err(|e| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Failed to redirect stdout to file: {}", e),
            )
        })?;
        dup2(&file, &mut stderr_fd).map_err(|e| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Failed to redirect stderr to file: {}", e),
            )
        })?;
        std::mem::forget(stdout_fd);
        std::mem::forget(stderr_fd);

        self.newfd = Some(file);
        Ok(())
    }

    /// Manually restore the original stdout/stderr
    ///
    /// This is called automatically by Drop, but can be called manually
    /// if you need to restore output before the CopyOutput is dropped.
    pub fn restore(&mut self) -> io::Result<()> {
        if let (Some(old_stdout), Some(old_stderr)) =
            (self.old_stdout.take(), self.old_stderr.take())
        {
            // Restore original stdout and stderr
            let mut stdout_fd = unsafe { OwnedFd::from_raw_fd(nix::libc::STDOUT_FILENO) };
            let mut stderr_fd = unsafe { OwnedFd::from_raw_fd(nix::libc::STDERR_FILENO) };
            dup2(&old_stdout, &mut stdout_fd).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("Failed to restore stdout: {}", e),
                )
            })?;

            dup2(&old_stderr, &mut stderr_fd).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("Failed to restore stderr: {}", e),
                )
            })?;

            // Don't close stdout/stderr
            std::mem::forget(stdout_fd);
            std::mem::forget(stderr_fd);
            // old_stdout and old_stderr are dropped here, closing the duplicated fds
            drop(old_stdout);
            drop(old_stderr);
        }

        // Clean up tee process or file
        if self.tee {
            if let Some(ref mut process) = self.process.take() {
                process.wait().map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::Other,
                        format!("Failed to wait on tee process: {}", e),
                    )
                })?;
            }
        } else if let Some(ref mut file) = self.newfd.take() {
            file.flush().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("Failed to flush output file: {}", e),
                )
            })?;
        }

        self.release_guard();

        Ok(())
    }

    /// Give up the claim on the process-wide descriptors, once.
    fn release_guard(&mut self) {
        if self.holds_guard {
            self.holds_guard = false;
            REDIRECT_ACTIVE.store(false, Ordering::SeqCst);
        }
    }
}

impl Drop for CopyOutput {
    fn drop(&mut self) {
        // Attempt to restore file descriptors safely
        // In Drop we can't propagate errors, so we log them instead
        if let Err(e) = self.restore() {
            eprintln!(
                "Warning: Failed to restore file descriptors during drop: {}",
                e
            );
            // Continue with cleanup - don't panic in Drop
        }
        self.release_guard();
    }
}

#[cfg(test)]
#[path = "tee_tests.rs"]
mod tee_tests;
