//! The call's microphone and speaker. Both streams live on one thread that
//! owns them until the call ends; the call loop only trades samples through
//! bounded queues and reads the meters.
use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

/// Unread microphone audio kept when the loop falls behind.
const MIC_BUFFER_SECONDS: usize = 5;
/// Queued speech never grows past this, whatever the provider sends.
const SPEAKER_BUFFER_SECONDS: usize = 120;

#[derive(Default)]
struct Shared {
    mic: Mutex<VecDeque<f32>>,
    speaker: Mutex<VecDeque<f32>>,
    mic_peak: AtomicU32,
    speaker_peak: AtomicU32,
    /// Playback gain as `f32` bits: 1.0, lowered while the user talks over it.
    gain: AtomicU32,
    failed: AtomicBool,
    stop: AtomicBool,
}

pub(crate) struct Devices {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    pub mic_rate: u32,
    pub speaker_rate: u32,
}

impl Devices {
    /// Open `input` (or the default microphone) and the default speaker.
    pub fn open(input: Option<String>) -> Result<Self> {
        let shared = Arc::new(Shared::default());
        shared.gain.store(1.0f32.to_bits(), Ordering::Relaxed);
        let (ready_tx, ready) = std::sync::mpsc::sync_channel(1);
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("voice-call-audio".into())
            .spawn(move || {
                let streams = open_streams(input.as_deref(), &thread_shared);
                let streams = match streams {
                    Ok((input, output, rates)) => {
                        // Nobody waits any more: the opener gave up.
                        if ready_tx.send(Ok(rates)).is_err() {
                            return;
                        }
                        (input, output)
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                while !thread_shared.stop.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                drop(streams);
            })?;
        let (mic_rate, speaker_rate) = ready
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| anyhow::anyhow!("The audio devices did not open"))??;
        Ok(Self {
            shared,
            thread: Some(thread),
            mic_rate,
            speaker_rate,
        })
    }

    /// Move every captured sample (mono, `mic_rate`) into `out`.
    pub fn take_mic(&self, out: &mut Vec<f32>) {
        let mut mic = self.shared.mic.lock().unwrap_or_else(|e| e.into_inner());
        out.extend(mic.drain(..));
    }

    /// Queue mono samples at `speaker_rate` behind what is already playing.
    pub fn play(&self, samples: &[f32]) {
        let mut speaker = self
            .shared
            .speaker
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let room =
            (self.speaker_rate as usize * SPEAKER_BUFFER_SECONDS).saturating_sub(speaker.len());
        speaker.extend(samples.iter().take(room).copied());
    }

    /// Samples still waiting to play.
    pub fn queued(&self) -> usize {
        self.shared
            .speaker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Drop everything not yet played.
    pub fn flush(&self) {
        self.shared
            .speaker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    pub fn set_gain(&self, gain: f32) {
        self.shared
            .gain
            .store(gain.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// Microphone RMS and speaker peak (0–1) since the previous call.
    pub fn take_levels(&self) -> (f32, f32) {
        (
            f32::from_bits(self.shared.mic_peak.swap(0, Ordering::Relaxed)),
            f32::from_bits(self.shared.speaker_peak.swap(0, Ordering::Relaxed)),
        )
    }

    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Acquire)
    }
}

impl Drop for Devices {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn open_streams(
    input: Option<&str>,
    shared: &Arc<Shared>,
) -> Result<(cpal::Stream, cpal::Stream, (u32, u32))> {
    let microphone = crate::input_device(input).context("No microphone available")?;
    let speaker = cpal::default_host()
        .default_output_device()
        .context("No speaker available")?;
    let input = input_stream(&microphone, shared)?;
    let output = output_stream(&speaker, shared)?;
    input.0.play()?;
    output.0.play()?;
    Ok((input.0, output.0, (input.1, output.1)))
}

fn input_stream(device: &cpal::Device, shared: &Arc<Shared>) -> Result<(cpal::Stream, u32)> {
    let config = device.default_input_config()?;
    let rate = config.sample_rate();
    let channels = config.channels() as usize;
    let cap = rate as usize * MIC_BUFFER_SECONDS;
    macro_rules! stream {
        ($sample:ty) => {{
            let data = shared.clone();
            let err = shared.clone();
            device.build_input_stream(
                &config.clone().into(),
                move |d: &[$sample], _| {
                    crate::meter(d, channels, &data.mic_peak);
                    if let Ok(mut mic) = data.mic.try_lock() {
                        for frame in d.chunks_exact(channels.max(1)) {
                            if mic.len() >= cap {
                                mic.pop_front();
                            }
                            mic.push_back(
                                frame
                                    .iter()
                                    .map(|s| cpal::Sample::to_sample::<f32>(*s))
                                    .sum::<f32>()
                                    / channels as f32,
                            );
                        }
                    }
                },
                move |_| err.failed.store(true, Ordering::Release),
                None,
            )?
        }};
    }
    let stream = match config.sample_format() {
        cpal::SampleFormat::I8 => stream!(i8),
        cpal::SampleFormat::I16 => stream!(i16),
        cpal::SampleFormat::I24 => stream!(cpal::I24),
        cpal::SampleFormat::I32 => stream!(i32),
        cpal::SampleFormat::I64 => stream!(i64),
        cpal::SampleFormat::U8 => stream!(u8),
        cpal::SampleFormat::U16 => stream!(u16),
        cpal::SampleFormat::U24 => stream!(cpal::U24),
        cpal::SampleFormat::U32 => stream!(u32),
        cpal::SampleFormat::U64 => stream!(u64),
        cpal::SampleFormat::F32 => stream!(f32),
        cpal::SampleFormat::F64 => stream!(f64),
        _ => bail!("Unsupported microphone format"),
    };
    Ok((stream, rate))
}

fn output_stream(device: &cpal::Device, shared: &Arc<Shared>) -> Result<(cpal::Stream, u32)> {
    let config = device.default_output_config()?;
    let rate = config.sample_rate();
    let channels = config.channels() as usize;
    macro_rules! stream {
        ($sample:ty) => {{
            let data = shared.clone();
            let err = shared.clone();
            device.build_output_stream(
                &config.clone().into(),
                move |out: &mut [$sample], _| {
                    let gain = f32::from_bits(data.gain.load(Ordering::Relaxed));
                    let mut peak = 0.0f32;
                    // A contended queue plays silence rather than blocking.
                    let mut speaker = data.speaker.try_lock().ok();
                    for frame in out.chunks_exact_mut(channels.max(1)) {
                        let s = speaker.as_mut().and_then(|q| q.pop_front()).unwrap_or(0.0) * gain;
                        peak = peak.max(s.abs());
                        let s: $sample = cpal::Sample::from_sample(s.clamp(-1.0, 1.0));
                        frame.fill(s);
                    }
                    if peak.is_finite() {
                        data.speaker_peak
                            .fetch_max(peak.to_bits(), Ordering::Relaxed);
                    }
                },
                move |_| err.failed.store(true, Ordering::Release),
                None,
            )?
        }};
    }
    let stream = match config.sample_format() {
        cpal::SampleFormat::I8 => stream!(i8),
        cpal::SampleFormat::I16 => stream!(i16),
        cpal::SampleFormat::I24 => stream!(cpal::I24),
        cpal::SampleFormat::I32 => stream!(i32),
        cpal::SampleFormat::I64 => stream!(i64),
        cpal::SampleFormat::U8 => stream!(u8),
        cpal::SampleFormat::U16 => stream!(u16),
        cpal::SampleFormat::U24 => stream!(cpal::U24),
        cpal::SampleFormat::U32 => stream!(u32),
        cpal::SampleFormat::U64 => stream!(u64),
        cpal::SampleFormat::F32 => stream!(f32),
        cpal::SampleFormat::F64 => stream!(f64),
        _ => bail!("Unsupported speaker format"),
    };
    Ok((stream, rate))
}
