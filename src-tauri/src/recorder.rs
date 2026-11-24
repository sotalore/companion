//! Push-to-talk recording from the default microphone.
//!
//! The microphone is opened only while the talk key is held: `start` opens it, `stop` closes it and
//! hands back what was heard. Between recordings nothing runs, so it costs the game nothing, and
//! Windows' "microphone in use" icon shows only while recording.
//!
//! The audio library (`cpal`) calls us on its own thread with each few milliseconds of sound. Its
//! stream object can't move between threads, so one thread of ours owns it for its whole life and
//! takes orders (start, stop) from a channel: like a small worker process in Ruby that reads jobs
//! from a queue.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime};

/// Whisper's input: 16,000 samples a second, one channel. Recordings are kept at this rate.
pub const RATE: u32 = 16_000;
/// A recording stops by itself after this long, in case the key's release is ever missed.
const MAX_LENGTH: Duration = Duration::from_secs(10 * 60);

/// One recording, ready to save.
pub struct Clip {
    /// When the key was pressed.
    pub started: SystemTime,
    /// Mono samples at `RATE`, from -1.0 to 1.0.
    pub samples: Vec<f32>,
    /// Which microphone it came from.
    pub device: String,
}

impl Clip {
    pub fn seconds(&self) -> f32 {
        self.samples.len() as f32 / RATE as f32
    }
}

enum Order {
    Start,
    Stop,
}

/// The handle the rest of the app keeps: send it orders, and it calls `on_done` with each recording.
pub struct Recorder {
    orders: Sender<Order>,
}

impl Recorder {
    /// Starts the recording thread. `on_started` runs once the microphone is open (or failed to
    /// open), `on_done` with each finished recording or the reason there isn't one.
    pub fn start_thread(
        on_started: impl Fn(Result<String, String>) + Send + 'static,
        on_done: impl Fn(Result<Clip, String>) + Send + 'static,
    ) -> Recorder {
        let (orders, inbox) = mpsc::channel();
        std::thread::Builder::new()
            .name("recorder".into())
            .spawn(move || run(inbox, on_started, on_done))
            .expect("can start the recorder thread");
        Recorder { orders }
    }

    pub fn start(&self) {
        let _ = self.orders.send(Order::Start);
    }

    pub fn stop(&self) {
        let _ = self.orders.send(Order::Stop);
    }
}

/// The recording thread: waits for Start, records until Stop (or `MAX_LENGTH`), and repeats.
fn run(
    inbox: Receiver<Order>,
    on_started: impl Fn(Result<String, String>),
    on_done: impl Fn(Result<Clip, String>),
) {
    // `recv` waits (without using the CPU) for the next order; it fails only when the app is closing.
    while let Ok(order) = inbox.recv() {
        if let Order::Stop = order {
            continue; // a release without a press we acted on
        }
        let started = SystemTime::now();
        let session = match open_microphone() {
            Ok(session) => session,
            Err(e) => {
                on_started(Err(e));
                continue;
            }
        };
        on_started(Ok(session.device.clone()));
        let began = Instant::now();
        // Wait for Stop, but not past MAX_LENGTH.
        loop {
            let left = MAX_LENGTH.saturating_sub(began.elapsed());
            match inbox.recv_timeout(left) {
                Ok(Order::Stop) | Err(RecvTimeoutError::Timeout) => break,
                Ok(Order::Start) => continue, // already recording
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        on_done(session.finish(started));
    }
}

/// An open microphone stream and the sound received so far.
struct Session {
    stream: cpal::Stream,
    /// The audio thread sends each chunk of sound here, already mixed down to one channel.
    chunks: Receiver<Vec<f32>>,
    errors: Receiver<String>,
    rate: u32,
    device: String,
}

fn open_microphone() -> Result<Session, String> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or("No microphone: Windows has no default recording device.")?;
    let name = device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "the microphone".into());
    let config = device
        .default_input_config()
        .map_err(|e| format!("Couldn't use {name}: {e}"))?;
    let rate = config.sample_rate();
    let channels = config.channels() as usize;
    let (chunk_tx, chunks) = mpsc::channel();
    let (error_tx, errors) = mpsc::channel();

    // The stream is built for the device's own sample type; each kind converts to f32 the same way.
    // `build::<i16>` is like calling a method generic over the type, which Ruby never has to spell out.
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => build::<f32>(&device, &config.into(), channels, chunk_tx, error_tx),
        cpal::SampleFormat::I16 => build::<i16>(&device, &config.into(), channels, chunk_tx, error_tx),
        cpal::SampleFormat::I32 => build::<i32>(&device, &config.into(), channels, chunk_tx, error_tx),
        cpal::SampleFormat::U16 => build::<u16>(&device, &config.into(), channels, chunk_tx, error_tx),
        other => return Err(format!("{name} records in a format the companion can't read ({other}).")),
    }
    .map_err(|e| format!("Couldn't open {name}: {e}. Is microphone access allowed for desktop apps in Windows' privacy settings?"))?;
    stream.play().map_err(|e| format!("Couldn't start {name}: {e}"))?;
    Ok(Session { stream, chunks, errors, rate, device: name })
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    chunks: Sender<Vec<f32>>,
    errors: Sender<String>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    device.build_input_stream(
        *config,
        // Runs on the audio thread for every chunk: keep it short. Each frame (one sample per
        // channel) becomes the average of its channels.
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let mono = data
                .chunks(channels.max(1))
                .map(|frame| frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / frame.len() as f32)
                .collect();
            let _ = chunks.send(mono);
        },
        move |err| {
            let _ = errors.send(err.to_string());
        },
        None,
    )
}

impl Session {
    /// Closes the microphone and returns the recording, at `RATE`.
    fn finish(self, started: SystemTime) -> Result<Clip, String> {
        drop(self.stream); // closing the stream stops the callbacks; no more chunks arrive
        if let Ok(err) = self.errors.try_recv() {
            return Err(format!("The microphone stopped: {err}"));
        }
        let heard: Vec<f32> = self.chunks.try_iter().flatten().collect();
        Ok(Clip { started, samples: resample(&heard, self.rate, RATE), device: self.device })
    }
}

/// Changes the sample rate of mono sound (most microphones give 48,000 a second; Whisper wants
/// 16,000). Going down, each output sample is the average of the input samples it covers, which
/// filters out the high pitches that would otherwise fold back as noise; going up, it interpolates.
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let step = from as f64 / to as f64; // input samples per output sample
    let out_len = (input.len() as f64 / step).floor() as usize;
    (0..out_len)
        .map(|i| {
            let start = i as f64 * step;
            if step > 1.0 {
                let (a, b) = (start as usize, ((start + step) as usize).min(input.len()).max(start as usize + 1));
                input[a..b].iter().sum::<f32>() / (b - a) as f32
            } else {
                let a = start as usize;
                let b = (a + 1).min(input.len() - 1);
                let t = (start - a as f64) as f32;
                input[a] * (1.0 - t) + input[b] * t
            }
        })
        .collect()
}

/// Saves mono `RATE` samples as a 16-bit WAV file.
pub fn write_wav(path: &std::path::Path, samples: &[f32]) -> Result<(), String> {
    let spec = hound::WavSpec { channels: 1, sample_rate: RATE, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut writer = hound::WavWriter::create(path, spec).map_err(|e| format!("Couldn't write {}: {e}", path.display()))?;
    for &s in samples {
        let value = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        writer.write_sample(value).map_err(|e| e.to_string())?;
    }
    writer.finalize().map_err(|e| e.to_string())
}

/// Reads a WAV file back as mono `RATE` samples (any WAV: it's mixed down and resampled if need be).
pub fn read_wav(path: &std::path::Path) -> Result<Vec<f32>, String> {
    let mut reader = hound::WavReader::open(path).map_err(|e| format!("Couldn't read {}: {e}", path.display()))?;
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>(),
        hound::SampleFormat::Int => {
            let scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader.samples::<i32>().map(|s| s.map(|v| v as f32 / scale)).collect::<Result<_, _>>()
        }
    }
    .map_err(|e| format!("{} is damaged: {e}", path.display()))?;
    let channels = spec.channels.max(1) as usize;
    let mono: Vec<f32> = samples.chunks(channels).map(|f| f.iter().sum::<f32>() / f.len() as f32).collect();
    Ok(resample(&mono, spec.sample_rate, RATE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resamples_48k_to_16k_by_averaging() {
        let input: Vec<f32> = (0..48_000).map(|i| (i % 3) as f32).collect(); // 0,1,2,0,1,2...
        let out = resample(&input, 48_000, 16_000);
        assert_eq!(out.len(), 16_000);
        assert!(out.iter().all(|&s| (s - 1.0).abs() < 1e-6), "each group of three averages to 1");
    }

    #[test]
    fn resamples_44k1_and_up() {
        assert_eq!(resample(&vec![0.5; 44_100], 44_100, 16_000).len(), 16_000);
        let up = resample(&[0.0, 1.0], 8_000, 16_000);
        assert_eq!(up, vec![0.0, 0.5, 1.0, 1.0]);
    }

    #[test]
    fn a_wav_reads_back_as_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.wav");
        let samples: Vec<f32> = (0..1600).map(|i| (i as f32 / 100.0).sin() * 0.5).collect();
        write_wav(&path, &samples).unwrap();
        let back = read_wav(&path).unwrap();
        assert_eq!(back.len(), samples.len());
        assert!(back.iter().zip(&samples).all(|(a, b)| (a - b).abs() < 1e-3));
    }
}
