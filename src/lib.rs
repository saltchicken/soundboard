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
