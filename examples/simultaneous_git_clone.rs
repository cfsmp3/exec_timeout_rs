use anyhow::{Context, Result};
use command_timeout::{run_command_with_timeout, CommandError, CommandOutput};
use std::collections::HashMap;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tempfile::Builder;
use tracing::{error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

// Define the Git repository URL
const CCXR: &str = "https://github.com/CCExtractor/ccextractor.git";
const SAMPLE_PLATFORM: &str = "https://github.com/CCExtractor/sample-platform.git";
const FLUTTERGUI: &str = "https://github.com/CCExtractor/ccextractorfluttergui.git";

// Run like this:
// RUST_LOG=debug cargo run --example simultaneous_git_clone

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // Using anyhow for simple example error handling
    // Initialize tracing subscriber
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO) // Default to INFO level
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env()) // Allow RUST_LOG override
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("Setting default tracing subscriber failed");

    info!("Starting simultaneous git clone example...");

    // --- Timeout Configuration ---
    let repos = vec![
        (
            CCXR,
            Duration::from_secs(10),
            Duration::from_secs(300),
            Duration::from_secs(60),
        ),
        (
            SAMPLE_PLATFORM,
            Duration::from_secs(15),
            Duration::from_secs(600),
            Duration::from_secs(120),
        ),
        (
            FLUTTERGUI,
            Duration::from_secs(20),
            Duration::from_secs(900),
            Duration::from_secs(180),
        ),
    ];
    info!("Configured repositories and timeouts:");
    // -----------------------------

    //Print repos and their timeouts
    for (url, min, max, activity) in &repos {
        info!(
            "Repo: '{}', Min Timeout: {:?}, Max Timeout: {:?}, Activity Timeout: {:?}",
            url, min, max, activity
        );
    }

    // Create a temp directories to clone into
    let mut results_summary: HashMap<&str, Result<(), anyhow::Error>> = HashMap::new();
    let clone_futures = repos.iter().map(|(url, min, max, activity)| {
        let url_clone = *url; // Clone the URL for use in the async block
        async move {
            let dir = Builder::new()
                .prefix(&format!(
                    "clone_{}",
                    url.split('/').last().unwrap_or("repo")
                ))
                .tempdir()
                .context("Failed to create temporary directory");

            let result = match dir {
                Ok(dir) => clone_repository(url, dir.path(), *min, *max, *activity).await,
                Err(e) => Err(e),
            };
            (url_clone, result)
        }
    });

    // Run all clone operations concurrently
    let results = futures::future::join_all(clone_futures).await;

    // Collect results into the summary
    for (url, result) in results {
        results_summary.insert(url, result);
    }

    // Log the summary of results
    info!("Clone operations summary:");
    for (url, result) in &results_summary {
        match result {
            Ok(_) => info!("SUCCESS: {}", url),
            Err(e) => error!("FAILED: {} - {:?}", url, e),
        }
    }

    // Log overall status
    let failed_count = results_summary.values().filter(|r| r.is_err()).count();
    if failed_count > 0 {
        error!("{} repositories failed to clone.", failed_count);
    } else {
        info!("All repositories cloned successfully!");
    }

    Ok(())
}

// Prepare the git clone command
async fn clone_repository(
    repo_url: &str,
    target_path: &Path,
    min_timeout: Duration,
    max_timeout: Duration,
    activity_timeout: Duration,
) -> Result<(), anyhow::Error> {
    // Using anyhow for simple example error handling

    //log clone initialization with timeouts
    info!(
        "Preparing to clone '{}' into directory '{}'",
        repo_url,
        target_path.display()
    );
    info!(
        "Timeouts: min={:?}, max={:?}, activity={:?}",
        min_timeout, max_timeout, activity_timeout
    );

    //build git clone command
    let mut cmd = Command::new("git");
    cmd.arg("clone")
        .arg("--progress") // Explicitly ask for progress output on stderr
        .arg(repo_url)
        .arg(target_path); // Clone into the target path

    // Run the command using the library function
    let result = run_command_with_timeout(cmd, min_timeout, max_timeout, activity_timeout).await;

    //error handling
    match result {
        Ok(output) => {
            handle_command_output(output, repo_url, &target_path.to_path_buf());
        }
        Err(e) => {
            // Log specific details first
            match &e {
                // Borrow e here to allow using it later
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

            // Print path even on error
            warn!(
                "Clone operation failed. Directory may be incomplete or empty: {}",
                target_path.display()
            );

            // Now convert the original error (which is still valid) and return
            return Err(e.into());
        }
    }

    Ok(())
}

fn handle_command_output(output: CommandOutput, repo_url: &str, target_path: &PathBuf) {
    info!("Finished cloning '{}'.", repo_url);
    info!("Total Duration: {:?}", output.duration);
    info!("Timed Out: {}", output.timed_out);

    if let Some(status) = output.exit_status {
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
        info!(
            "Stdout (first 1KB):\n---\n{}...\n---",
            String::from_utf8_lossy(&output.stdout.iter().take(1024).cloned().collect::<Vec<_>>())
        );
    }

    info!("Stderr Length: {} bytes", output.stderr.len());
    if !output.stderr.is_empty() {
        // Git clone progress usually goes to stderr
        warn!(
            "Stderr (first 1KB):\n---\n{}...\n---",
            String::from_utf8_lossy(&output.stderr.iter().take(1024).cloned().collect::<Vec<_>>())
        );
        // For full stderr: warn!("Stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    }

    if output.exit_status.map_or(false, |s| s.success()) && !output.timed_out {
        info!("---> Clone completed successfully for '{}'! <---", repo_url);
    } else if output.timed_out {
        error!("---> Clone FAILED due to timeout for '{}'! <---", repo_url);
    } else {
        error!(
            "---> Clone FAILED with non-zero exit status for '{}'! <---",
            repo_url
        );
    }

    //log success with directory location for each clone
    info!(
        "Clone operation finished. Directory location: {}",
        target_path.display()
    );
}
