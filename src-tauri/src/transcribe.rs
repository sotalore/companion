//! Speech to text with Whisper, on peregrin. The voice never leaves the machine.
//!
//! Whisper (whisper.cpp, through the `whisper-rs` crate) is built only with the `whisper` feature
//! (`cargo build --features whisper`), because building it needs libclang. Without the feature the app
//! still records notes, and the Transcribe button says what's missing.
//!
//! A batch runs on its own thread at low priority, and Whisper uses only some of the CPU's cores, so
//! even a batch started by mistake mid-game leaves the game room.

use std::path::Path;

/// Whisper's threads: half the CPU's (the i7-11700K has 16), so the rest stays free.
#[cfg_attr(not(feature = "whisper"), allow(dead_code))]
fn threads() -> i32 {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    (cpus / 2).max(1) as i32
}

/// Lowers this thread's priority, so Windows runs the game first when both want the CPU.
pub fn lower_priority() {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
    // SAFETY: GetCurrentThread returns a handle to this thread that needs no closing, and setting
    // its priority can't affect memory.
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

/// The text Whisper is told came before the note: the game and, if known, the scene.
#[cfg_attr(not(feature = "whisper"), allow(dead_code))]
fn prompt(scene: Option<&str>) -> String {
    match scene {
        Some(name) => format!("Notes about {name}, a scene in Shroud of the Avatar: its places, inns, shops and people."),
        None => "Notes about a scene in Shroud of the Avatar: its places, inns, shops and people.".to_string(),
    }
}

/// A loaded model, ready to transcribe one note after another.
pub struct Whisper {
    #[cfg(feature = "whisper")]
    context: whisper_rs::WhisperContext,
}

#[cfg(feature = "whisper")]
impl Whisper {
    pub fn load(model: &Path) -> Result<Whisper, String> {
        // Send whisper.cpp's chatter to the log crate (which prints nothing here) rather than stderr.
        whisper_rs::install_logging_hooks();
        let path = model.to_str().ok_or("The model's path isn't valid text.")?;
        let context = whisper_rs::WhisperContext::new_with_params(path, whisper_rs::WhisperContextParameters::default())
            .map_err(|e| format!("Couldn't load the Whisper model {}: {e}", model.display()))?;
        Ok(Whisper { context })
    }

    /// The text of one note: mono samples at 16 kHz. `scene` (its name, if known) goes into the prompt
    /// Whisper starts from, which makes it likelier to spell the scene's and the game's names right.
    pub fn transcribe(&mut self, samples: &[f32], scene: Option<&str>) -> Result<String, String> {
        use whisper_rs::{FullParams, SamplingStrategy};
        let mut state = self.context.create_state().map_err(|e| format!("Whisper couldn't start: {e}"))?;
        // Beam search: on peregrin it costs about the same as greedy (most of the time goes into
        // listening to the audio, not choosing words), and it makes fewer mistakes.
        let prompt = prompt(scene); // before `params`, which borrows it
        let mut params = FullParams::new(SamplingStrategy::BeamSearch { beam_size: 5, patience: -1.0 });
        params.set_n_threads(threads());
        params.set_initial_prompt(&prompt);
        params.set_language(Some("en"));
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        state.full(params, samples).map_err(|e| format!("Whisper failed: {e}"))?;
        let text: Vec<String> = state.as_iter().map(|segment| segment.to_string().trim().to_string()).collect();
        Ok(text.join(" ").trim().to_string())
    }
}

#[cfg(not(feature = "whisper"))]
impl Whisper {
    pub fn load(_model: &Path) -> Result<Whisper, String> {
        Err("This build of the companion has no Whisper: it's built with `--features whisper`, which needs \
             libclang (sotalore/companion#4)."
            .into())
    }

    pub fn transcribe(&mut self, _samples: &[f32], _scene: Option<&str>) -> Result<String, String> {
        unreachable!("load always fails without the whisper feature")
    }
}

#[cfg(all(test, feature = "whisper"))]
mod tests {
    use super::*;

    /// Times a transcription: `WHISPER_MODEL=... WHISPER_WAV=... cargo test --release -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn times_a_transcription() {
        let (Some(model), Some(wav)) = (std::env::var_os("WHISPER_MODEL"), std::env::var_os("WHISPER_WAV")) else {
            return;
        };
        println!("{}", whisper_rs::print_system_info());
        let samples = crate::recorder::read_wav(Path::new(&wav)).unwrap();
        let mut whisper = Whisper::load(Path::new(&model)).unwrap();
        for scene in [None, Some("Owl's Head")] {
            let began = std::time::Instant::now();
            let text = whisper.transcribe(&samples, scene).unwrap();
            println!("{:.1} s ({scene:?}): {text}", began.elapsed().as_secs_f32());
        }
    }
}
