use hound::{WavReader, WavSpec, WavWriter};
use std::env;
use std::io;
use std::path::{Path, PathBuf};

/// Creates a temporary, pitch-shifted copy of a WAV file.
///
/// This is a synchronous function and should be called from a
/// non-blocking context (e.g., `tokio::task::spawn_blocking`).
///
/// It works by copying all audio samples but writing a new
/// header with a modified sample rate.
pub fn create_pitched_copy_sync(original_path: &Path, semitone_shift: f64) -> io::Result<PathBuf> {
    // 1. Calculate the pitch ratio (e.g., ~0.943 for -1 semitone)
    let pitch_ratio = 2.0_f64.powf(semitone_shift / 12.0);

    // 2. Open the original file

    let mut reader = WavReader::open(original_path).map_err(io::Error::other)?;
    let in_spec = reader.spec();

    // 3. Calculate the new spec with the modified sample rate
    let new_sample_rate = (in_spec.sample_rate as f64 * pitch_ratio).round() as u32;
    let out_spec = WavSpec {
        channels: in_spec.channels,
        sample_rate: new_sample_rate,
        bits_per_sample: in_spec.bits_per_sample,
        sample_format: in_spec.sample_format,
    };

    // 4. Create a unique path for the temporary file
    let unique_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_micros();
    let temp_file_path = env::temp_dir().join(format!("pitched_sample_{}.wav", unique_id));

    // 5. Create the writer for the new temp file

    let mut writer = WavWriter::create(&temp_file_path, out_spec).map_err(io::Error::other)?;

    // 6. Copy samples, handling the different possible WAV formats
    //    We must match the format we are reading.
    match (in_spec.sample_format, in_spec.bits_per_sample) {
        (hound::SampleFormat::Int, 16) => {
            for sample in reader.samples::<i16>() {
                writer
                    .write_sample(sample.map_err(io::Error::other)?)
                    .map_err(io::Error::other)?;
            }
        }
        (hound::SampleFormat::Int, 32) => {
            for sample in reader.samples::<i32>() {
                writer
                    .write_sample(sample.map_err(io::Error::other)?)
                    .map_err(io::Error::other)?;
            }
        }
        (hound::SampleFormat::Float, 32) => {
            // This is the format our pipewire_source creates
            for sample in reader.samples::<f32>() {
                writer
                    .write_sample(sample.map_err(io::Error::other)?)
                    .map_err(io::Error::other)?;
            }
        }
        (hound::SampleFormat::Int, 24) => {
            // hound reads 24-bit samples as i32
            for sample in reader.samples::<i32>() {
                writer
                    .write_sample(sample.map_err(io::Error::other)?)
                    .map_err(io::Error::other)?;
            }
        }
        _ => {
            // If we encounter an unsupported format, return an error.
            return Err(io::Error::other(format!(
                "Unsupported WAV format: {:?}, {}-bit",
                in_spec.sample_format, in_spec.bits_per_sample
            )));
        }
    }

    // 7. Finalize the file and return the path

    writer.finalize().map_err(io::Error::other)?;
    Ok(temp_file_path)
}

