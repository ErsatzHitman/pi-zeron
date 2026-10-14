//! Desktop dictation: capture on this device, transcription in the cloud
//! ([`cloud`]). No engine, document, RPC or audio persistence. Voice mode,
//! a spoken conversation, is [`call`].
use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};

pub mod call;
mod cleanup;
pub mod cloud;
mod resample;
mod trigger;

pub use trigger::{DEFAULT_PHRASE as DEFAULT_TRIGGER_PHRASE, MAX_PHRASE_CHARS};

pub const MAX_SECONDS: usize = 60;

/// Why a trigger phrase cannot be used, or `None` when it can (empty is off).
pub fn trigger_problem(phrase: &str) -> Option<&'static str> {
    if phrase.trim().is_empty() {
        return None;
    }
    trigger::Trigger::check(phrase, |p| cleanup::clean(p, true)).err()
}

struct Audio {
    /// Only the audio callback touches this until the stream is dropped.
    samples: Mutex<Vec<f32>>,
    // Outside the lock: the capture loop polls these every 10 ms and must
    // never make the real-time callback's `try_lock` drop a buffer.
    failed: AtomicBool,
    full: AtomicBool,
}
impl Audio {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            samples: Mutex::new(Vec::with_capacity(capacity)),
            failed: AtomicBool::new(false),
            full: AtomicBool::new(false),
        }
    }
}
/// Loudest RMS since the UI last read it. Non-negative `f32` bit patterns sort
/// like their values, so `fetch_max` keeps the peak without a lock.
fn meter<T: cpal::Sample>(data: &[T], channels: usize, level: &AtomicU32)
where
    f32: cpal::FromSample<T>,
{
    let frames = data.len() / channels.max(1);
    if frames == 0 {
        return;
    }
    let energy = data
        .chunks_exact(channels)
        .map(|frame| {
            let s = frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / channels as f32;
            s * s
        })
        .sum::<f32>();
    let rms = (energy / frames as f32).sqrt();
    if rms.is_finite() {
        level.fetch_max(rms.to_bits(), Ordering::Relaxed);
    }
}
fn append<T: cpal::Sample>(data: &[T], channels: usize, rate: u32, a: &Audio)
where
    f32: cpal::FromSample<T>,
{
    if let Ok(mut samples) = a.samples.try_lock() {
        for frame in data.chunks_exact(channels) {
            if samples.len() >= rate as usize * MAX_SECONDS {
                a.full.store(true, Ordering::Release);
                break;
            }
            samples.push(frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / channels as f32);
        }
    }
}
/// A microphone the user can choose: a stable identifier and a display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDevice {
    pub id: String,
    pub name: String,
}

/// Every input device the default host reports. Enumeration can block on the
/// audio server, so call it off the UI thread.
pub fn input_devices() -> Vec<InputDevice> {
    let Ok(devices) = cpal::default_host().input_devices() else {
        return Vec::new();
    };
    let mut devices: Vec<_> = devices
        .filter_map(|device| {
            Some(InputDevice {
                id: device.id().ok()?.to_string(),
                name: device.description().ok()?.name().to_owned(),
            })
        })
        .collect();
    devices.dedup_by(|a, b| a.id == b.id);
    devices
}

/// The id of the device the system currently records from by default.
pub fn default_input_device() -> Option<String> {
    Some(
        cpal::default_host()
            .default_input_device()?
            .id()
            .ok()?
            .to_string(),
    )
}

/// The chosen device when it is connected, the system default otherwise.
fn input_device(id: Option<&str>) -> Option<cpal::Device> {
    let host = cpal::default_host();
    id.and_then(|id| id.parse::<cpal::DeviceId>().ok())
        .and_then(|id| host.device_by_id(&id))
        .or_else(|| host.default_input_device())
}

pub struct Capture {
    stream: Option<cpal::Stream>,
    audio: Arc<Audio>,
    rate: u32,
}
impl Capture {
    pub fn start(device: Option<&str>, level: Arc<AtomicU32>) -> Result<Self> {
        let device = input_device(device).context("No microphone available")?;
        let config = device.default_input_config()?;
        let rate = config.sample_rate();
        let channels = config.channels() as usize;
        let audio = Arc::new(Audio::with_capacity(rate as usize * MAX_SECONDS));
        let a = audio.clone();
        let e = audio.clone();
        let err = move |_| e.failed.store(true, Ordering::Release);
        // Preserve the device's native configuration; convert every CPAL
        // sample representation through the same bounded mono callback.
        macro_rules! stream {
            ($sample:ty) => {
                device.build_input_stream(
                    &config.into(),
                    move |d: &[$sample], _| {
                        meter(d, channels, &level);
                        append(d, channels, rate, &a)
                    },
                    err,
                    None,
                )?
            };
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
        stream.play()?;
        Ok(Self {
            stream: Some(stream),
            audio,
            rate,
        })
    }
    pub fn ended(&self) -> bool {
        self.audio.full.load(Ordering::Acquire) || self.audio.failed.load(Ordering::Acquire)
    }
    pub fn finish(mut self) -> Result<(Vec<f32>, u32)> {
        self.stream.take();
        if self.audio.failed.load(Ordering::Acquire) {
            bail!("Microphone disconnected. Your draft is safe.")
        }
        let mut samples = self.audio.samples.lock().unwrap_or_else(|e| e.into_inner());
        Ok((std::mem::take(&mut *samples), self.rate))
    }
}

#[derive(Debug)]
pub enum Event {
    Listening,
    Finalizing,
    Final(String),
    Failed(String),
}
struct Job {
    config: cloud::Config,
    stop: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    device: Option<String>,
    events: std::sync::mpsc::SyncSender<Event>,
}
// Capture owns its stream on a separate thread. Requests remain serialized on
// WORKER, including after the user cancels.
trait Recording {
    fn ended(&self) -> bool;
    fn finish(self) -> Result<(Vec<f32>, u32)>;
}
impl Recording for Capture {
    fn ended(&self) -> bool {
        self.ended()
    }
    fn finish(self) -> Result<(Vec<f32>, u32)> {
        self.finish()
    }
}

fn record<C: Recording>(
    job: &Job,
    start: impl FnOnce() -> Result<C>,
) -> Result<Option<(Vec<f32>, u32)>> {
    if job.cancel.load(Ordering::Acquire) || job.stop.load(Ordering::Acquire) {
        return Ok(None);
    }
    let capture = start()?;
    // Stop/cancel may arrive while the audio device is opening.
    if job.cancel.load(Ordering::Acquire) {
        return Ok(None);
    }
    if !job.stop.load(Ordering::Acquire) {
        let _ = job.events.try_send(Event::Listening);
    }
    let started = std::time::Instant::now();
    while !job.stop.load(Ordering::Acquire)
        && !job.cancel.load(Ordering::Acquire)
        && !capture.ended()
        && started.elapsed().as_secs() < MAX_SECONDS as u64
    {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if job.cancel.load(Ordering::Acquire) {
        return Ok(None);
    }
    let audio = capture.finish()?; // Close the microphone before the upload.
    let _ = job.events.try_send(Event::Finalizing);
    Ok(Some(audio))
}

fn run_job<C, F>(
    job: &Job,
    start: impl FnOnce() -> Result<C> + Send,
    load: impl FnOnce() -> Result<F>,
) -> Result<Option<String>>
where
    C: Recording,
    F: FnOnce(Vec<f32>, u32) -> Result<String>,
{
    if job.cancel.load(Ordering::Acquire) {
        return Ok(None);
    }
    std::thread::scope(|scope| {
        let capture = std::thread::Builder::new()
            .name("dictation-capture".into())
            .spawn_scoped(scope, || record(job, start))?;
        // If native loading unwinds, scope teardown still has to close capture
        // before joining it. Normal cancellation uses the same signal.
        struct CancelOnDrop<'a>(&'a AtomicBool);
        impl Drop for CancelOnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let _close_capture = CancelOnDrop(&job.cancel);
        let model = load();
        // A load failure must stop capture even if the user has not pressed Stop.
        // Keep the admission guard until the stream has actually been dropped.
        if model.is_err() {
            job.cancel.store(true, Ordering::Release);
        }
        let audio = capture
            .join()
            .map_err(|_| anyhow::anyhow!("Microphone worker failed"))?;
        let transcribe = model?;
        if job.cancel.load(Ordering::Acquire) {
            return Ok(None);
        }
        let Some((samples, rate)) = audio? else {
            return Ok(Some(String::new()));
        };
        let text = transcribe(samples, rate)?;
        Ok((!job.cancel.load(Ordering::Acquire)).then_some(text))
    })
}

static BUSY: AtomicBool = AtomicBool::new(false);
// The command owns admission through capture teardown and uninterruptible
// native work. Queue failure also releases it, so rapid retries cannot wedge it.
struct BusyGuard;
impl BusyGuard {
    fn acquire() -> Result<Self> {
        if BUSY
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            bail!("Dictation is still active or finishing. Wait a moment, then try again.")
        }
        Ok(Self)
    }
}
impl Drop for BusyGuard {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::Release);
    }
}
static WORKER: std::sync::OnceLock<std::sync::mpsc::SyncSender<(Job, BusyGuard)>> =
    std::sync::OnceLock::new();
pub struct Session {
    stop: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    events: std::sync::mpsc::Receiver<Event>,
}
impl Session {
    /// `device` is an [`InputDevice::id`]; `None` or a disconnected device
    /// records from the system default.
    pub fn start(config: cloud::Config, device: Option<String>) -> Result<Self> {
        let admission = BusyGuard::acquire()?;
        let sender = WORKER.get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::sync_channel::<(Job, BusyGuard)>(1);
            std::thread::spawn(move || {
                while let Ok((job, _admission)) = rx.recv() {
                    // A panic must not take down the worker, and with it every
                    // later session, until the app restarts.
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run_job(
                            &job,
                            || Capture::start(job.device.as_deref(), job.level.clone()),
                            || Ok(|samples, rate| cloud::transcribe(&job.config, samples, rate)),
                        )
                    }))
                    .unwrap_or_else(|_| {
                        Err(anyhow::anyhow!(
                            "Dictation stopped unexpectedly. Your draft is safe."
                        ))
                    });
                    match result {
                        Ok(Some(text)) => {
                            let _ = job.events.try_send(Event::Final(text));
                        }
                        Ok(None) => {}
                        Err(e) => {
                            let _ = job.events.try_send(Event::Failed(e.to_string()));
                        }
                    }
                }
            });
            tx
        });
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let level = Arc::new(AtomicU32::new(0));
        let (tx, events) = std::sync::mpsc::sync_channel(4);
        if sender
            .try_send((
                Job {
                    config,
                    stop: stop.clone(),
                    cancel: cancel.clone(),
                    level: level.clone(),
                    device,
                    events: tx,
                },
                admission,
            ))
            .is_err()
        {
            bail!("Dictation worker unavailable")
        }
        Ok(Self {
            stop,
            cancel,
            level,
            events,
        })
    }
    pub fn poll(&mut self) -> Option<Event> {
        self.events.try_recv().ok()
    }
    pub fn finish(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
    /// Peak microphone RMS (0–1) since the previous call. Level only; the
    /// samples themselves stay on the worker.
    pub fn take_level(&self) -> f32 {
        f32::from_bits(self.level.swap(0, Ordering::Relaxed))
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}
#[cfg(test)]
fn busy() -> bool {
    BUSY.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::Sample;
    #[test]
    fn native_sample_formats_preserve_levels_and_downmix_channels() {
        fn check<T: cpal::Sample + cpal::FromSample<f32>>()
        where
            f32: cpal::FromSample<T>,
        {
            let audio = Audio::with_capacity(0);
            let input: Vec<T> = [-0.5_f32, 0.5, 0.25, 0.75, 0.0, 0.0]
                .into_iter()
                .map(|s| s.to_sample::<T>())
                .collect();
            append(&input, 2, 16_000, &audio);
            assert_eq!(*audio.samples.lock().unwrap(), vec![0.0, 0.5, 0.0]);
            let level = AtomicU32::new(0);
            meter(&input, 2, &level);
            let rms = f32::from_bits(level.load(Ordering::Relaxed));
            assert!((rms - (0.25_f32 / 3.0).sqrt()).abs() < 0.01, "{rms}");
            meter(&input[4..], 2, &level);
            assert_eq!(f32::from_bits(level.load(Ordering::Relaxed)), rms);
        }
        check::<i8>();
        check::<i16>();
        check::<cpal::I24>();
        check::<i32>();
        check::<i64>();
        check::<u8>();
        check::<u16>();
        check::<cpal::U24>();
        check::<u32>();
        check::<u64>();
        check::<f32>();
        check::<f64>();
    }
    #[test]
    fn trigger_phrases_are_checked_after_cleanup() {
        assert_eq!(trigger_problem(""), None);
        assert_eq!(trigger_problem(DEFAULT_TRIGGER_PHRASE), None);
        assert!(trigger_problem("um go").is_some());
    }
}

#[cfg(test)]
mod session_tests;
