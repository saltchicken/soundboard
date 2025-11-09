use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;

/// Defines where audio should be played back.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PlaybackSink {
    Default,
    Mixer,
    Both,
}

/// Asynchronously plays an audio file through PipeWire.
pub async fn play_audio_file(
    path: &PathBuf,
    sink_target: PlaybackSink,
    volume: f64,
) -> io::Result<()> {
    let player = "pw-play";
    let volume_str = volume.to_string();
    println!(
        "Attempting to play file with '{}': {}",
        player,
        path.display()
    );

    let mut cmd_default = Command::new(player);
    cmd_default.arg("--volume");
    cmd_default.arg(&volume_str);
    cmd_default.arg(path);
    cmd_default.stdout(Stdio::null()).stderr(Stdio::null());

    let mut cmd_mixer = Command::new(player);
    cmd_default.arg("--volume");
    cmd_default.arg(&volume_str);

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
            let default_handle = tokio::spawn(async move { cmd_default.status().await });
            let mixer_handle = tokio::spawn(async move { cmd_mixer.status().await });

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
