
use soundboard::{
    AudioCommand,
    get_audio_storage_path,
};
mod audio_player;
use crate::audio_player::{PlaybackSink, play_audio_file};
mod lcd;
use crate::lcd::{create_fallback_image, create_fallback_lcd_image, update_lcd_mode};
mod audio_processor;

mod audio_capture;
use elgato_streamdeck::{AsyncStreamDeck, DeviceStateUpdate, list_devices, new_hidapi};
use image::open;
use image::{DynamicImage, Rgb};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use tokio::fs as tokio_fs;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Mode {
    Playback,
    Edit,
}

struct AppState {
    mode: Mode,
    playback_sink: PlaybackSink,
    playback_volume: HashMap<u8, f64>,
    button_files: HashMap<u8, PathBuf>,
    active_recording_key: Option<u8>,
    selected_for_delete: Option<u8>,
    pitch_shift_semitones: HashMap<u8, f64>,
    img_rec_off: DynamicImage,
    img_rec_on: DynamicImage,
    img_play: DynamicImage,
    img_lcd_playback: DynamicImage,
    img_lcd_edit: DynamicImage,

    audio_cmd_tx: mpsc::Sender<AudioCommand>,
}

impl AppState {
    async fn handle_encoder_twist(&mut self, dial: u8, ticks: i32, device: &AsyncStreamDeck) {
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
        } else if dial == 1 {
            if self.mode == Mode::Edit {
                if let Some(key) = self.selected_for_delete {
                    // A key is selected, so adjust its volume
                    let current_volume = self.playback_volume.entry(key).or_insert(1.0);
                    *current_volume += ticks as f64 * 0.05; // 5% per tick
                    *current_volume = current_volume.clamp(0.0, 1.5); // 0% to 150%
                    println!(
                        "Set volume for key {} to {:.0}%",
                        key,
                        *current_volume * 100.0
                    );
                } else {
                    println!("Dial 1 (Volume) turned in Edit mode, but no sample is selected.");
                }
            }
        } else if dial == 2 {
            if self.mode == Mode::Edit {
                if let Some(key) = self.selected_for_delete {
                    // A key is selected, so adjust its pitch
                    let current_pitch = self.pitch_shift_semitones.entry(key).or_insert(0.0);
                    // Adjust by 0.1 semitones per "tick" of the dial
                    *current_pitch += ticks as f64 * 0.1;
                    println!(
                        "Set pitch for key {} to {:.2} semitones",
                        key, *current_pitch
                    );
                } else {
                    println!("Dial 2 turned in Edit mode, but no sample is selected.");
                }
            }
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

                        match tokio_fs::remove_file(path).await {
                            Ok(_) => {
                                println!("...File {} deleted.", path.display());
                                self.pitch_shift_semitones.remove(&key_to_delete);
                                self.playback_volume.remove(&key_to_delete);
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


    async fn handle_button_down(&mut self, key: u8, device: &AsyncStreamDeck) {
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
                            "Button {} down (Playback Mode, no file). Sending START.",
                            key
                        );

                        // This is a sync send, but it's non-blocking (just
                        // drops the command in a queue) so it's fine in async.
                        let cmd = AudioCommand::Start(path.clone());
                        if let Err(e) = self.audio_cmd_tx.send(cmd) {
                            eprintln!("Failed to send START command: {}", e);
                        } else {

                            // The audio thread will handle logic.
                            self.active_recording_key = Some(key);
                            device
                                .set_button_image(key, self.img_rec_on.clone())
                                .await
                                .unwrap();
                            device.flush().await.unwrap();
                            println!("...START sent.");
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


    async fn handle_button_up(&mut self, key: u8, device: &AsyncStreamDeck) {
        match self.mode {
            Mode::Playback => {
                if self.active_recording_key == Some(key) {
                    println!(
                        "Button {} up (Playback Mode, was recording), sending STOP",
                        key
                    );

                    if let Err(e) = self.audio_cmd_tx.send(AudioCommand::Stop) {
                        eprintln!("Failed to send STOP command: {}", e);
                    } else {
                        println!("...STOP sent.");
                    }

                    self.active_recording_key = None;
                    device
                        .set_button_image(key, self.img_play.clone())
                        .await
                        .unwrap();
                    device.flush().await.unwrap();
                } else if let Some(path) = self.button_files.get(&key) {
                    if path.exists() {
                        println!("Button {} up (Playback Mode). Triggering playback.", key);

                        let pitch_shift =
                            self.pitch_shift_semitones.get(&key).cloned().unwrap_or(0.0);
                        let path_clone = path.clone();
                        let sink_clone = self.playback_sink;
                        let volume_clone = self.playback_volume.get(&key).cloned().unwrap_or(1.0);

                        // This task will create a temp file if needed, play it,
                        // and then clean up the temp file.
                        tokio::spawn(async move {
                            let mut temp_path: Option<PathBuf> = None;
                            // 1. Check if we need to apply pitch shift
                            // We use an epsilon (0.01) to avoid floating point issues
                            let path_to_play = if pitch_shift.abs() > 0.01 {
                                println!("...Applying pitch shift: {:.2} semitones", pitch_shift);

                                let path_for_blocking = path_clone.clone();
                                // 2. Run the synchronous file I/O in a blocking thread
                                // This prevents blocking the main async runtime
                                match tokio::task::spawn_blocking(move || {

                                    audio_processor::create_pitched_copy_sync(
                                        &path_for_blocking,
                                        pitch_shift,
                                    )
                                })
                                .await
                                {
                                    Ok(Ok(new_path)) => {
                                        // Successfully created temp file
                                        temp_path = Some(new_path.clone());
                                        new_path
                                    }
                                    Ok(Err(e)) => {
                                        // Failed to create, play original
                                        eprintln!(
                                            "Failed to create pitched copy: {}. Playing original.",
                                            e
                                        );

                                        path_clone
                                    }
                                    Err(e) => {
                                        // Task itself failed, play original
                                        eprintln!(
                                            "Task join error for pitched copy: {}. Playing original.",
                                            e
                                        );

                                        path_clone
                                    }
                                }
                            } else {
                                // No pitch shift, play original
                                path_clone
                            };
                            // 3. Play the chosen file (original or temp)
                            if let Err(e) =
                                play_audio_file(&path_to_play, sink_clone, volume_clone).await
                            {
                                eprintln!("Playback failed: {}", e);
                            }
                            // 4. Clean up the temp file if one was created
                            if let Some(p) = temp_path {
                                if let Err(e) = tokio_fs::remove_file(&p).await {
                                    eprintln!(
                                        "Failed to clean up temp file {}: {}",
                                        p.display(),
                                        e
                                    );
                                } else {
                                    println!("Cleaned up temp file: {}", p.display());
                                }
                            }
                        });
                        // Set image back to "play" immediately
                        device
                            .set_button_image(key, self.img_play.clone())
                            .await
                            .unwrap();
                        device.flush().await.unwrap();
                    }
                }
            }
            Mode::Edit => {
                // ButtonUp does nothing in Edit mode
            }
        }
    }
}

#[tokio::main]
async fn main() {

    let audio_storage_path = match get_audio_storage_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("Failed to get audio storage path: {}", e);
            return;
        }
    };


    let (audio_tx, audio_rx) = mpsc::channel();


    // This thread will block on the pipewire mainloop, which is perfect.
    std::thread::spawn(move || {
        println!("Audio capture thread started...");
        if let Err(e) = audio_capture::run_capture_loop(audio_rx) {
            eprintln!("Audio capture thread failed: {}", e);
        } else {
            println!("Audio capture thread exited cleanly.");
        }
    });


    // No `start_pipewire_source`, `wait_for_server`, `shutdown_tx`,
    // or `server_process.wait()` task.

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
                    playback_volume: HashMap::new(),
                    button_files: HashMap::new(),
                    active_recording_key: None,
                    selected_for_delete: None,
                    pitch_shift_semitones: HashMap::new(),
                    img_rec_off: img_rec_off.clone(),
                    img_rec_on: img_rec_on.clone(),
                    img_play: img_play.clone(),
                    img_lcd_playback: img_lcd_playback.clone(),
                    img_lcd_edit: img_lcd_edit.clone(),

                    audio_cmd_tx: audio_tx.clone(),
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
                            DeviceStateUpdate::EncoderTwist(dial, ticks) => {
                                app_state
                                    .handle_encoder_twist(dial, ticks as i32, &device)
                                    .await;
                            }
                            DeviceStateUpdate::EncoderDown(dial) => {
                                app_state.handle_encoder_down(dial, &device).await;
                            }
                            DeviceStateUpdate::ButtonDown(key) => {

                                app_state.handle_button_down(key, &device).await;
                            }
                            DeviceStateUpdate::ButtonUp(key) => {

                                app_state.handle_button_up(key, &device).await;
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

    println!("Main function exiting. Audio thread will exit when sender is dropped.");

    // When `audio_tx` (inside `app_state`) is dropped here,
    // the `rx.recv()` loop in `handle_audio_commands` will
    // end, and the audio thread will clean itself up.
}
