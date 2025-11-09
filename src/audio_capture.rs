use crate::AudioCommand;
use hound::{SampleFormat, WavSpec, WavWriter};
use pipewire as pw;
use pw::{properties::properties, spa};
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::pod::Pod;
use std::convert::TryInto;
use std::fs;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc::Receiver};
use std::thread;

#[derive(Debug, PartialEq, Clone)]
enum State {
    Listening,
    Recording(PathBuf),
}

struct UserData {
    format: Option<spa::param::audio::AudioInfoRaw>,
    state: State,
    buffer: Vec<f32>,
}

fn save_recording_from_buffer(
    buffer: Vec<f32>,
    format: &spa::param::audio::AudioInfoRaw,
    filename: &Path,
) {
    if buffer.is_empty() {
        println!("Buffer is empty, not saving.");
        return;
    }
    if let Some(parent) = filename.parent()
        && !parent.exists()
        && let Err(e) = fs::create_dir_all(parent)
    {
        eprintln!("Failed to create directory {}: {}", parent.display(), e);
        return;
    }
    let spec = WavSpec {
        channels: format.channels() as u16,
        sample_rate: format.rate(),
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };
    println!("Saving recording to {}...", filename.display());
    match WavWriter::create(filename, spec) {
        Ok(mut writer) => {
            for &sample in &buffer {
                if let Err(e) = writer.write_sample(sample) {
                    eprintln!("Error writing sample: {}", e);
                    break;
                }
            }
            if let Err(e) = writer.finalize() {
                eprintln!("Error finalizing WAV file: {}", e);
            } else {
                println!(
                    "Saved {} samples ({} channels) to {}.",
                    buffer.len(),
                    format.channels(),
                    filename.display()
                );
            }
        }
        Err(e) => {
            eprintln!("Error creating WAV file: {}", e);
        }
    }
}

/// It runs in a separate thread and blocks on the MPSC channel.
fn handle_audio_commands(rx: Receiver<AudioCommand>, data: Arc<Mutex<UserData>>) {
    // This loop blocks on `rx.recv()`, waiting for commands from the main thread.
    // When the main thread drops its `Sender`, this loop will end.
    for command in rx {
        let mut save_data: Option<(Vec<f32>, spa::param::audio::AudioInfoRaw, PathBuf)> = None;
        {
            // Scoped MutexGuard
            let mut user_data = data.lock().unwrap();
            match command {
                AudioCommand::Start(path) => {
                    if user_data.format.is_none() {
                        eprintln!("Refused START: Audio format not yet known.");
                    } else {
                        match user_data.state {
                            State::Listening => {
                                println!("START recording to {}", path.display());
                                user_data.state = State::Recording(path);
                                user_data.buffer.clear();
                            }
                            State::Recording(_) => {
                                eprintln!("Refused START: Already recording.");
                            }
                        }
                    }
                }
                AudioCommand::Stop => {
                    let old_state = std::mem::replace(&mut user_data.state, State::Listening);
                    if let State::Recording(save_path) = old_state {
                        println!("STOP recording.");
                        let buffer_to_save = std::mem::take(&mut user_data.buffer);
                        let format_to_save = *user_data.format.as_ref().unwrap();
                        save_data = Some((buffer_to_save, format_to_save, save_path));
                    } else {
                        eprintln!("Refused STOP: Not recording.");
                    }
                }
            }
        }
        // Save data *outside* the mutex lock
        if let Some((buffer, format, path)) = save_data {
            save_recording_from_buffer(buffer, &format, &path);
        }
    }
    println!("Audio command channel closed. Exiting command loop.");
}

pub fn run_capture_loop(rx: Receiver<AudioCommand>) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let data = Arc::new(Mutex::new(UserData {
        format: None,
        state: State::Listening,
        buffer: Vec::new(),
    }));

    // --- PipeWire Stream Setup (Unchanged) ---
    let props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::STREAM_CAPTURE_SINK => "true",
    };
    let stream = pw::stream::StreamBox::new(&core, "audio-capture", props)?;
    let _listener = stream
        .add_local_listener_with_user_data(data.clone())
        .param_changed(|_, user_data_arc, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let (media_type, media_subtype) = match format_utils::parse_format(param) {
                Ok(v) => v,
                Err(_) => return,
            };
            if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                return;
            }
            let mut user_data = user_data_arc.lock().unwrap();
            let mut info = spa::param::audio::AudioInfoRaw::new();
            info.parse(param)
                .expect("Failed to parse param changed to AudioInfoRaw");
            println!(
                "capturing rate:{} channels:{}",
                info.rate(),
                info.channels()
            );
            user_data.format = Some(info);
        })
        .process(|stream, user_data_arc| {
            let mut user_data = user_data_arc.lock().unwrap();
            let Some(_format) = user_data.format.as_ref() else {
                return;
            };
            if user_data.state == State::Listening {
                let _ = stream.dequeue_buffer();
                return;
            }
            match stream.dequeue_buffer() {
                None => println!("out of buffers"),
                Some(mut buffer) => {
                    let datas = buffer.datas_mut();
                    if datas.is_empty() {
                        return;
                    }
                    let data = &mut datas[0];
                    let n_samples = data.chunk().size() / (mem::size_of::<f32>() as u32);
                    if let Some(samples) = data.data() {
                        let mut all_samples = Vec::with_capacity(n_samples as usize);
                        for n in 0..(n_samples as usize) {
                            let start = n * mem::size_of::<f32>();
                            let end = start + mem::size_of::<f32>();
                            let chan = &samples[start..end];
                            all_samples.push(f32::from_le_bytes(chan.try_into().unwrap()));
                        }
                        if let State::Recording(_) = user_data.state {
                            user_data.buffer.extend_from_slice(&all_samples);
                        }
                    }
                }
            }
        })
        .register()?;

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .unwrap()
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).unwrap()];
    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;
    // --- End of Stream Setup ---

    let ipc_data = data.clone();
    thread::spawn(move || {
        handle_audio_commands(rx, ipc_data);
    });

    mainloop.run();
    Ok(())
}

