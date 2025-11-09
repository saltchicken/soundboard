use soundboard::{
    AudioCommand, AudioResponse, get_audio_storage_path, get_socket_path, send_audio_command,
    start_pipewire_source, wait_for_server,
};
mod audio_player;
use crate::audio_player::{PlaybackSink, play_audio_file};
mod lcd;
use crate::lcd::{create_fallback_image, create_fallback_lcd_image, update_lcd_mode};
use elgato_streamdeck::{AsyncStreamDeck, DeviceStateUpdate, list_devices, new_hidapi};
use image::Rgb;
use image::open;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::watch;
const DELETE_HOLD_DURATION: Duration = Duration::from_secs(2);
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Mode {
    Playback,
    Edit,
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
        .unwrap_or_else(|_| create_fallback_lcd_image(Rgb([10, 50, 10])));
    let img_lcd_edit = open("assets/lcd_edit.png")
        .unwrap_or_else(|_| create_fallback_lcd_image(Rgb([50, 10, 10])));
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

                                        if let Some(path) = button_files.get(&key) {
                                            if path.exists() {

                                                device
                                                    .set_button_image(key, img_rec_on.clone())
                                                    .await
                                                    .unwrap();
                                                device.flush().await.unwrap();
                                            } else {

                                                println!(
                                                    "Button {} down (Playback Mode, no file). Checking status...",
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
                                    Mode::Edit => {

                                        if let Some(path) = button_files.get(&key) {
                                            if path.exists() {
                                                println!(
                                                    "Button {} down (Edit Mode, file exists). Holding for delete...",
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
                                                    "Button {} down (Edit Mode, no file). No action.",
                                                    key
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                            DeviceStateUpdate::ButtonUp(key) => {
                                match mode {
                                    Mode::Playback => {

                                        if active_recording_key == Some(key) {

                                            println!(
                                                "Button {} up (Playback Mode, was recording), sending STOP",
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
                                        } else {

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
                                                        play_audio_file(&path_clone, sink_clone)
                                                            .await
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
                                    }
                                    Mode::Edit => {


                                        if let Some(start_time) = pending_delete.remove(&key) {
                                            let hold_duration = start_time.elapsed();
                                            println!(
                                                "Button {} up (Edit Mode, was pending delete). Held for {:?}",
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