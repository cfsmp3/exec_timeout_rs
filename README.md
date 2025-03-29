# command-timeout

[![crates.io](https://img.shields.io/crates/v/command-timeout.svg)](https://crates.io/crates/command-timeout) <!-- Replace with actual badge once published -->
[![docs.rs](https://docs.rs/command-timeout/badge.svg)](https://docs.rs/command-timeout) <!-- Replace with actual badge once published -->
<!-- Add build status badge if using CI -->
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](https://opensource.org/licenses/MIT)

A Rust library providing an async (Tokio) function to run external commands with advanced timeout capabilities, specifically designed for Linux.

## Overview

Sometimes, external commands like `git clone` or other network-dependent operations can hang indefinitely due to network issues or other problems. This library provides a way to run such commands while enforcing multiple timeout constraints:

1.  **Minimum Timeout:** Ensures the command runs for at least a specified duration before being eligible for termination due to inactivity (unless it finishes naturally earlier).
2.  **Maximum Timeout:** An absolute time limit after which the command *will* be terminated, regardless of activity.
3.  **Activity Timeout:** If the command doesn't produce any output on stdout or stderr for a specified duration, it's considered idle. Activity resets this timer.

The primary goal is to allow commands to run as long as they are actively making progress (producing output) but terminate them if they become stuck or exceed a hard limit.

## Features

*   **Async Execution:** Built on Tokio for non-blocking command execution.
*   **Sophisticated Timeouts:** Combines minimum execution time, maximum execution time, and inactivity detection.
*   **Activity Detection:** Monitors stdout and stderr; any output resets the inactivity timer.
*   **Robust Termination:** Kills the entire process group (using `SIGKILL` via `nix::sys::signal::killpg`) to ensure child processes (like `sleep` started by `sh`) are also terminated.
*   **Output Capture:** Captures stdout and stderr as raw `Vec<u8>`, suitable for binary data.
*   **Detailed Results:** Returns a struct (`CommandOutput`) containing captured output, exit status (`Option<ExitStatus>`), total duration, and whether a timeout occurred.
*   **Tracing Integration:** Uses the `tracing` crate (`debug!` level) for detailed logging of internal events (spawning, deadlines, activity, kills).

## Platform Limitations

*   **Linux Only:** This crate relies on Linux-specific features (`setpgid`, `killpg` with SIGKILL) for process group management and termination. It will not compile or work correctly on other platforms like Windows or macOS.

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
command-timeout = "0.1.0" # Replace with the desired version from crates.io
# Required dependencies if you don't already have them
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
thiserror = "1.0"
nix = { version = "0.29", features = ["signal", "process"] } # Check latest version
libc = "0.2"
```

## Usage

The main entry point is the run_command_with_timeout async function.

```   
use command_timeout::{run_command_with_timeout, CommandOutput, CommandError};
use std::process::Command;
use std::time::Duration;
use tokio; // Ensure tokio runtime is available

#[tokio::main]
async fn main() -> Result<(), CommandError> {
    // 1. Configure the command
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
       .arg("echo 'Starting...'; sleep 1; echo 'Progress...' >&2; sleep 3; echo 'Done.'");

    // 2. Define timeouts
    let min_timeout = Duration::from_millis(500);    // Must run for at least 0.5s
    let max_timeout = Duration::from_secs(10);       // Absolute limit of 10s
    let activity_timeout = Duration::from_secs(2); // Kill if idle for 2s

    // 3. Run the command
    println!("Running command...");
    let result = run_command_with_timeout(
        cmd,
        min_timeout,
        max_timeout,
        activity_timeout,
    ).await?; // Handle potential setup/IO errors

    // 4. Process the results
    println!("\n--- Results ---");
    println!("Timed Out: {}", result.timed_out);
    println!("Duration: {:?}", result.duration);

    if let Some(status) = result.exit_status {
        println!("Exit Status: {}", status);
        println!("Exit Code: {:?}", status.code());
        println!("Terminated by Signal: {:?}", status.signal()); // Useful if killed
    } else {
        println!("Exit Status: None (Process killed and status maybe unavailable)");
    }

    // Output is Vec<u8>, use from_utf8_lossy for display if expecting text
    println!("Stdout:\n{}", String::from_utf8_lossy(&result.stdout));
    println!("Stderr:\n{}", String::from_utf8_lossy(&result.stderr));

    Ok(())
}
```