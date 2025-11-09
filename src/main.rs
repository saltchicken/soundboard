use elgato_streamdeck::images::convert_image_with_format;
use elgato_streamdeck::{AsyncStreamDeck, DeviceStateUpdate, list_devices, new_hidapi};
use image::open;
use image::{DynamicImage, Rgb};
use soundboard::{AudioCommand, AudioResponse, get_audio_storage_path, get_socket_path};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::watch;

const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_RETRY_INTERVAL: Duration = Duration::from_millis(100);
const DELETE_HOLD_DURATION: Duration = Duration::from_secs(2);

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Mode {
    Playback,
    Edit,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum PlaybackSink {
    Default,
    Mixer,
    Both,
}

async fn play_audio_file(path: &PathBuf, sink_target: PlaybackSink) -> io::Result<()> {
    let player = "pw-play";
    println!(
        "Attempting to play file with '{}': {}",
        player,
        path.display()
    );

    let mut cmd_default = Command::new(player);
    cmd_default.arg(path);
    cmd_default.stdout(Stdio::null()).stderr(Stdio::null());

    let mut cmd_mixer = Command::new(player);
    cmd_mixer.arg("--target");
    cmd_mixer.arg("MyMixer");
    cmd_mixer.arg(path);
    cmd_mixer.stdout(Stdio::null()).stderr(Stdio::null());

    match sink_target {
        PlaybackSink::Default => {
            println!("...routing playback to Default.");
            let status = cmd_default.status().await?;
            if !status.success() {
                let msg = format!("Playback command (Default) failed with status: {}", status);
                eprintln!("{}", msg);
                return Err(io::Error::other(msg));
            }
        }
        PlaybackSink::Mixer => {
            println!("...routing playback to sink: MyMixer");
            let status = cmd_mixer.status().await?;
            if !status.success() {
                let msg = format!("Playback command (MyMixer) failed with status: {}", status);
                eprintln!("{}", msg);
                return Err(io::Error::other(msg));
            }
        }
        PlaybackSink::Both => {
            println!("...routing playback to BOTH Default and MyMixer.");
            // Spawn both commands concurrently
            let default_handle = tokio::spawn(async move { cmd_default.status().await });
            let mixer_handle = tokio::spawn(async move { cmd_mixer.status().await });

            // Await both handles
            match tokio::try_join!(default_handle, mixer_handle) {
                Ok((Ok(status_default), Ok(status_mixer))) => {
                    if !status_default.success() {
                        eprintln!("Playback (Default) failed with status: {}", status_default);
                    }
                    if !status_mixer.success() {
                        eprintln!("Playback (MyMixer) failed with status: {}", status_mixer);
                    }
                    if !status_default.success() || !status_mixer.success() {
                        return Err(io::Error::other("One or more playback commands failed."));
                    }
                }
                Ok((Err(e), _)) | Ok((_, Err(e))) => {
                    let msg = format!("Failed to get command status: {}", e);
                    eprintln!("{}", msg);
                    return Err(io::Error::other(msg));
                }
                Err(e) => {
                    let msg = format!("Task join failed: {}", e);
                    eprintln!("{}", msg);
                    return Err(io::Error::other(msg));
                }
            }
        }
    }

    println!("Playback successful for sink: {:?}", sink_target);
    Ok(())
}

async fn send_audio_command(
    socket_path: &Path,
    command: &AudioCommand,
) -> io::Result<AudioResponse> {
    // ... (This function is unchanged)
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

async fn update_lcd_mode(
    device: &AsyncStreamDeck,
    mode: Mode,
    img_playback: &DynamicImage,
    img_edit: &DynamicImage,
) {
    // ... (This function is unchanged)
    println!("Setting LCD mode to: {:?}", mode);
    let img_to_use = match mode {
        Mode::Playback => img_playback,
        Mode::Edit => img_edit,
    };
    if let Some(format) = device.kind().lcd_image_format() {
        let scaled_image = img_to_use.clone().resize_to_fill(
            format.size.0 as u32,
            format.size.1 as u32,
            image::imageops::FilterType::Nearest,
        );
        let converted_image = convert_image_with_format(format, scaled_image).unwrap();
        let _ = device.write_lcd_fill(&converted_image).await;
    } else {
        eprintln!("Failed to set LCD image (is this a Stream Deck Plus?)");
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

async fn wait_for_server(socket_path: &Path) -> io::Result<()> {
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

#[tokio::main]
async fn main() {
    let socket_path = match get_socket_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("Failed to get socket path: {}", e);
            return;
        }
    };
    let audio_storage_path = match get_audio_storage_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("Failed to get audio storage path: {}", e);
            return;
        }
    };
    let (shutdown_tx, mut shutdown_rx) = watch::channel(());
    let mut server_process = start_pipewire_source().unwrap();
    if let Err(e) = wait_for_server(&socket_path).await {
        eprintln!(
            "Failed to connect to pipewire_source server: {}. Is it already running?",
            e
        );
        eprintln!(
            "Ensure '{}' is writable and the server can start.",
            socket_path.display()
        );
        let _ = server_process.kill().await; // Kill the child process
        return; // Exit
    }
    // Spawn a task to monitor the server process
    let server_pid = server_process.id().unwrap_or(0);
    tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = shutdown_rx.changed() => {
                eprintln!("Main task requested shutdown. Killing server (PID: {})...", server_pid);
                if let Err(e) = server_process.kill().await {
                    eprintln!("Failed to kill server process: {}", e);
                }
            }
            status = server_process.wait() => {
                match status {
                    Ok(status) => {
                        eprintln!(
                            "Audio server (PID: {}) exited on its own with status: {}",
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
            }
        }
    });
    let img_rec_off =
        open("assets/rec_off.png").unwrap_or_else(|_| create_fallback_image(Rgb([80, 80, 80])));
    let img_rec_on =
        open("assets/rec_on.png").unwrap_or_else(|_| create_fallback_image(Rgb([255, 0, 0])));
    let img_play =
        open("assets/play.png").unwrap_or_else(|_| create_fallback_image(Rgb([0, 255, 0])));

    let img_lcd_playback = open("assets/lcd_strip.png")
        .unwrap_or_else(|_| create_fallback_lcd_image(Rgb([20, 200, 20])));
    let img_lcd_edit = open("assets/lcd_edit.png")
        .unwrap_or_else(|_| create_fallback_lcd_image(Rgb([200, 20, 20])));
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

                let mut mode = Mode::Playback;
                let mut playback_sink: PlaybackSink = PlaybackSink::Default;
                println!("Starting in {:?} mode.", mode);
                println!("Playback sink set to: {:?}", playback_sink);

                update_lcd_mode(&device, mode, &img_lcd_playback, &img_lcd_edit).await;
                let mut button_files: HashMap<u8, PathBuf> = HashMap::new();
                for i in 0..8 {
                    let file_name = format!("recording_{}.wav", (b'A' + i) as char);
                    let mut file_path = audio_storage_path.clone();
                    file_path.push(file_name);
                    button_files.insert(i, file_path);
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
                loop {
                    let updates = match reader.read(100.0).await {
                        Ok(updates) => updates,
                        Err(_) => break,
                    };
                    for update in updates {
                        match update {
                            DeviceStateUpdate::EncoderTwist(dial, _ticks) => {
                                if dial == 0 {
                                    mode = match mode {
                                        Mode::Playback => Mode::Edit,
                                        Mode::Edit => Mode::Playback,
                                    };
                                    println!("Mode switched to: {:?}", mode);
                                    // Update the LCD strip to reflect the new mode
                                    update_lcd_mode(
                                        &device,
                                        mode,
                                        &img_lcd_playback,
                                        &img_lcd_edit,
                                    )
                                    .await;
                                    device.flush().await.unwrap();
                                }
                            }
                            DeviceStateUpdate::EncoderDown(dial) => {
                                if dial == 0 {
                                    playback_sink = match playback_sink {
                                        PlaybackSink::Default => PlaybackSink::Mixer,
                                        PlaybackSink::Mixer => PlaybackSink::Both,
                                        PlaybackSink::Both => PlaybackSink::Default,
                                    };
                                    println!("Playback sink set to: {:?}", playback_sink);
                                }
                            }
                            DeviceStateUpdate::ButtonDown(key) => {
                                match mode {
                                    Mode::Playback => {
                                        // In Playback mode, just show a "pressed" state
                                        if let Some(path) = button_files.get(&key)
                                            && path.exists()
                                        {
                                            // Set to 'rec_on' as a "pressed" state
                                            device
                                                .set_button_image(key, img_rec_on.clone())
                                                .await
                                                .unwrap();
                                            device.flush().await.unwrap();
                                        }
                                    }
                                    Mode::Edit => {
                                        // In Edit mode, this is the original record/delete-hold logic
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
                                                match send_audio_command(
                                                    &socket_path,
                                                    &AudioCommand::Status,
                                                )
                                                .await
                                                {
                                                    Ok(AudioResponse::Status(status)) => {
                                                        if status.contains("Listening") {
                                                            println!(
                                                                "...Audio monitor is Listening. Sending START."
                                                            );
                                                            let cmd =
                                                                AudioCommand::Start(path.clone());
                                                            match send_audio_command(
                                                                &socket_path,
                                                                &cmd,
                                                            )
                                                            .await
                                                            {
                                                                Ok(AudioResponse::Ok) => {
                                                                    active_recording_key =
                                                                        Some(key);
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
                                                                    eprintln!(
                                                                        "Failed to send START: {}",
                                                                        e
                                                                    )
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
                                                        eprintln!(
                                                            "Unexpected STATUS response: {:?}",
                                                            other
                                                        )
                                                    }
                                                    Err(e) => {
                                                        eprintln!("Failed to get STATUS: {}.", e)
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            DeviceStateUpdate::ButtonUp(key) => {
                                match mode {
                                    Mode::Playback => {
                                        // In Playback mode, ButtonUp triggers playback
                                        if let Some(path) = button_files.get(&key)
                                            && path.exists()
                                        {
                                            println!(
                                                "Button {} up (Playback Mode). Triggering playback.",
                                                key
                                            );

                                            // Spawn playback in a new task
                                            let path_clone = path.clone();
                                            let sink_clone = playback_sink;
                                            tokio::spawn(async move {
                                                if let Err(e) =
                                                    play_audio_file(&path_clone, sink_clone).await
                                                {
                                                    eprintln!("Playback failed: {}", e);
                                                }
                                            });

                                            // Set image back to "play"
                                            device
                                                .set_button_image(key, img_play.clone())
                                                .await
                                                .unwrap();
                                            device.flush().await.unwrap();
                                        }
                                    }
                                    Mode::Edit => {
                                        // In Edit mode, this is the original stop-record/delete-commit logic
                                        if active_recording_key == Some(key) {
                                            println!(
                                                "Button {} up, (was recording), sending STOP",
                                                key
                                            );
                                            match send_audio_command(
                                                &socket_path,
                                                &AudioCommand::Stop,
                                            )
                                            .await
                                            {
                                                Ok(AudioResponse::Ok) => {
                                                    active_recording_key = None;
                                                    device
                                                        .set_button_image(key, img_play.clone())
                                                        .await
                                                        .unwrap();
                                                    println!("...STOPPED. File saved.");
                                                }
                                                Ok(other) => {
                                                    eprintln!(
                                                        "Unexpected STOP response: {:?}",
                                                        other
                                                    )
                                                }
                                                Err(e) => {
                                                    eprintln!("Failed to send STOP: {}", e);
                                                }
                                            }
                                            device.flush().await.unwrap();
                                        } else if let Some(start_time) = pending_delete.remove(&key)
                                        {
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
                                                            println!(
                                                                "...File {} deleted.",
                                                                path.display()
                                                            );
                                                            device
                                                                .set_button_image(
                                                                    key,
                                                                    img_rec_off.clone(),
                                                                )
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
                                                                .set_button_image(
                                                                    key,
                                                                    img_play.clone(),
                                                                )
                                                                .await
                                                                .unwrap();
                                                        }
                                                    }
                                                }
                                            } else {
                                                println!("...Hold < 2s. (Edit Mode) No action.");
                                                if let Some(path) = button_files.get(&key)
                                                    && path.exists()
                                                {
                                                    // Set image back to "play"
                                                    device
                                                        .set_button_image(key, img_play.clone())
                                                        .await
                                                        .unwrap();
                                                }
                                            }
                                            device.flush().await.unwrap();
                                        }
                                    }
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
    if let Err(e) = shutdown_tx.send(()) {
        eprintln!("Failed to send shutdown signal: {}", e);
    }
    // Clean up the socket file
    if let Err(e) = fs::remove_file(&socket_path)
        && e.kind() != io::ErrorKind::NotFound
    {
        eprintln!(
            "Failed to remove socket file {}: {}",
            socket_path.display(),
            e
        );
    }
}
