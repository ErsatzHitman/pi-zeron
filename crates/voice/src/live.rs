//! The live preview while dictating. The recording is cut where the speaker
//! pauses, each phrase is transcribed as soon as it ends (one request at a
//! time; phrases that end meanwhile go together) and the text so far shows
//! in the draft. When the recording stops, the transcript of all of it
//! replaces the preview, so a phrase the preview missed is never lost.
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

const FRAME_MS: usize = 20;
/// A phrase needs this much speech...
const MIN_SPEECH_MS: usize = 300;
/// ...and ends at a pause this long.
const PAUSE_MS: usize = 450;
/// Speech with no pause is cut here anyway.
const MAX_PHRASE_MS: usize = 12_000;
/// Silence kept ahead of the first word.
const LEAD_MS: usize = 200;
/// How often the recording loop hands the preview what it captured.
pub(crate) const FEED: Duration = Duration::from_millis(100);

/// Transcribes one stretch of audio at a sample rate.
pub(crate) type Transcribe = Arc<dyn Fn(Vec<f32>, u32) -> anyhow::Result<String> + Send + Sync>;

/// Cuts a recording into phrases at the speaker's pauses. A frame is speech
/// when it is well above the room's noise floor, which it learns as it goes.
pub(crate) struct Segmenter {
    frame: usize,
    /// The samples since the last cut.
    phrase: Vec<f32>,
    /// How many of them have been judged, a whole frame at a time.
    judged: usize,
    /// Speech frames in the phrase, and silent frames at its end.
    speech: usize,
    quiet: usize,
    floor: f32,
}

impl Segmenter {
    pub fn new(rate: u32) -> Self {
        Self {
            frame: (rate as usize * FRAME_MS / 1000).max(1),
            phrase: Vec::new(),
            judged: 0,
            speech: 0,
            quiet: 0,
            floor: 0.002,
        }
    }

    /// Take in more of the recording; the phrases that ended come back.
    pub fn push(&mut self, samples: &[f32]) -> Vec<Vec<f32>> {
        self.phrase.extend_from_slice(samples);
        let mut ended = Vec::new();
        while self.phrase.len() - self.judged >= self.frame {
            let frame = &self.phrase[self.judged..self.judged + self.frame];
            let rms = (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt();
            self.judged += self.frame;
            if rms > (self.floor * 3.0).max(0.01) {
                self.speech += 1;
                self.quiet = 0;
            } else {
                self.quiet += 1;
                // The floor follows the room: down at once, up slowly.
                self.floor = if rms < self.floor {
                    rms.max(1e-4)
                } else {
                    self.floor + (rms - self.floor) * 0.05
                };
            }
            let speech_ms = self.speech * FRAME_MS;
            let quiet_ms = self.quiet * FRAME_MS;
            // A click or a cough is not a phrase: past a long pause it is
            // forgotten with the silence around it.
            if self.speech == 0 || (speech_ms < MIN_SPEECH_MS && quiet_ms >= 2 * PAUSE_MS) {
                self.speech = 0;
                let keep = self.frame * (LEAD_MS / FRAME_MS);
                if self.judged > 2 * keep {
                    let drop = self.judged - keep;
                    self.phrase.drain(..drop);
                    self.judged -= drop;
                }
                continue;
            }
            let length_ms = self.judged / self.frame * FRAME_MS;
            if (speech_ms >= MIN_SPEECH_MS && quiet_ms >= PAUSE_MS) || length_ms >= MAX_PHRASE_MS {
                let rest = self.phrase.split_off(self.judged);
                ended.push(std::mem::replace(&mut self.phrase, rest));
                self.judged = 0;
                self.speech = 0;
                self.quiet = 0;
            }
        }
        ended
    }
}

/// Start previewing a recording at `rate`: send it what is captured, as it
/// is captured, and `show` gets the text so far after each phrase. At most
/// one request goes every `gap`. Dropping the sender ends the preview; what
/// was not transcribed by then is left to the final transcript.
pub(crate) fn start(
    rate: u32,
    gap: Duration,
    transcribe: Transcribe,
    show: impl Fn(String) + Send + 'static,
) -> mpsc::Sender<Vec<f32>> {
    let (tx, rx) = mpsc::channel();
    // Without a thread the receiver is gone and sends simply fail: the
    // dictation goes on with no preview.
    let _ = std::thread::Builder::new()
        .name("dictation-preview".into())
        .spawn(move || run(&rx, rate, gap, &*transcribe, &show));
    tx
}

fn run(
    rx: &mpsc::Receiver<Vec<f32>>,
    rate: u32,
    gap: Duration,
    transcribe: &(dyn Fn(Vec<f32>, u32) -> anyhow::Result<String> + Send + Sync),
    show: &dyn Fn(String),
) {
    let mut cut = Segmenter::new(rate);
    let mut waiting: Vec<f32> = Vec::new();
    let mut text = String::new();
    let mut last: Option<Instant> = None;
    loop {
        match rx.recv_timeout(FEED) {
            Ok(samples) => {
                for phrase in cut.push(&samples) {
                    waiting.extend(phrase);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        if waiting.is_empty() || last.is_some_and(|at| at.elapsed() < gap) {
            continue;
        }
        last = Some(Instant::now());
        match transcribe(std::mem::take(&mut waiting), rate) {
            Ok(phrase) => {
                let phrase = phrase.trim();
                if !phrase.is_empty() {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(phrase);
                    show(text.clone());
                }
            }
            // A failed preview (a rate limit, say) ends the preview, so it
            // never takes the requests the final transcript needs.
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const RATE: u32 = 16_000;

    fn speech(ms: usize) -> Vec<f32> {
        (0..RATE as usize * ms / 1000)
            .map(|i| 0.2 * (i as f32 * 0.05).sin())
            .collect()
    }

    fn silence(ms: usize) -> Vec<f32> {
        vec![0.0005; RATE as usize * ms / 1000]
    }

    #[test]
    fn a_phrase_ends_at_a_pause_after_enough_speech() {
        let mut cut = Segmenter::new(RATE);
        assert!(cut.push(&silence(1_000)).is_empty());
        assert!(cut.push(&speech(800)).is_empty());
        assert!(cut.push(&silence(200)).is_empty(), "a short breath goes on");
        assert!(cut.push(&speech(400)).is_empty());
        let ended = cut.push(&silence(500));
        assert_eq!(ended.len(), 1);
        // The lead-in silence was trimmed; the phrase is its speech and pauses.
        let ms = ended[0].len() * 1000 / RATE as usize;
        assert!((2_000..=2_300).contains(&ms), "{ms}");
        assert!(
            cut.push(&silence(2_000)).is_empty(),
            "silence alone is no phrase"
        );
    }

    #[test]
    fn a_click_is_forgotten_and_long_speech_is_cut_anyway() {
        let mut cut = Segmenter::new(RATE);
        cut.push(&speech(60));
        assert!(cut.push(&silence(1_500)).is_empty());
        assert!(cut.push(&speech(250)).is_empty());
        assert!(cut.push(&silence(1_000)).is_empty(), "too little speech");
        let ended = cut.push(&speech(13_000));
        assert_eq!(ended.len(), 1);
        let ms = ended[0].len() * 1000 / RATE as usize;
        assert!(ms <= MAX_PHRASE_MS + FRAME_MS, "{ms}");
    }

    #[test]
    fn the_preview_grows_phrase_by_phrase_and_stops_at_a_failure() {
        let (tx, rx) = mpsc::channel();
        let shown = Arc::new(Mutex::new(Vec::<String>::new()));
        let calls = Arc::new(Mutex::new(0));
        let worker = {
            let shown = shown.clone();
            let calls = calls.clone();
            std::thread::spawn(move || {
                let transcribe = move |samples: Vec<f32>, rate: u32| {
                    assert_eq!(rate, RATE);
                    let mut calls = calls.lock().unwrap();
                    *calls += 1;
                    match *calls {
                        1 => Ok(format!(
                            "phrase of {} ms",
                            samples.len() * 1000 / RATE as usize
                        )),
                        2 => Ok("  ".to_owned()),
                        3 => Ok("second".to_owned()),
                        _ => Err(anyhow::anyhow!("429")),
                    }
                };
                run(&rx, RATE, Duration::ZERO, &transcribe, &|text| {
                    shown.lock().unwrap().push(text)
                });
            })
        };
        let wait_calls = |n: usize| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while *calls.lock().unwrap() < n {
                assert!(Instant::now() < deadline, "call {n}");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let mut phrase = speech(500);
        phrase.extend(silence(500));
        tx.send(phrase.clone()).unwrap();
        wait_calls(1);
        tx.send(phrase.clone()).unwrap();
        wait_calls(2);
        tx.send(phrase.clone()).unwrap();
        wait_calls(3);
        tx.send(phrase.clone()).unwrap();
        wait_calls(4);
        // After the failure nothing more is asked.
        worker.join().unwrap();
        let shown = shown.lock().unwrap().clone();
        assert_eq!(shown.len(), 2, "{shown:?}");
        assert!(shown[0].starts_with("phrase of "), "{shown:?}");
        assert_eq!(shown[1], format!("{} second", shown[0]));
        assert!(tx.send(phrase).is_err(), "the preview is gone");
    }
}
