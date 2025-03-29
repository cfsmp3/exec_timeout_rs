// examples/git_clone_kernel.rs

use command_timeout::{run_command_with_timeout, CommandError, CommandOutput};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf; // <<< Import PathBuf
use std::process::{Command, ExitStatus};
use std::time::Duration;
use tempfile::Builder;
use tracing::{error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

// Define the Git repository URL
const KERNEL_REPO_URL: &str = "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git";
// Or use a smaller repo for quicker testing:
// const KERNEL_REPO_URL: &str = "https://github.com/git/git.git";

// Run like this:
// RUST_LOG=debug cargo run --example git_clone_kernel

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> { // Using anyhow for simple example error handling
    // Initialize tracing subscriber
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO) // Default to INFO level
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env()) // Allow RUST_LOG override
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("Setting default tracing subscriber failed");

    info!("Starting Linux kernel git clone example...");

    // --- Timeout Configuration ---
    let min_timeout = Duration::from_secs(10);
    let max_timeout = Duration::from_secs(60 * 60 * 24 * 7); // 1 week (effectively infinite)
    let activity_timeout = Duration::from_secs(60 * 5); // 5 minutes
    // -----------------------------

    // Create a temporary directory builder
    let temp_dir_builder = Builder::new().prefix("kernel_clone_persistent").tempdir()?; // Use a different prefix
    // --- CHANGE: Keep the path instead of the TempDir object ---
    let clone_target_path_buf: PathBuf = temp_dir_builder.into_path();
    // --- END CHANGE ---
    let clone_target_path_str = clone_target_path_buf.to_str().unwrap_or("."); // Use "." as fallback if path invalid unicode

    info!(
        "Preparing to clone '{}' into directory '{}'", // Updated log message slightly
        KERNEL_REPO_URL,
        clone_target_path_buf.display() // Use display() for logging
    );
    info!(
        "Timeouts: min={:?}, max={:?} (effectively disabled), activity={:?}",
        min_timeout, max_timeout, activity_timeout
    );
    info!("Run with RUST_LOG=debug for detailed library tracing.");
    info!("NOTE: The clone directory will NOT be deleted automatically.");

    // Prepare the git clone command
    let mut cmd = Command::new("git");
    cmd.arg("clone")
       .arg("--progress") // Explicitly ask for progress output on stderr
       .arg(KERNEL_REPO_URL)
       .arg(clone_target_path_str); // Clone into the persistent temp dir path

    // Run the command using the library function
    let command_result = run_command_with_timeout(cmd, min_timeout, max_timeout, activity_timeout).await;

    match command_result {
        Ok(output) => {
            handle_command_output(output);
            // Print path regardless of command success/failure if setup succeeded
            info!(
                "Clone operation finished. Directory location: {}",
                clone_target_path_buf.display()
            );
        }
        Err(e) => {
            // Log specific details first
            match &e { // Borrow e here to allow using it later
                CommandError::Spawn(io_err) => error!("Failed to spawn git: {}", io_err),
                CommandError::Io(io_err) => error!("IO error reading output: {}", io_err),
                CommandError::Kill(io_err) => error!("Error sending kill signal: {}", io_err),
                CommandError::Wait(io_err) => error!("Error waiting for command exit: {}", io_err),
                // Use 'ref msg' to borrow the string instead of moving it
                CommandError::InvalidTimeout(ref msg) => error!("Invalid timeout config: {}", msg),
                CommandError::StdoutPipe => error!("Failed to get stdout pipe from command"),
                CommandError::StderrPipe => error!("Failed to get stderr pipe from command"),
            }
            error!("Command execution failed."); // General failure message

            // --- CHANGE: Print path even on error ---
            warn!(
                "Clone operation failed. Directory may be incomplete or empty: {}",
                clone_target_path_buf.display()
            );
            // --- END CHANGE ---

            // Now convert the original error (which is still valid) and return
            return Err(e.into());
        }
    }

    Ok(())
}

fn handle_command_output(output: CommandOutput) {
    info!("Command finished.");
    info!("Total Duration: {:?}", output.duration);
    info!("Timed Out: {}", output.timed_out);

    // Type hint for clarity, though optional
    let status_opt: Option<ExitStatus> = output.exit_status;

    if let Some(status) = status_opt {
        if status.success() {
            info!("Exit Status: {} (Success)", status);
        } else {
            warn!("Exit Status: {} (Failure)", status);
            if let Some(code) = status.code() {
                warn!("Exit Code: {}", code);
            }
            // signal() is now available because ExitStatusExt is in scope
            if let Some(signal) = status.signal() {
                warn!("Terminated by Signal: {}", signal);
            }
        }
    } else {
        warn!("Exit Status: None (Killed by timeout, status unavailable?)");
    }

    info!("Stdout Length: {} bytes", output.stdout.len());
    if !output.stdout.is_empty() {
        // Print snippet or full output (be cautious with large output)
        info!("Stdout (first 1KB):\n---\n{}...\n---", String::from_utf8_lossy(&output.stdout.iter().take(1024).cloned().collect::<Vec<_>>()));
    }

    info!("Stderr Length: {} bytes", output.stderr.len());
    if !output.stderr.is_empty() {
        // Git clone progress usually goes to stderr
        warn!("Stderr (first 1KB):\n---\n{}...\n---", String::from_utf8_lossy(&output.stderr.iter().take(1024).cloned().collect::<Vec<_>>()));
        // For full stderr: warn!("Stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    }

    if output.exit_status.map_or(false, |s| s.success()) && !output.timed_out {
         info!("---> Clone completed successfully! <---");
    } else if output.timed_out {
         error!("---> Clone FAILED due to timeout. <---");
    } else {
         error!("---> Clone FAILED with non-zero exit status. <---");
    }
}