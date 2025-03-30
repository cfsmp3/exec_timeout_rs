// src/lib.rs

use std::os::unix::process::CommandExt; // For pre_exec
use std::process::{Command as StdCommand, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, Command as TokioCommand};
use tokio::time::sleep_until;
use tracing::{debug, instrument, warn};
// --- Add nix imports ---
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
// --- End add ---

// --- Structs and Enums ---

/// Represents the output of a command executed with `run_command_with_timeout`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// The data captured from the command's standard output (stdout).
    pub stdout: Vec<u8>,
    /// The data captured from the command's standard error (stderr).
    pub stderr: Vec<u8>,
    /// The exit status of the command. `None` if the command was killed due to a timeout or if waiting failed after kill.
    pub exit_status: Option<ExitStatus>,
    /// The total time the command ran or was allowed to run before being terminated.
    pub duration: Duration,
    /// Indicates whether the command was terminated due to exceeding a timeout condition.
    pub timed_out: bool,
}

/// Errors that can occur during command execution.
#[derive(Error, Debug)]
pub enum CommandError {
    #[error("Failed to spawn command")]
    Spawn(#[source] std::io::Error),
    #[error("Failed to get command stdout")]
    StdoutPipe,
    #[error("Failed to get command stderr")]
    StderrPipe,
    #[error("I/O error reading command output")]
    Io(#[source] std::io::Error),
    #[error("Failed to kill command")]
    Kill(#[source] std::io::Error),
    #[error("Failed to wait for command exit")]
    Wait(#[source] std::io::Error),
    #[error("Invalid timeout configuration: {0}")]
    InvalidTimeout(String),
}

/// Configuration for command timeouts.
#[derive(Clone, Copy, Debug)]
struct TimeoutConfig {
    minimum: Duration,
    maximum: Duration,
    activity: Duration,
    start_time: Instant,
    absolute_deadline: Instant,
}

/// Holds the state during command execution.
struct CommandExecutionState<R1: AsyncRead + Unpin, R2: AsyncRead + Unpin> {
    child: Child,
    stdout_reader: Option<BufReader<R1>>,
    stderr_reader: Option<BufReader<R2>>,
    stdout_buffer: Vec<u8>,
    stderr_buffer: Vec<u8>,
    stdout_read_buffer: Vec<u8>,
    stderr_read_buffer: Vec<u8>,
    current_deadline: Instant,
    timed_out: bool,
    exit_status: Option<ExitStatus>,
}

// --- Helper Functions (Definitions Before Use) ---

/// Validates the timeout durations.
fn validate_timeouts(min: Duration, max: Duration, activity: Duration) -> Result<(), CommandError> {
    if min > max {
        return Err(CommandError::InvalidTimeout(format!(
            "minimum_timeout ({:?}) cannot be greater than maximum_timeout ({:?})",
            min, max
        )));
    }
    if activity == Duration::ZERO {
        return Err(CommandError::InvalidTimeout(
            "activity_timeout must be positive".to_string(),
        ));
    }
    Ok(())
}

/// Spawns the command, sets up pipes, and initializes the execution state.
/// Note: The command passed in should already have pre_exec configured if needed.
fn spawn_command_and_setup_state(
    command: &mut StdCommand,
    initial_deadline: Instant,
) -> Result<CommandExecutionState<impl AsyncRead + Unpin, impl AsyncRead + Unpin>, CommandError> {
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    // Command already has pre_exec set by the caller function
    let mut tokio_cmd = TokioCommand::from(std::mem::replace(command, StdCommand::new("")));

    let mut child = tokio_cmd
        .kill_on_drop(true)
        .spawn()
        .map_err(CommandError::Spawn)?;

    debug!(pid = child.id(), "Process spawned successfully");

    let stdout_pipe = child.stdout.take().ok_or(CommandError::StdoutPipe)?;
    let stderr_pipe = child.stderr.take().ok_or(CommandError::StderrPipe)?;

    debug!(deadline = ?initial_deadline, "Initial deadline set");

    Ok(CommandExecutionState {
        child,
        stdout_reader: Some(BufReader::new(stdout_pipe)),
        stderr_reader: Some(BufReader::new(stderr_pipe)),
        stdout_buffer: Vec::new(),
        stderr_buffer: Vec::new(),
        stdout_read_buffer: Vec::with_capacity(1024),
        stderr_read_buffer: Vec::with_capacity(1024),
        current_deadline: initial_deadline,
        timed_out: false,
        exit_status: None,
    })
}

/// Calculates the next deadline based on activity, capped by the absolute deadline.
fn calculate_new_deadline(absolute_deadline: Instant, activity_timeout: Duration) -> Instant {
    let potential_new_deadline = Instant::now() + activity_timeout;
    let new_deadline = std::cmp::min(potential_new_deadline, absolute_deadline);
    debug!(
        potential = ?potential_new_deadline,
        absolute = ?absolute_deadline,
        new = ?new_deadline,
        "Calculated new deadline based on activity"
    );
    new_deadline
}

/// Updates the current deadline based on detected activity.
#[instrument(level = "debug", skip(current_deadline, timeouts))]
fn handle_stream_activity(
    bytes_read: usize,
    stream_name: &str,
    current_deadline: &mut Instant,
    timeouts: &TimeoutConfig,
) {
    debug!(
        bytes = bytes_read,
        stream = stream_name,
        "Activity detected"
    );
    let new_deadline = calculate_new_deadline(timeouts.absolute_deadline, timeouts.activity);
    if new_deadline != *current_deadline {
        debug!(old = ?*current_deadline, new = ?new_deadline, "Updating deadline");
        *current_deadline = new_deadline;
    } else {
        debug!(deadline = ?*current_deadline, "Deadline remains unchanged (likely at absolute limit)");
    }
}

/// Reads a chunk from the stream using read_buf.
async fn read_stream_chunk<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<usize>> {
    // read_buf appends to the vector's initialized part.
    // Caller MUST clear the buffer afterwards if reusing it.
    match reader.read_buf(buf).await {
        Ok(0) => Ok(None),    // EOF
        Ok(n) => Ok(Some(n)), // Read n bytes
        Err(e) => Err(e),
    }
}

/// Drains remaining data from an optional reader into a buffer.
async fn drain_reader<R: AsyncRead + Unpin>(
    reader_opt: &mut Option<BufReader<R>>,
    buffer: &mut Vec<u8>,
    read_buf: &mut Vec<u8>, // Use the per-stream read buffer
    stream_name: &str,
) -> Result<(), CommandError> {
    if let Some(reader) = reader_opt.as_mut() {
        debug!("Draining remaining output from {}", stream_name);
        loop {
            read_buf.clear(); // Clear temporary buffer before reading new chunk
            match read_stream_chunk(reader, read_buf).await {
                Ok(Some(n)) => {
                    if n > 0 {
                        debug!("Drained {} bytes from {}", n, stream_name);
                        buffer.extend_from_slice(&read_buf[..n]);
                    } else {
                        debug!("Drained 0 bytes from {}, treating as EOF.", stream_name);
                        break; // Should be caught by Ok(None) but handles defensively
                    }
                }
                Ok(None) => {
                    // EOF
                    debug!("EOF reached while draining {}", stream_name);
                    break; // Finished draining
                }
                Err(e) => {
                    // Ignore expected errors after process exit, log others
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                    ) {
                        debug!(
                            "{} closed while draining ({}): {}",
                            stream_name,
                            e.kind(),
                            e
                        );
                    } else {
                        warn!("Error draining remaining {} output: {}", stream_name, e);
                        // Don't return error, just stop draining and report what was gathered
                    }
                    break; // Stop draining on error
                }
            }
        }
        // Mark as drained by removing the reader
        *reader_opt = None;
        debug!("Finished draining {}", stream_name);
    }
    Ok(())
}

/// Handles the timeout event: logs, attempts to kill process group, checks for immediate exit.
/// Returns Ok(Some(status)) if process exited before kill, Ok(None) if kill attempted/succeeded, Err on failure.
#[instrument(level = "debug", skip(child, timeouts))]
async fn handle_timeout_event(
    child: &mut Child,
    triggered_deadline: Instant,
    timeouts: &TimeoutConfig,
) -> Result<Option<ExitStatus>, CommandError> {
    let now = Instant::now();
    let elapsed = now.duration_since(timeouts.start_time);
    debug!(deadline = ?triggered_deadline.duration_since(timeouts.start_time), elapsed = ?elapsed, "Timeout check triggered");
    let killed_reason;

    if now >= timeouts.absolute_deadline {
        debug!(timeout=?timeouts.maximum, "Maximum timeout exceeded");
        killed_reason = "maximum timeout";
    } else {
        debug!(timeout=?timeouts.activity, min_duration=?timeouts.minimum, "Activity timeout likely exceeded after minimum duration");
        killed_reason = "activity timeout";
    }

    let pid_opt = child.id(); // Get the PID (u32) of the direct child

    if let Some(pid_u32) = pid_opt {
        warn!(
            pid = pid_u32,
            reason = killed_reason,
            elapsed = ?elapsed,
            "Killing process group due to timeout"
        );
        // Convert u32 PID to nix's Pid type (i32)
        let pid = Pid::from_raw(pid_u32 as i32);
        // Send SIGKILL to the entire process group.
        // killpg takes the PID of any process in the group (usually the leader)
        // and signals the entire group associated with that process.
        match killpg(pid, Signal::SIGKILL) {
            Ok(()) => {
                debug!(
                    pid = pid_u32,
                    pgid = pid.as_raw(),
                    "Process group kill signal (SIGKILL) sent successfully."
                );
                // Signal sent, the final wait() in finalize_exit_status will reap the original child.
                Ok(None)
            }
            Err(e) => {
                // ESRCH means the process group doesn't exist (likely all processes exited quickly)
                if e == nix::errno::Errno::ESRCH {
                    warn!(pid = pid_u32, error = %e, "Failed to kill process group (ESRCH - likely already exited). Checking child status.");
                    // Check if the *original* child process exited
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            debug!(pid = pid_u32, status = %status, "Original child had already exited before kill signal processed");
                            return Ok(Some(status)); // Treat as natural exit
                        }
                        Ok(None) => {
                            debug!(pid = pid_u32, "Original child still running or uncollected after killpg failed (ESRCH).");
                            // Proceed as if timeout kill was attempted.
                            return Ok(None);
                        }
                        Err(wait_err) => {
                            warn!(pid = pid_u32, error = %wait_err, "Error checking child status after failed killpg (ESRCH)");
                            return Err(CommandError::Wait(wait_err));
                        }
                    }
                } else {
                    // Another error occurred during killpg (e.g., permissions EPERM)
                    warn!(pid = pid_u32, pgid = pid.as_raw(), error = %e, "Failed to send kill signal to process group.");
                    // Map nix::Error to std::io::Error for CommandError::Kill
                    return Err(CommandError::Kill(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("Failed to kill process group for PID {}: {}", pid_u32, e),
                    )));
                }
            }
        }
    } else {
        // This case should be extremely unlikely if spawn succeeded.
        warn!(
            "Could not get PID to kill process for timeout. Process might have exited abnormally."
        );
        // Cannot attempt kill, treat as timed out, let finalize_exit_status handle wait().
        Ok(None)
    }
}

/// The main `select!` loop monitoring the process, streams, and timeouts.
async fn run_command_loop(
    state: &mut CommandExecutionState<impl AsyncRead + Unpin, impl AsyncRead + Unpin>,
    timeouts: &TimeoutConfig,
) -> Result<(), CommandError> {
    loop {
        let deadline_sleep = sleep_until(state.current_deadline.into());
        tokio::pin!(deadline_sleep);

        // Conditions to enable select branches
        let can_read_stdout = state.stdout_reader.is_some() && state.exit_status.is_none();
        let can_read_stderr = state.stderr_reader.is_some() && state.exit_status.is_none();
        let can_check_exit = state.exit_status.is_none();
        let can_check_timeout = state.exit_status.is_none();

        tokio::select! {
            biased; // Prioritize checking exit status

            // 1. Check for process exit
            result = state.child.wait(), if can_check_exit => {
                state.exit_status = match result {
                    Ok(status) => {
                        debug!(status = %status, "Process exited naturally");
                        Some(status)
                    },
                    Err(e) => {
                        warn!(error = %e, "Error waiting for process exit");
                        return Err(CommandError::Wait(e));
                    }
                };
                break; // Process finished naturally
            }

            // 2. Read from stdout (Safer access pattern)
            read_result = async {
                if let Some(reader) = state.stdout_reader.as_mut() {
                    if state.exit_status.is_none() {
                       read_stream_chunk(reader, &mut state.stdout_read_buffer).await
                    } else { Ok(None) } // Treat as EOF if process exited
                } else { Ok(None) } // Reader gone
            }, if can_read_stdout => {
                match read_result {
                    Ok(Some(n)) => {
                        state.stdout_buffer.extend_from_slice(&state.stdout_read_buffer[..n]);
                        handle_stream_activity(n, "stdout", &mut state.current_deadline, timeouts);
                    }
                    Ok(None) => { // EOF or reader gone or process exited during poll
                        if state.stdout_reader.is_some() {
                           debug!("Stdout pipe closed (EOF) or process exited during read.");
                           state.stdout_reader = None; // Mark as closed
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "Error reading stdout");
                        state.stdout_read_buffer.clear(); // Clear buffer even on error
                        return Err(CommandError::Io(e));
                    }
                }
                state.stdout_read_buffer.clear(); // Clear after processing
            }

            // 3. Read from stderr (Safer access pattern)
            read_result = async {
                 if let Some(reader) = state.stderr_reader.as_mut() {
                     if state.exit_status.is_none() {
                        read_stream_chunk(reader, &mut state.stderr_read_buffer).await
                     } else { Ok(None) } // Treat as EOF if process exited
                 } else { Ok(None) } // Reader gone
            }, if can_read_stderr => {
                 match read_result {
                    Ok(Some(n)) => {
                        state.stderr_buffer.extend_from_slice(&state.stderr_read_buffer[..n]);
                        handle_stream_activity(n, "stderr", &mut state.current_deadline, timeouts);
                    }
                    Ok(None) => { // EOF or reader gone or process exited during poll
                         if state.stderr_reader.is_some() {
                            debug!("Stderr pipe closed (EOF) or process exited during read.");
                            state.stderr_reader = None; // Mark as closed
                         }
                    }
                    Err(e) => {
                        warn!(error = %e, "Error reading stderr");
                        state.stderr_read_buffer.clear(); // Clear on error
                        return Err(CommandError::Io(e));
                    }
                }
                state.stderr_read_buffer.clear(); // Clear after processing
            }

            // 4. Check for timeout
            _ = &mut deadline_sleep, if can_check_timeout => {
                match handle_timeout_event(
                    &mut state.child,
                    state.current_deadline,
                    timeouts
                ).await {
                    Ok(Some(status)) => { // Process exited just before kill
                        debug!("Timeout detected but process already exited.");
                        state.exit_status = Some(status);
                        state.timed_out = false; // Not actually killed by us
                    }
                    Ok(None) => { // Timeout occurred, kill attempted/succeeded.
                        state.timed_out = true;
                    }
                    Err(e) => { // Error during kill or subsequent check
                        return Err(e);
                    }
                }
                 break; // Exit loop after timeout event
            }
        }
    } // end loop

    Ok(())
}

/// Waits for the child process to exit if it was killed and status isn't known yet.
async fn finalize_exit_status(
    child: &mut Child,
    current_status: Option<ExitStatus>,
    timed_out: bool,
) -> Result<Option<ExitStatus>, CommandError> {
    if timed_out && current_status.is_none() {
        debug!(
            pid = child.id(),
            "Waiting for process to exit after kill signal..."
        );
        match child.wait().await {
            Ok(status) => {
                debug!(pid = child.id(), status = %status, "Process exited after kill");
                Ok(Some(status))
            }
            Err(e) => {
                warn!(pid = child.id(), error = %e, "Error waiting for process exit after kill. Proceeding without status.");
                Ok(None) // Kill attempted, but final status couldn't be obtained
            }
        }
    } else {
        Ok(current_status) // Already have status, or didn't time out
    }
}

// --- Public API Function ---

/// Runs a standard library `Command` asynchronously with sophisticated timeout logic.
///
/// (Rustdoc remains the same)
#[instrument(skip(command), fields(command = ?command.get_program(), args = ?command.get_args()))]
pub async fn run_command_with_timeout(
    mut command: StdCommand,
    minimum_timeout: Duration,
    maximum_timeout: Duration,
    activity_timeout: Duration,
) -> Result<CommandOutput, CommandError> {
    validate_timeouts(minimum_timeout, maximum_timeout, activity_timeout)?;

    let start_time = Instant::now();
    let absolute_deadline = start_time + maximum_timeout;
    let initial_deadline = std::cmp::min(
        absolute_deadline,
        start_time + std::cmp::max(minimum_timeout, activity_timeout),
    );

    let timeout_config = TimeoutConfig {
        minimum: minimum_timeout,
        maximum: maximum_timeout,
        activity: activity_timeout,
        start_time,
        absolute_deadline,
    };

    // Configure the command to run in its own process group
    // This MUST be done before spawning the command.
    // Take ownership to modify, then pass the modified command to spawn_command_and_setup_state
    let mut std_cmd = std::mem::replace(&mut command, StdCommand::new("")); // Take ownership temporarily
    unsafe {
        std_cmd.pre_exec(|| {
            // libc::setpgid(0, 0) makes the new process its own group leader.
            // Pass 0 for both pid and pgid to achieve this for the calling process.
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                // Capture the error from the OS if setpgid fails
                Err(std::io::Error::last_os_error())
            }
        });
    }
    // Put the modified command back for spawning
    command = std_cmd;

    // Setup state (spawns command with pre_exec hook)
    let mut state = spawn_command_and_setup_state(&mut command, initial_deadline)?;

    // Main execution loop
    run_command_loop(&mut state, &timeout_config).await?;

    // Drain remaining output after loop exit (natural exit or timeout break)
    debug!("Command loop finished. Draining remaining output streams.");
    drain_reader(
        &mut state.stdout_reader,
        &mut state.stdout_buffer,
        &mut state.stdout_read_buffer,
        "stdout",
    )
    .await?;
    drain_reader(
        &mut state.stderr_reader,
        &mut state.stderr_buffer,
        &mut state.stderr_read_buffer,
        "stderr",
    )
    .await?;

    // Post-loop processing: Final wait if killed and status not yet obtained
    let final_exit_status = finalize_exit_status(
        &mut state.child,
        state.exit_status, // Use status potentially set in loop
        state.timed_out,
    )
    .await?;

    let end_time = Instant::now();
    let duration = end_time.duration_since(start_time);

    debug!(
        duration = ?duration,
        exit_status = ?final_exit_status,
        timed_out = state.timed_out,
        stdout_len = state.stdout_buffer.len(),
        stderr_len = state.stderr_buffer.len(),
        "Command execution finished."
    );

    Ok(CommandOutput {
        stdout: state.stdout_buffer,
        stderr: state.stderr_buffer,
        exit_status: final_exit_status,
        duration,
        timed_out: state.timed_out,
    })
}

// ----------- Tests -----------
#[cfg(test)]
mod tests {
    use super::*;
    use libc;
    use std::os::unix::process::ExitStatusExt; // For signal checking
    use tokio::runtime::Runtime;
    use tracing_subscriber::{fmt, EnvFilter}; // Make sure libc is in scope for SIGKILL constant

    // Helper to initialize tracing for tests
    fn setup_tracing() {
        // Use `RUST_LOG=debug` env var to see logs, default info
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .try_init()
            .ok(); // ok() ignores errors if already initialized
    }

    // Helper to run async tests
    fn run_async_test<F, Fut>(test_fn: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        setup_tracing();
        let rt = Runtime::new().unwrap();
        rt.block_on(test_fn());
    }

    #[test]
    fn test_command_runs_successfully_within_timeouts() {
        run_async_test(|| async {
            let mut cmd = StdCommand::new("sh");
            cmd.arg("-c")
                .arg("echo 'Hello'; sleep 0.1; echo 'World' >&2");

            let min_timeout = Duration::from_millis(50);
            let max_timeout = Duration::from_secs(2);
            let activity_timeout = Duration::from_secs(1);

            let result = run_command_with_timeout(cmd, min_timeout, max_timeout, activity_timeout)
                .await
                .expect("Command failed unexpectedly");

            assert_eq!(result.stdout, b"Hello\n");
            assert_eq!(result.stderr, b"World\n");
            assert!(result.exit_status.is_some(), "Exit status should be Some");
            assert_eq!(
                result.exit_status.unwrap().code(),
                Some(0),
                "Exit code should be 0"
            );
            assert!(!result.timed_out, "Should not have timed out");
            assert!(
                result.duration >= Duration::from_millis(100),
                "Duration should be >= 100ms"
            );
            assert!(
                result.duration < max_timeout,
                "Duration should be < max_timeout"
            );
        });
    }

    #[test]
    fn test_command_exits_quickly_before_min_timeout() {
        run_async_test(|| async {
            let mut cmd = StdCommand::new("echo");
            cmd.arg("Immediate exit");

            let min_timeout = Duration::from_secs(2); // Long min timeout
            let max_timeout = Duration::from_secs(5);
            let activity_timeout = Duration::from_secs(1);

            let start = Instant::now();
            let result = run_command_with_timeout(cmd, min_timeout, max_timeout, activity_timeout)
                .await
                .expect("Command failed unexpectedly");

            let duration = start.elapsed();

            assert_eq!(result.stdout, b"Immediate exit\n");
            assert!(result.stderr.is_empty(), "Stderr should be empty");
            assert!(result.exit_status.is_some(), "Exit status should be Some");
            assert_eq!(
                result.exit_status.unwrap().code(),
                Some(0),
                "Exit code should be 0"
            );
            assert!(!result.timed_out, "Should not have timed out");
            assert!(
                duration < Duration::from_millis(500),
                "Test duration should be short"
            );
            assert!(
                result.duration < Duration::from_millis(500),
                "Reported duration should be short"
            );
        });
    }

    #[test]
    fn test_maximum_timeout_kills_long_running_command() {
        run_async_test(|| async {
            let mut cmd = StdCommand::new("sleep");
            cmd.arg("5"); // Sleeps for 5 seconds

            let min_timeout = Duration::from_millis(100);
            let max_timeout = Duration::from_secs(1); // Max timeout is 1 second
            let activity_timeout = Duration::from_secs(10); // Activity > max to ensure max triggers

            let result = run_command_with_timeout(cmd, min_timeout, max_timeout, activity_timeout)
                .await
                .expect("Command failed unexpectedly");

            assert!(result.stdout.is_empty(), "Stdout should be empty");
            assert!(result.stderr.is_empty(), "Stderr should be empty");
            assert!(
                result.exit_status.is_some(),
                "Exit status should be Some after kill"
            );
            // SIGKILL is signal 9
            assert_eq!(
                result.exit_status.unwrap().signal(),
                Some(libc::SIGKILL as i32),
                "Should be killed by SIGKILL"
            );
            assert!(result.timed_out, "Should have timed out");
            assert!(
                result.duration >= max_timeout,
                "Duration should be >= max_timeout"
            );
            // Allow slightly more buffer for process group kill and reaping
            assert!(
                result.duration < max_timeout + Duration::from_millis(750),
                "Duration allow buffer"
            );
        });
    }

    #[test]
    fn test_activity_timeout_kills_idle_command_after_min_timeout() {
        run_async_test(|| async {
            let mut cmd = StdCommand::new("sh");
            cmd.arg("-c")
                .arg("echo 'Initial output'; sleep 5; echo 'This should not appear'");

            let min_timeout = Duration::from_millis(200);
            let max_timeout = Duration::from_secs(10);
            let activity_timeout = Duration::from_secs(1); // Kill after 1s of inactivity

            let result = run_command_with_timeout(cmd, min_timeout, max_timeout, activity_timeout)
                .await
                .expect("Command failed unexpectedly");

            assert_eq!(result.stdout, b"Initial output\n");
            assert!(result.stderr.is_empty(), "Stderr should be empty");
            assert!(
                result.exit_status.is_some(),
                "Exit status should be Some after kill"
            );
            // SIGKILL is signal 9
            assert_eq!(
                result.exit_status.unwrap().signal(),
                Some(libc::SIGKILL as i32),
                "Should be killed by SIGKILL"
            );
            assert!(result.timed_out, "Should have timed out");

            // Duration Assertions (Process group kill should be faster now)
            // 1. Should run for at least the minimum guaranteed time
            assert!(
                result.duration >= min_timeout,
                "Duration ({:?}) should be >= min_timeout ({:?})",
                result.duration,
                min_timeout
            );

            // 2. Should run for approximately the activity timeout after the initial echo.
            let lower_bound = activity_timeout; // Kill signal sent *at* activity timeout
            let upper_bound = activity_timeout + Duration::from_millis(750); // Allow generous buffer for reaping
            assert!(
                result.duration >= lower_bound,
                "Duration ({:?}) should be >= activity_timeout ({:?})",
                result.duration,
                lower_bound
            );
            assert!(
                result.duration < upper_bound,
                "Duration ({:?}) should be < activity_timeout plus buffer ({:?})",
                result.duration,
                upper_bound
            );

            // 3. Must be killed before the internal sleep finishes
            assert!(
                result.duration < Duration::from_secs(5),
                "Should be killed before sleep 5 ends"
            );
        });
    }

    #[test]
    fn test_activity_resets_timeout_allowing_completion() {
        run_async_test(|| async {
            let mut cmd = StdCommand::new("sh");
            cmd.arg("-c")
                .arg("echo '1'; sleep 0.5; echo '2' >&2; sleep 0.5; echo '3'; sleep 0.5; echo '4'");

            let min_timeout = Duration::from_millis(100);
            let max_timeout = Duration::from_secs(5);
            let activity_timeout = Duration::from_secs(1); // Activity timeout > sleep interval

            let result = run_command_with_timeout(cmd, min_timeout, max_timeout, activity_timeout)
                .await
                .expect("Command failed unexpectedly");

            assert_eq!(result.stdout, b"1\n3\n4\n");
            assert_eq!(result.stderr, b"2\n");
            assert!(result.exit_status.is_some(), "Exit status should be Some");
            assert_eq!(
                result.exit_status.unwrap().code(),
                Some(0),
                "Exit code should be 0"
            );
            assert!(!result.timed_out, "Should not have timed out");
            assert!(
                result.duration > Duration::from_secs(1),
                "Duration should be > 1s (actual ~1.5s)"
            );
            assert!(
                result.duration < max_timeout,
                "Duration should be < max_timeout"
            );
        });
    }

    #[test]
    fn test_binary_output_is_handled() {
        run_async_test(|| async {
            let mut cmd = StdCommand::new("head");
            cmd.arg("-c").arg("50").arg("/dev/urandom");

            let min_timeout = Duration::from_millis(50);
            let max_timeout = Duration::from_secs(2);
            let activity_timeout = Duration::from_secs(1);

            let result = run_command_with_timeout(cmd, min_timeout, max_timeout, activity_timeout)
                .await
                .expect("Command failed unexpectedly");

            assert_eq!(result.stdout.len(), 50, "Stdout length should be 50");
            assert!(result.stderr.is_empty(), "Stderr should be empty");
            assert!(result.exit_status.is_some(), "Exit status should be Some");
            assert_eq!(
                result.exit_status.unwrap().code(),
                Some(0),
                "Exit code should be 0"
            );
            assert!(!result.timed_out, "Should not have timed out");
        });
    }

    #[test]
    fn test_command_not_found() {
        run_async_test(|| async {
            let cmd = StdCommand::new("a_command_that_does_not_exist_hopefully"); // removed mut

            let min_timeout = Duration::from_millis(50);
            let max_timeout = Duration::from_secs(2);
            let activity_timeout = Duration::from_secs(1);

            let result =
                run_command_with_timeout(cmd, min_timeout, max_timeout, activity_timeout).await;

            assert!(result.is_err(), "Should return error");
            match result.err().unwrap() {
                CommandError::Spawn(e) => {
                    assert_eq!(
                        e.kind(),
                        std::io::ErrorKind::NotFound,
                        "Error kind should be NotFound"
                    );
                }
                e => panic!("Expected CommandError::Spawn, got {:?}", e),
            }
        });
    }

    #[test]
    fn test_min_timeout_greater_than_max_timeout() {
        run_async_test(|| async {
            let cmd = StdCommand::new("echo"); // removed mut
                                               // cmd.arg("test"); // Don't need args

            let min_timeout = Duration::from_secs(2);
            let max_timeout = Duration::from_secs(1); // Invalid config
            let activity_timeout = Duration::from_secs(1);

            let result =
                run_command_with_timeout(cmd, min_timeout, max_timeout, activity_timeout).await;

            assert!(result.is_err(), "Should return error");
            match result.err().unwrap() {
                CommandError::InvalidTimeout(_) => {} // Expected error
                e => panic!("Expected CommandError::InvalidTimeout, got {:?}", e),
            }
        });
    }

    #[test]
    fn test_zero_activity_timeout() {
        run_async_test(|| async {
            let cmd = StdCommand::new("echo"); // removed mut
                                               // cmd.arg("test"); // Don't need args

            let min_timeout = Duration::from_millis(100);
            let max_timeout = Duration::from_secs(1);
            let activity_timeout = Duration::ZERO; // Invalid config

            let result =
                run_command_with_timeout(cmd, min_timeout, max_timeout, activity_timeout).await;

            assert!(result.is_err(), "Should return error");
            match result.err().unwrap() {
                CommandError::InvalidTimeout(_) => {} // Expected error
                e => panic!("Expected CommandError::InvalidTimeout, got {:?}", e),
            }
        });
    }

    #[test]
    fn test_process_exits_with_error_code() {
        run_async_test(|| async {
            let mut cmd = StdCommand::new("sh");
            cmd.arg("-c").arg("echo 'Error message' >&2; exit 55");

            let min_timeout = Duration::from_millis(50);
            let max_timeout = Duration::from_secs(2);
            let activity_timeout = Duration::from_secs(1);

            let result = run_command_with_timeout(cmd, min_timeout, max_timeout, activity_timeout)
                .await
                .expect("Command failed unexpectedly");

            assert!(result.stdout.is_empty(), "Stdout should be empty");
            assert_eq!(result.stderr, b"Error message\n");
            assert!(result.exit_status.is_some(), "Exit status should be Some");
            assert_eq!(
                result.exit_status.unwrap().code(),
                Some(55),
                "Exit code should be 55"
            );
            assert!(!result.timed_out, "Should not have timed out");
        });
    }

    #[test]
    fn test_continuous_output_does_not_timeout() {
        run_async_test(|| async {
            let mut cmd = StdCommand::new("sh");
            // Continuously output numbers for ~2 seconds, sleeping shortly
            cmd.arg("-c")
                .arg("i=0; while [ $i -lt 20 ]; do echo $i; i=$((i+1)); sleep 0.1; done");

            let min_timeout = Duration::from_millis(50);
            let max_timeout = Duration::from_secs(10);
            let activity_timeout = Duration::from_millis(500); // activity > sleep

            let result = run_command_with_timeout(cmd, min_timeout, max_timeout, activity_timeout)
                .await
                .expect("Command failed unexpectedly");

            assert!(!result.stdout.is_empty(), "Stdout should not be empty");
            assert!(result.stderr.is_empty(), "Stderr should be empty");
            assert!(result.exit_status.is_some(), "Exit status should be Some");
            assert_eq!(
                result.exit_status.unwrap().code(),
                Some(0),
                "Exit code should be 0"
            );
            assert!(!result.timed_out, "Should not have timed out");
            assert!(
                result.duration > Duration::from_secs(2),
                "Duration should be > 2s"
            ); // 20 * 0.1s
            assert!(
                result.duration < Duration::from_secs(3),
                "Duration should be < 3s"
            );
        });
    }

    #[test]
    fn test_timeout_immediately_if_min_timeout_is_zero_and_no_activity() {
        run_async_test(|| async {
            let mut cmd = StdCommand::new("sleep");
            cmd.arg("5");

            let min_timeout = Duration::ZERO; // Allows immediate check
            let max_timeout = Duration::from_secs(10);
            let activity_timeout = Duration::from_millis(100); // Check quickly

            let result = run_command_with_timeout(cmd, min_timeout, max_timeout, activity_timeout)
                .await
                .expect("Command failed unexpectedly");

            assert!(result.stdout.is_empty(), "Stdout should be empty");
            assert!(result.stderr.is_empty(), "Stderr should be empty");
            assert!(
                result.exit_status.is_some(),
                "Exit status should be Some after kill"
            );
            // SIGKILL is signal 9
            assert_eq!(
                result.exit_status.unwrap().signal(),
                Some(libc::SIGKILL as i32),
                "Should be killed by SIGKILL"
            );
            assert!(result.timed_out, "Should have timed out");
            // Should be killed after activity_timeout (since min is 0)
            assert!(
                result.duration >= activity_timeout,
                "Duration should be >= activity_timeout"
            );
            // Allow slightly more buffer for process group kill and reaping
            assert!(
                result.duration < activity_timeout + Duration::from_millis(750),
                "Duration allow buffer"
            );
        });
    }
} // end tests mod
