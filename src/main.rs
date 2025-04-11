use clap::Parser;
use serde::Deserialize;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command as StdCommand;
use std::time::Duration;

use command_timeout::{run_command_with_timeout};

/// CLI for the command timeout library.
///
/// Example usage:
///
///     cargo run -c "curl https://www.google.com/" -conf config.toml
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// External command to execute (e.g., curl https://www.google.com/).
    /// If not provided, you will be prompted.
    #[arg(short = 'c')]
    command: Option<String>,

    /// Path to the configuration file (TOML) that contains timeout settings.
    /// If not provided, you will be prompted.
    #[arg(short = 'f', long = "conf")]
    config: Option<PathBuf>,
}

/// The configuration structure loaded from a TOML file.
#[derive(Deserialize, Debug)]
struct Config {
    minimum_timeout_ms: u64,
    maximum_timeout_ms: u64,
    activity_timeout_ms: u64,
}

/// Reads and parses configuration from a TOML file.
fn read_config(path: &PathBuf) -> Result<Config, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(path)?;
    let cfg: Config = toml::from_str(&content)?;
    Ok(cfg)
}

fn split_command(command_str: &str) -> (String, Vec<String>) {
    let mut parts = command_str.split_whitespace();
    let executable = parts.next().unwrap_or("").to_string();
    let args = parts.map(|s| s.to_string()).collect();
    (executable, args)
}

fn init_logging(level: &str) {
    std::env::set_var("RUST_LOG", level);
    let _ = env_logger::builder().is_test(true).try_init();
}

fn prompt_for_input(prompt: &str) -> Result<String, Box<dyn std::error::Error>> {
    print!("{}", prompt);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    init_logging("info");
    log::info!("Starting CLI");

    let command_str = if let Some(cmd) = args.command {
        cmd
    } else {
        match prompt_for_input("Enter your Command - ") {
            Ok(cmd) if !cmd.is_empty() => cmd,
            _ => {
                eprintln!("No command provided; exiting.");
                std::process::exit(1);
            }
        }
    };

    let config_path = if let Some(path) = args.config {
        path
    } else {
        match prompt_for_input("Enter your config location - ") {
            Ok(p) if !p.is_empty() => PathBuf::from(p),
            _ => {
                eprintln!("No configuration file provided; exiting.");
                std::process::exit(1);
            }
        }
    };

    let config = match read_config(&config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Error reading config file: {}", e);
            std::process::exit(1);
        }
    };
    log::debug!("Loaded config: {:#?}", config);

    let (executable, args_vec) = split_command(&command_str);
    if executable.is_empty() {
        eprintln!("Empty command provided; exiting.");
        std::process::exit(1);
    }
    log::info!("Command to run: {} with args {:?}", executable, args_vec);

    let mut command = StdCommand::new(executable);
    command.args(args_vec);

    let minimum_timeout = Duration::from_millis(config.minimum_timeout_ms);
    let maximum_timeout = Duration::from_millis(config.maximum_timeout_ms);
    let activity_timeout = Duration::from_millis(config.activity_timeout_ms);

    println!("Running command with timeouts:");
    println!("  Minimum Timeout: {:?}", minimum_timeout);
    println!("  Maximum Timeout: {:?}", maximum_timeout);
    println!("  Activity Timeout: {:?}", activity_timeout);

    match run_command_with_timeout(command, minimum_timeout, maximum_timeout, activity_timeout).await {
        Ok(output) => {
            println!("\n--- Command Output ---");
            println!("Timed Out: {}", output.timed_out);
            println!("Duration: {:?}", output.duration);
            if let Some(status) = output.exit_status {
                println!("Exit Status: {}", status);
                println!("Exit Code: {:?}", status.code());
            } else {
                println!("Exit Status: None (process may have been killed)");
            }
            println!("Stdout:\n{}", String::from_utf8_lossy(&output.stdout));
            println!("Stderr:\n{}", String::from_utf8_lossy(&output.stderr));
        }
        Err(e) => {
            eprintln!("Error running command: {:?}", e);
            std::process::exit(1);
        }
    }
}
