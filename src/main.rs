use soundboard::{AudioCommand, AudioResponse};

use elgato_streamdeck::images::convert_image_with_format;
use elgato_streamdeck::{AsyncStreamDeck, DeviceStateUpdate, list_devices, new_hidapi};
use image::open;
use image::{DynamicImage, Rgb};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

const SOCKET_PATH: &str = "/tmp/rust-audio-monitor.sock";
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_RETRY_INTERVAL: Duration = Duration::from_millis(100);
// const PLAYBACK_SINK_NAME: Option<&str> = Some("MyMixer");
const PLAYBACK_SINK_NAME: Option<&str> = None;
const DELETE_HOLD_DURATION: Duration = Duration::from_secs(2);

async fn play_audio_file(path: &PathBuf) -> io::Result<()> {
    let player = "pw-play";
    println!(
        "Attempting to play file with '{}': {}",
        player,
        path.display()
    );
    // Create the command
    let mut cmd = Command::new(player);
    if let Some(sink_name) = PLAYBACK_SINK_NAME {
        cmd.arg("--target");
        cmd.arg(sink_name);
        println!("...routing playback to sink: {}", sink_name);
    } else {
        println!("...routing playback to default output.");
    }
    cmd.arg(path);
    // Run the command and wait for its status
    // This runs in a spawned tokio task, so it won't block the UI
    let status = cmd.status().await?;
    if status.success() {
        println!("Playback successful.");
        Ok(())
    } else {
        // This will catch errors like "pw-play: command not found"
        let msg = format!(
            "Playback command '{}' failed with status: {}",
            player, status
        );
        eprintln!("{}", msg);
        Err(io::Error::other(msg))
    }
}

// TODO: Implement this
fn get_socket_path() -> std::io::Result<PathBuf> {
    match dirs::runtime_dir() {
        Some(mut path) => {
            path.push("soundboard.sock");
            Ok(path)
        }
        None => Err(std::io::Error::other("Could not find runtime directory")),
    }
}

// TODO: Implement this
fn get_audio_storage_path() -> std::io::Result<PathBuf> {
    match dirs::audio_dir() {
        Some(mut path) => {
            path.push("soundboard-recordings");
            std::fs::create_dir_all(&path)?;
            Ok(path)
        }
        None => Err(std::io::Error::other("Could not find audio directory")),
    }
}

async fn send_audio_command(command: &AudioCommand) -> io::Result<AudioResponse> {
    let stream = match UnixStream::connect(SOCKET_PATH).await {
        Ok(stream) => stream,
        Err(e) => {
            let msg = format!("Failed to connect to socket {}: {}", SOCKET_PATH, e);
            eprintln!("{}", msg);
            return Err(io::Error::new(io::ErrorKind::ConnectionRefused, msg));
        }
    };

    let (reader, writer) = stream.into_split();
    let mut buf_writer = tokio::io::BufWriter::new(writer);
    let mut buf_reader = BufReader::new(reader);

    let cmd_json = match serde_json::to_string(command) {
        Ok(json) => json + "\n", // Add newline as delimiter
        Err(e) => {
            return Err(io::Error::other(format!(
                "Failed to serialize command: {}",
                e
            )));
        }
    };

    if let Err(e) = buf_writer.write_all(cmd_json.as_bytes()).await {
        eprintln!("Failed to write command: {}", e);
        return Err(e);
    }
    if let Err(e) = buf_writer.flush().await {
        eprintln!("Failed to flush command: {}", e);
        return Err(e);
    }
    if let Err(e) = buf_writer.shutdown().await {
        eprintln!("Failed to shutdown writer: {}", e);
        return Err(e);
    }

    let mut response_line = String::new();
    if let Err(e) = buf_reader.read_line(&mut response_line).await {
        eprintln!("Failed to read response: {}", e);
        return Err(e);
    }

    if response_line.is_empty() {
        let msg = "Server sent an empty response.";
        eprintln!("{}", msg);
        return Err(io::Error::other(msg));
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

fn create_fallback_image(color: Rgb<u8>) -> DynamicImage {
    DynamicImage::ImageRgb8(image::RgbImage::from_fn(72, 72, move |_, _| color))
}

fn create_fallback_lcd_image(color: Rgb<u8>) -> DynamicImage {
    DynamicImage::ImageRgb8(image::RgbImage::from_fn(800, 100, move |_, _| color))
}

fn start_pipewire_source() -> Result<tokio::process::Child, std::io::Error> {
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

async fn wait_for_server() -> io::Result<()> {
    let start = tokio::time::Instant::now();
    println!("Waiting for server socket at {}...", SOCKET_PATH);
    loop {
        // Check for timeout
        if start.elapsed() > SERVER_START_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Server failed to start within timeout.",
            ));
        }
        // Try to connect
        match UnixStream::connect(SOCKET_PATH).await {
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

#[tokio::main]
async fn main() {
    let mut server_process = start_pipewire_source().unwrap();
    if let Err(e) = wait_for_server().await {
        eprintln!(
            "Failed to connect to pipewire_source server: {}. Is it already running?",
            e
        );
        eprintln!("Ensure '{SOCKET_PATH}' is writable and the server can start.");
        let _ = server_process.kill().await; // Kill the child process
        return; // Exit
    }

    // Spawn a task to monitor the server process
    let server_pid = server_process.id().unwrap_or(0);
    tokio::spawn(async move {
        match server_process.wait().await {
            Ok(status) => {
                eprintln!(
                    "Audio server (PID: {}) exited with status: {}",
                    server_pid, status
                );
            }
            Err(e) => {
                eprintln!(
                    "Failed to wait on audio server (PID: {}): {}",
                    server_pid, e
                );
            }
        }
    });

    let img_rec_off =
        open("assets/rec_off.png").unwrap_or_else(|_| create_fallback_image(Rgb([80, 80, 80])));
    let img_rec_on =
        open("assets/rec_on.png").unwrap_or_else(|_| create_fallback_image(Rgb([255, 0, 0])));
    let img_play =
        open("assets/play.png").unwrap_or_else(|_| create_fallback_image(Rgb([0, 255, 0])));
    let img_lcd_strip = open("assets/lcd_strip.png")
        .unwrap_or_else(|_| create_fallback_lcd_image(Rgb([20, 200, 20])));
    match new_hidapi() {
        Ok(hid) => {
            for (kind, serial) in list_devices(&hid) {
                println!(
                    "Found Stream Deck: {:?} {} {}",
                    kind,
                    serial,
                    kind.product_id()
                );
                let device =
                    AsyncStreamDeck::connect(&hid, kind, &serial).expect("Failed to connect");
                device.set_brightness(50).await.unwrap();
                device.clear_all_button_images().await.unwrap();
                println!("Setting LCD touch strip image...");
                if let Some(format) = device.kind().lcd_image_format() {
                    let scaled_image = img_lcd_strip.clone().resize_to_fill(
                        format.size.0 as u32,
                        format.size.1 as u32,
                        image::imageops::FilterType::Nearest,
                    );
                    let converted_image = convert_image_with_format(format, scaled_image).unwrap();
                    let _ = device.write_lcd_fill(&converted_image).await;
                } else {
                    eprintln!("Failed to set LCD image (is this a Stream Deck Plus?)",);
                }
                let mut button_files: HashMap<u8, PathBuf> = HashMap::new();
                for i in 0..8 {
                    let file_name = format!("/tmp/recording_{}.wav", (b'A' + i) as char);
                    button_files.insert(i, PathBuf::from(file_name));
                }
                let mut active_recording_key: Option<u8> = None;
                let mut pending_delete: HashMap<u8, Instant> = HashMap::new();
                for (key, path) in &button_files {
                    let initial_image = if path.exists() {
                        img_play.clone()
                    } else {
                        img_rec_off.clone()
                    };
                    device.set_button_image(*key, initial_image).await.unwrap();
                }
                device.flush().await.unwrap();
                let reader = device.get_reader();
                'infinite: loop {
                    let updates = match reader.read(100.0).await {
                        Ok(updates) => updates,
                        Err(_) => break,
                    };
                    for update in updates {
                        match update {
                            DeviceStateUpdate::ButtonDown(key) => {
                                if let Some(path) = button_files.get(&key) {
                                    if path.exists() {
                                        println!(
                                            "Button {} down (file exists). Holding for delete...",
                                            key
                                        );
                                        pending_delete.insert(key, Instant::now());
                                        device
                                            .set_button_image(key, img_rec_on.clone())
                                            .await
                                            .unwrap();
                                        device.flush().await.unwrap();
                                    } else {
                                        println!(
                                            "Button {} down (no file). Checking status...",
                                            key
                                        );
                                        match send_audio_command(&AudioCommand::Status).await {
                                            Ok(AudioResponse::Status(status)) => {
                                                if status.contains("Listening") {
                                                    println!(
                                                        "...Audio monitor is Listening. Sending START."
                                                    );
                                                    let cmd = AudioCommand::Start(path.clone());
                                                    match send_audio_command(&cmd).await {
                                                        Ok(AudioResponse::Ok) => {
                                                            active_recording_key = Some(key);
                                                            device
                                                                .set_button_image(
                                                                    key,
                                                                    img_rec_on.clone(),
                                                                )
                                                                .await
                                                                .unwrap();
                                                            device.flush().await.unwrap();
                                                            println!("...STARTED");
                                                        }
                                                        Ok(other) => eprintln!(
                                                            "Unexpected START response: {:?}",
                                                            other
                                                        ),
                                                        Err(e) => {
                                                            eprintln!("Failed to send START: {}", e)
                                                        }
                                                    }
                                                } else {
                                                    println!(
                                                        "...Audio monitor is NOT Listening (Status: {}).",
                                                        status
                                                    );
                                                }
                                            }
                                            Ok(other) => {
                                                eprintln!("Unexpected STATUS response: {:?}", other)
                                            }
                                            Err(e) => {
                                                eprintln!("Failed to get STATUS: {}.", e)
                                            }
                                        }
                                    }
                                }
                            }
                            DeviceStateUpdate::ButtonUp(key) => {
                                if key == device.kind().key_count() - 1 {
                                    println!("Exit button pressed. Shutting down.");
                                    break 'infinite;
                                }
                                if active_recording_key == Some(key) {
                                    println!("Button {} up, (was recording), sending STOP", key);
                                    match send_audio_command(&AudioCommand::Stop).await {
                                        Ok(AudioResponse::Ok) => {
                                            active_recording_key = None;
                                            device
                                                .set_button_image(key, img_play.clone())
                                                .await
                                                .unwrap();
                                            println!("...STOPPED. File saved.");
                                        }
                                        Ok(other) => {
                                            eprintln!("Unexpected STOP response: {:?}", other)
                                        }
                                        Err(e) => {
                                            eprintln!("Failed to send STOP: {}", e);
                                        }
                                    }
                                    device.flush().await.unwrap();
                                } else if let Some(start_time) = pending_delete.remove(&key) {
                                    let hold_duration = start_time.elapsed();
                                    println!(
                                        "Button {} up (was pending delete). Held for {:?}",
                                        key, hold_duration
                                    );
                                    if hold_duration >= DELETE_HOLD_DURATION {
                                        // Held for > 2s: Delete the file
                                        if let Some(path) = button_files.get(&key) {
                                            match fs::remove_file(path) {
                                                Ok(_) => {
                                                    println!("...File {} deleted.", path.display());
                                                    device
                                                        .set_button_image(key, img_rec_off.clone())
                                                        .await
                                                        .unwrap();
                                                }
                                                Err(e) => {
                                                    eprintln!(
                                                        "...Failed to delete file {}: {}",
                                                        path.display(),
                                                        e
                                                    );
                                                    device
                                                        .set_button_image(key, img_play.clone())
                                                        .await
                                                        .unwrap();
                                                }
                                            }
                                        }
                                    } else {
                                        // Held for < 2s: Play the file
                                        println!("...Hold < 2s. Triggering playback.");
                                        if let Some(path) = button_files.get(&key) {
                                            // Spawn playback in a new task
                                            // so it doesn't block our event loop
                                            let path_clone = path.clone();
                                            tokio::spawn(async move {
                                                if let Err(e) = play_audio_file(&path_clone).await {
                                                    eprintln!("Playback failed: {}", e);
                                                }
                                            });
                                        }
                                        // Set image back to "play"
                                        device
                                            .set_button_image(key, img_play.clone())
                                            .await
                                            .unwrap();
                                    }
                                    device.flush().await.unwrap();
                                }
                            }
                            _ => {}
                        }
                    }
                }
                drop(reader);
                println!("Cleaning up buttons...");
                device.clear_all_button_images().await.unwrap();
                device.flush().await.unwrap();
            }
        }
        Err(e) => eprintln!("Failed to create HidApi instance: {}", e),
    }
    println!("Main function exiting. Ensuring server is killed.");
    // TODO: The server process is now owned by the monitor task,
    // so we can't kill it here. We should send a shutdown signal.
    // For now, we'll just let the OS clean it up when main exits.
    // A better solution would be a tokio::sync::watch channel to signal shutdown.
}
