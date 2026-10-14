//! Turn detection on this device: 20 ms energy frames against an adaptive
//! noise floor. Speech starts a turn, a pause ends it. While the assistant
//! is speaking there is no echo canceller, so only louder, longer speech
//! counts as the user talking over it.
use std::{collections::VecDeque, time::Duration};

const FRAME: Duration = Duration::from_millis(20);
/// Audio kept from before speech was detected, so first syllables survive.
const PREROLL: Duration = Duration::from_millis(300);
/// Voiced frames in a row that start a turn, quiet and over playback.
const START_FRAMES: u32 = 3;
const START_FRAMES_OVER_PLAYBACK: u32 = 8;
/// Voiced audio that makes a turn real: shorter bursts are coughs and clicks.
const CONFIRM: Duration = Duration::from_millis(300);
const MAX_TURN: Duration = Duration::from_secs(30);
const MIN_THRESHOLD: f32 = 0.012;
const MIN_THRESHOLD_OVER_PLAYBACK: f32 = 0.05;

#[derive(Debug, PartialEq)]
pub(crate) enum TurnEvent {
    /// Speech may have begun: duck playback.
    Started,
    /// It is speech: interrupt the assistant.
    Confirmed,
    /// The turn ended with this audio, at the detector's rate.
    Ended(Vec<f32>),
    /// The burst was too short to be speech.
    Dropped,
}

pub(crate) struct Turns {
    rate: u32,
    frame: usize,
    end_frames: u32,
    pending: Vec<f32>,
    noise: f32,
    preroll: VecDeque<f32>,
    /// `Some` while a turn is underway.
    utterance: Option<Vec<f32>>,
    voiced_run: u32,
    silent_run: u32,
    voiced: u32,
    confirmed: bool,
}

impl Turns {
    pub fn new(rate: u32, end_silence: Duration) -> Self {
        let frame = (rate as usize * FRAME.as_millis() as usize / 1000).max(1);
        Self {
            rate,
            frame,
            end_frames: frames(end_silence).max(5),
            pending: Vec::with_capacity(frame),
            noise: 0.004,
            preroll: VecDeque::new(),
            utterance: None,
            voiced_run: 0,
            silent_run: 0,
            voiced: 0,
            confirmed: false,
        }
    }

    pub fn active(&self) -> bool {
        self.utterance.is_some()
    }

    /// Forget any turn in progress (muting, or the call ending).
    pub fn reset(&mut self) {
        self.pending.clear();
        self.preroll.clear();
        self.utterance = None;
        self.voiced_run = 0;
        self.silent_run = 0;
        self.voiced = 0;
        self.confirmed = false;
    }

    /// Feed captured mono samples. `playing` is whether the assistant's
    /// voice is coming out of the speaker right now.
    pub fn push(&mut self, samples: &[f32], playing: bool) -> Vec<TurnEvent> {
        let mut events = Vec::new();
        for &s in samples {
            self.pending.push(s);
            if self.pending.len() == self.frame {
                let frame = std::mem::take(&mut self.pending);
                self.frame_done(&frame, playing, &mut events);
                self.pending = frame;
                self.pending.clear();
            }
        }
        events
    }

    fn frame_done(&mut self, frame: &[f32], playing: bool, events: &mut Vec<TurnEvent>) {
        let rms = (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt();
        let rms = if rms.is_finite() { rms } else { 0.0 };
        let threshold = if playing {
            (self.noise * 6.0).max(MIN_THRESHOLD_OVER_PLAYBACK)
        } else {
            (self.noise * 3.0).max(MIN_THRESHOLD)
        };
        let loud = rms > threshold;
        match &mut self.utterance {
            None => {
                self.preroll.extend(frame.iter().copied());
                let keep = self.rate as usize * PREROLL.as_millis() as usize / 1000;
                while self.preroll.len() > keep {
                    self.preroll.pop_front();
                }
                if loud {
                    self.voiced_run += 1;
                } else {
                    self.voiced_run = 0;
                    // Playback would teach the floor the assistant's voice.
                    if !playing {
                        self.noise = (self.noise * 0.95 + rms * 0.05).clamp(0.001, 0.05);
                    }
                }
                let needed = if playing {
                    START_FRAMES_OVER_PLAYBACK
                } else {
                    START_FRAMES
                };
                if self.voiced_run >= needed {
                    self.utterance = Some(self.preroll.drain(..).collect());
                    self.voiced = self.voiced_run;
                    self.silent_run = 0;
                    self.confirmed = false;
                    events.push(TurnEvent::Started);
                }
            }
            Some(utterance) => {
                utterance.extend_from_slice(frame);
                if loud {
                    self.voiced += 1;
                    self.silent_run = 0;
                } else {
                    self.silent_run += 1;
                }
                if !self.confirmed && self.voiced >= frames(CONFIRM) {
                    self.confirmed = true;
                    events.push(TurnEvent::Confirmed);
                }
                let too_long = utterance.len() >= self.rate as usize * MAX_TURN.as_secs() as usize;
                if self.silent_run >= self.end_frames || too_long {
                    let audio = self.utterance.take().unwrap_or_default();
                    let confirmed = self.confirmed;
                    self.reset();
                    events.push(if confirmed {
                        TurnEvent::Ended(audio)
                    } else {
                        TurnEvent::Dropped
                    });
                }
            }
        }
    }
}

fn frames(duration: Duration) -> u32 {
    (duration.as_millis() / FRAME.as_millis()) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 16_000;

    fn tone(ms: u32, amplitude: f32) -> Vec<f32> {
        (0..RATE * ms / 1000)
            .map(|i| amplitude * (std::f32::consts::TAU * 220.0 * i as f32 / RATE as f32).sin())
            .collect()
    }

    #[test]
    fn speech_then_a_pause_is_one_turn_with_its_first_syllable() {
        let mut turns = Turns::new(RATE, Duration::from_millis(600));
        assert!(turns.push(&tone(500, 0.0005), false).is_empty());
        let events = turns.push(&tone(800, 0.2), false);
        assert_eq!(events, vec![TurnEvent::Started, TurnEvent::Confirmed]);
        let events = turns.push(&tone(700, 0.0005), false);
        let [TurnEvent::Ended(audio)] = events.as_slice() else {
            panic!("{events:?}")
        };
        // Pre-roll, the speech and the closing pause.
        assert!(
            audio.len() >= (RATE as usize * 1400 / 1000),
            "{}",
            audio.len()
        );
        assert!(!turns.active());
    }

    #[test]
    fn a_short_click_is_dropped() {
        let mut turns = Turns::new(RATE, Duration::from_millis(600));
        turns.push(&tone(400, 0.0005), false);
        let mut events = turns.push(&tone(100, 0.3), false);
        events.extend(turns.push(&tone(800, 0.0005), false));
        assert_eq!(events, vec![TurnEvent::Started, TurnEvent::Dropped]);
    }

    #[test]
    fn playback_needs_louder_longer_speech() {
        let mut turns = Turns::new(RATE, Duration::from_millis(600));
        turns.push(&tone(400, 0.0005), false);
        // Echo-level audio over playback is not the user.
        assert!(turns.push(&tone(1000, 0.03), true).is_empty());
        let events = turns.push(&tone(600, 0.3), true);
        assert_eq!(events, vec![TurnEvent::Started, TurnEvent::Confirmed]);
    }

    #[test]
    fn steady_noise_raises_the_floor() {
        let mut turns = Turns::new(RATE, Duration::from_millis(600));
        // Fan noise just under the starting threshold for a while…
        turns.push(&tone(3000, 0.011), false);
        // …then the same level is no longer near the threshold.
        assert!(turns.noise > 0.006, "{}", turns.noise);
        assert!(turns.push(&tone(1000, 0.012), false).is_empty());
    }

    #[test]
    fn a_turn_never_outgrows_the_limit() {
        let mut turns = Turns::new(RATE, Duration::from_millis(600));
        turns.push(&tone(200, 0.0005), false);
        let events = turns.push(&tone(31_000, 0.2), false);
        assert!(
            matches!(events.last(), Some(TurnEvent::Ended(_))),
            "{events:?}"
        );
    }
}
