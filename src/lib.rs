use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use std::env;
use tokio::process::{Child, Command};

use std::time::Duration;

const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_RETRY_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Serialize, Deserialize, Debug)]
pub enum AudioCommand {
    Start(PathBuf),
    Stop,
    Status,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum AudioResponse {
    Status(String),
    Ok,
    Error(String),
}

pub fn get_socket_path() -> std::io::Result<PathBuf> {
    match dirs::runtime_dir() {
        Some(mut path) => {
            path.push("soundboard.sock");
            Ok(path)
        }
        None => Err(std::io::Error::other("Could not find runtime directory")),
    }
}

pub fn start_pipewire_source() -> Result<tokio::process::Child, std::io::Error> {
    // 1. Find the path to our own executable
    let mut server_exe_path = match env::current_exe() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("Failed to get current executable path: {}", e);
            return Err(e);
        }
    };
    server_exe_path.pop();
    server_exe_path.push("pipewire_source");
    println!(
        "Attempting to spawn server at: {}",
        server_exe_path.display()
    );
    let server_process: Child = Command::new(&server_exe_path)
        .stdout(Stdio::null()) // Silences the server's stdout
        .stderr(Stdio::null()) // Silences the server's stderr
        .spawn()
        .expect("Failed to spawn pipewire_source server. Did you `cargo build` first?");
    let server_pid = server_process.id().unwrap_or(0);
    println!(
        "Spawned server process (PID: {}). Waiting for it to initialize...",
        server_pid
    );
    Ok(server_process)
}

pub async fn wait_for_server(socket_path: &Path) -> io::Result<()> {
    let start = tokio::time::Instant::now();
    println!("Waiting for server socket at {}...", socket_path.display());
    loop {
        // Check for timeout
        if start.elapsed() > SERVER_START_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Server failed to start within timeout.",
            ));
        }
        // Try to connect
        match UnixStream::connect(socket_path).await {
            Ok(_) => {
                // Connection successful, socket exists.
                println!("...Server socket found!");
                return Ok(()); // Success
            }
            Err(_) => {
                // Socket not ready, wait and retry.
            }
        }
        // Wait before retrying
        tokio::time::sleep(SERVER_RETRY_INTERVAL).await;
    }
}

pub fn get_audio_storage_path() -> std::io::Result<PathBuf> {
    match dirs::audio_dir() {
        Some(mut path) => {
            path.push("soundboard-recordings");
            std::fs::create_dir_all(&path)?;
            Ok(path)
        }
        None => Err(std::io::Error::other("Could not find audio directory")),
    }
}

pub async fn send_audio_command(
    socket_path: &std::path::Path,
    command: &AudioCommand,
) -> io::Result<AudioResponse> {
    let stream = match UnixStream::connect(socket_path).await {
        Ok(stream) => stream,
        Err(e) => {
            let msg = format!(
                "Failed to connect to socket {}: {}",
                socket_path.display(),
                e
            );
            eprintln!("{}", msg);
            return Err(io::Error::new(io::ErrorKind::ConnectionRefused, msg));
        }
    };

    let (reader, writer) = stream.into_split();
    let mut buf_writer = tokio::io::BufWriter::new(writer);
    let mut buf_reader = BufReader::new(reader);

    let cmd_json = match serde_json::to_string(command) {
        Ok(json) => json + "\n",
        Err(e) => {
            return Err(io::Error::other(format!(
                "Failed to serialize command: {}",
                e
            )));
        }
    };

    buf_writer.write_all(cmd_json.as_bytes()).await?;
    buf_writer.flush().await?;
    buf_writer.shutdown().await?;

    let mut response_line = String::new();
    buf_reader.read_line(&mut response_line).await?;

    if response_line.is_empty() {
        return Err(io::Error::other("Server sent an empty response."));
    }

    match serde_json::from_str::<AudioResponse>(&response_line) {
        Ok(response) => Ok(response),
        Err(e) => {
            let msg = format!(
                "Failed to parse server response ('{}'): {}",
                response_line.trim(),
                e
            );
            eprintln!("{}", msg);
            Err(io::Error::other(msg))
        }
    }
}
