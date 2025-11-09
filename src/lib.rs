use serde::{Deserialize, Serialize};
use std::io;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

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
