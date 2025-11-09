use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
