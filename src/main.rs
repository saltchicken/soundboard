use soundboard::{
    AudioCommand, AudioResponse, get_audio_storage_path, get_socket_path, send_audio_command,
    start_pipewire_source, wait_for_server,
};
mod audio_player;
use crate::audio_player::{PlaybackSink, play_audio_file};
mod lcd;
use crate::lcd::{create_fallback_image, create_fallback_lcd_image, update_lcd_mode};
use elgato_streamdeck::{AsyncStreamDeck, DeviceStateUpdate, list_devices, new_hidapi};

use image::open;
use image::{DynamicImage, Rgb};
use std::collections::HashMap;
use std::fs;
use std::io;

use std::path::{Path, PathBuf};
use tokio::sync::watch;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Mode {
    Playback,
    Edit,
}




struct AppState {
    mode: Mode,
    playback_sink: PlaybackSink,
    button_files: HashMap<u8, PathBuf>,
    active_recording_key: Option<u8>,
    selected_for_delete: Option<u8>,

    img_rec_off: DynamicImage,
    img_rec_on: DynamicImage,
    img_play: DynamicImage,
    img_lcd_playback: DynamicImage,
    img_lcd_edit: DynamicImage,
}




impl AppState {

    async fn handle_encoder_twist(&mut self, dial: u8, device: &AsyncStreamDeck) {
        if dial == 0 {
            self.mode = match self.mode {
                Mode::Playback => Mode::Edit,
                Mode::Edit => Mode::Playback,
            };
            println!("Mode switched to: {:?}", self.mode);
            if self.mode == Mode::Playback {
                if let Some(selected_key) = self.selected_for_delete.take() {
                    println!(
                        "Mode switched away from Edit. Deselecting key {}.",
                        selected_key
                    );
                    // Reset the button's image
                    if let Some(path) = self.button_files.get(&selected_key) {
                        let img = if path.exists() {
                            self.img_play.clone()
                        } else {
                            self.img_rec_off.clone()
                        };
                        device.set_button_image(selected_key, img).await.unwrap();
                    }
                }
            }
            // Update the LCD strip to reflect the new mode
            update_lcd_mode(
                device,
                self.mode,
                &self.img_lcd_playback,
                &self.img_lcd_edit,
            )
            .await;
            device.flush().await.unwrap();
        }
    }


    async fn handle_encoder_down(&mut self, dial: u8, device: &AsyncStreamDeck) {
        if dial == 0 {
            self.playback_sink = match self.playback_sink {
                PlaybackSink::Default => PlaybackSink::Mixer,
                PlaybackSink::Mixer => PlaybackSink::Both,
                PlaybackSink::Both => PlaybackSink::Default,
            };
            println!("Playback sink set to: {:?}", self.playback_sink);
        } else if dial == 3 {
            if self.mode == Mode::Edit {
                if let Some(key_to_delete) = self.selected_for_delete.take() {
                    println!(
                        "Encoder 3 pressed in Edit mode. Deleting selected key: {}",
                        key_to_delete
                    );
                    if let Some(path) = self.button_files.get(&key_to_delete) {
                        match fs::remove_file(path) {
                            Ok(_) => {
                                println!("...File {} deleted.", path.display());
                                device
                                    .set_button_image(key_to_delete, self.img_rec_off.clone())
                                    .await
                                    .unwrap();
                            }
                            Err(e) => {
                                eprintln!("...Failed to delete file {}: {}", path.display(), e);
                                // Set image back to 'play' even if delete failed
                                device
                                    .set_button_image(key_to_delete, self.img_play.clone())
                                    .await
                                    .unwrap();
                            }
                        }
                        device.flush().await.unwrap();
                    }
                } else {
                    println!("Encoder 3 pressed in Edit mode, but no sample is selected.");
                }
            } else {
                println!("Encoder 3 pressed (not in Edit mode). No action.");
            }
        }
    }


    async fn handle_button_down(&mut self, key: u8, device: &AsyncStreamDeck, socket_path: &Path) {
        match self.mode {
            Mode::Playback => {
                if let Some(path) = self.button_files.get(&key) {
                    if path.exists() {
                        device
                            .set_button_image(key, self.img_rec_on.clone())
                            .await
                            .unwrap();
                        device.flush().await.unwrap();
                    } else {
                        println!(
                            "Button {} down (Playback Mode, no file). Checking status...",
                            key
                        );
                        match send_audio_command(socket_path, &AudioCommand::Status).await {
                            Ok(AudioResponse::Status(status)) => {
                                if status.contains("Listening") {
                                    println!("...Audio monitor is Listening. Sending START.");
                                    let cmd = AudioCommand::Start(path.clone());
                                    match send_audio_command(socket_path, &cmd).await {
                                        Ok(AudioResponse::Ok) => {
                                            self.active_recording_key = Some(key);
                                            device
                                                .set_button_image(key, self.img_rec_on.clone())
                                                .await
                                                .unwrap();
                                            device.flush().await.unwrap();
                                            println!("...STARTED");
                                        }
                                        Ok(other) => {
                                            eprintln!("Unexpected START response: {:?}", other)
                                        }
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
            Mode::Edit => {
                if let Some(path) = self.button_files.get(&key) {
                    if path.exists() {
                        if let Some(prev_selected_key) = self.selected_for_delete {
                            // A key is already selected
                            if prev_selected_key == key {
                                // This key was already selected. Toggle it OFF.
                                println!("Button {} down (Edit Mode). Deselecting {}.", key, key);
                                device
                                    .set_button_image(key, self.img_play.clone())
                                    .await
                                    .unwrap();
                                self.selected_for_delete = None;
                            } else {
                                // A different key was selected. Deselect old, select new.
                                println!(
                                    "Button {} down (Edit Mode). Deselecting old key {}.",
                                    key, prev_selected_key
                                );
                                device
                                    .set_button_image(prev_selected_key, self.img_play.clone())
                                    .await
                                    .unwrap();
                                println!("...Selecting new key {}.", key);
                                device
                                    .set_button_image(key, self.img_rec_on.clone())
                                    .await
                                    .unwrap();
                                self.selected_for_delete = Some(key);
                            }
                        } else {
                            // Nothing was selected. Select this key.
                            println!(
                                "Button {} down (Edit Mode). Selecting key {} for deletion.",
                                key, key
                            );
                            device
                                .set_button_image(key, self.img_rec_on.clone())
                                .await
                                .unwrap();
                            self.selected_for_delete = Some(key);
                        }
                        device.flush().await.unwrap();
                    } else {
                        println!("Button {} down (Edit Mode, no file). No action.", key);
                    }
                }
            }
        }
    }


    async fn handle_button_up(&mut self, key: u8, device: &AsyncStreamDeck, socket_path: &Path) {
        match self.mode {
            Mode::Playback => {
                if self.active_recording_key == Some(key) {
                    println!(
                        "Button {} up (Playback Mode, was recording), sending STOP",
                        key
                    );
                    match send_audio_command(socket_path, &AudioCommand::Stop).await {
                        Ok(AudioResponse::Ok) => {
                            self.active_recording_key = None;
                            device
                                .set_button_image(key, self.img_play.clone())
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
                } else if let Some(path) = self.button_files.get(&key) {
                    if path.exists() {
                        println!("Button {} up (Playback Mode). Triggering playback.", key);
                        // Spawn playback in a new task
                        let path_clone = path.clone();

                        let sink_clone = self.playback_sink;
                        tokio::spawn(async move {
                            if let Err(e) = play_audio_file(&path_clone, sink_clone).await {
                                eprintln!("Playback failed: {}", e);
                            }
                        });
                        // Set image back to "play"
                        device
                            .set_button_image(key, self.img_play.clone())
                            .await
                            .unwrap();
                        device.flush().await.unwrap();
                    }
                }
            }
            Mode::Edit => {
                // ButtonUp does nothing in Edit mode now.
                // Selection is handled on ButtonDown.
                // Deletion is handled by Encoder 3.
            }
        }
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




                let mut app_state = AppState {
                    mode: Mode::Playback,
                    playback_sink: PlaybackSink::Default,
                    button_files: HashMap::new(),
                    active_recording_key: None,
                    selected_for_delete: None,

                    img_rec_off: img_rec_off.clone(),
                    img_rec_on: img_rec_on.clone(),
                    img_play: img_play.clone(),
                    img_lcd_playback: img_lcd_playback.clone(),
                    img_lcd_edit: img_lcd_edit.clone(),
                };

                println!("Starting in {:?} mode.", app_state.mode);
                println!("Playback sink set to: {:?}", app_state.playback_sink);


                update_lcd_mode(
                    &device,
                    app_state.mode,
                    &app_state.img_lcd_playback,
                    &app_state.img_lcd_edit,
                )
                .await;


                for i in 0..8 {
                    let file_name = format!("recording_{}.wav", (b'A' + i) as char);
                    let mut file_path = audio_storage_path.clone();
                    file_path.push(file_name);
                    app_state.button_files.insert(i, file_path);
                }


                for (key, path) in &app_state.button_files {
                    let initial_image = if path.exists() {
                        app_state.img_play.clone()
                    } else {
                        app_state.img_rec_off.clone()
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

                                app_state.handle_encoder_twist(dial, &device).await;
                            }
                            DeviceStateUpdate::EncoderDown(dial) => {

                                app_state.handle_encoder_down(dial, &device).await;
                            }
                            DeviceStateUpdate::ButtonDown(key) => {

                                app_state
                                    .handle_button_down(key, &device, &socket_path)
                                    .await;
                            }
                            DeviceStateUpdate::ButtonUp(key) => {

                                app_state.handle_button_up(key, &device, &socket_path).await;
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
    if let Err(e) = fs::remove_file(&socket_path) {
        if e.kind() != io::ErrorKind::NotFound {
            eprintln!(
                "Failed to remove socket file {}: {}",
                socket_path.display(),
                e
            );
        }
    }
}