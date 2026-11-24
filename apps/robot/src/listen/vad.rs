//! Endpoint one 16 kHz beam. All limits use sample counts, not scheduler timing.
use std::{collections::VecDeque, time::Instant};

pub(super) const RATE: u32 = 16_000;
pub(super) const FRAME: usize = 320; // 20 ms
const MAX_FRAMES: usize = 600;
const END_FRAMES: usize = 40;

pub(super) struct Clip {
    pub started: Instant,
    pub pcm: Vec<i16>,
}

#[derive(Default)]
pub(super) struct Vad {
    lead: VecDeque<[i16; FRAME]>,
    recording: Option<Clip>,
    voiced: usize,
    quiet: usize,
    noise_power: u64,
    saturated: bool,
}

impl Vad {
    /// Drop any in-flight utterance and pre-roll while preserving the learned noise floor.
    pub fn reset(&mut self) {
        self.lead.clear();
        self.recording = None;
        self.voiced = 0;
        self.quiet = 0;
        self.saturated = false;
    }

    pub fn feed(&mut self, frame: &[i16; FRAME], now: Instant, busy: bool) -> Option<Clip> {
        let power = frame
            .iter()
            .map(|&s| u64::from(s.unsigned_abs()).pow(2))
            .sum::<u64>()
            / FRAME as u64;
        let threshold = self.noise_power.saturating_mul(4).max(500 * 500);
        let speech = power >= threshold.saturating_mul(if busy { 2 } else { 1 });
        self.quiet = if speech {
            0
        } else {
            self.quiet.saturating_add(1)
        };
        if self.saturated {
            // Never interpret the tail of a truncated utterance as a new command,
            // while still tracking the ambient floor so steady >500 RMS cannot lock out speech.
            self.noise_power = (self.noise_power * 31 + power) / 32;
            self.saturated = self.quiet < END_FRAMES;
            return None;
        }
        if self.recording.is_none() {
            if !speech {
                self.noise_power = (self.noise_power * 31 + power) / 32;
                if self.lead.len() == 8 {
                    self.lead.pop_front();
                }
                self.lead.push_back(*frame);
                return None;
            }
            self.recording = Some(Clip {
                started: now,
                pcm: self.lead.drain(..).flatten().collect(),
            });
            self.voiced = 0;
        }
        let clip = self.recording.as_mut()?;
        clip.pcm.extend_from_slice(frame);
        self.voiced += usize::from(speech);
        self.saturated = clip.pcm.len() >= MAX_FRAMES * FRAME && self.quiet < END_FRAMES;
        if !self.saturated && self.quiet < END_FRAMES {
            return None;
        }
        let clip = self.recording.take().filter(|_| self.voiced >= 8);
        if !self.saturated {
            self.reset();
        }
        clip
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speech_keeps_onset_and_waits_through_short_pauses() {
        let mut vad = Vad::default();
        let now = Instant::now();
        for volume in [0; 4]
            .into_iter()
            .chain([750; 8])
            .chain([0; 35])
            .chain([750; 8])
            .chain([0; 39])
        {
            assert!(vad.feed(&[volume; FRAME], now, false).is_none());
        }
        let clip = vad.feed(&[0; FRAME], now, false).unwrap();
        assert_eq!(clip.pcm.len(), (4 + 8 + 35 + 8 + 40) * FRAME);
        assert_eq!(clip.pcm[4 * FRAME], 750);
    }

    #[test]
    fn noise_click_and_continuous_input_are_bounded() {
        let mut vad = Vad::default();
        let now = Instant::now();
        for v in [450; 100].into_iter().chain([32_000; 1]).chain([0; 40]) {
            assert!(vad.feed(&[v; FRAME], now, false).is_none());
        }
        let lengths: Vec<_> = (0..2000)
            .filter_map(|_| vad.feed(&[i16::MIN; FRAME], now, false))
            .map(|c| c.pcm.len())
            .collect();
        assert_eq!(lengths, [MAX_FRAMES * FRAME]);
    }

    #[test]
    fn sustained_ambient_above_500_rms_recovers_for_speech() {
        let mut vad = Vad::default();
        let now = Instant::now();
        let ambient_clips: Vec<_> = (0..700)
            .filter_map(|_| vad.feed(&[600; FRAME], now, false))
            .map(|c| c.pcm.len())
            .collect();
        assert_eq!(ambient_clips, [MAX_FRAMES * FRAME]);

        for volume in [2_000; 12].into_iter().chain([600; END_FRAMES - 1]) {
            assert!(vad.feed(&[volume; FRAME], now, false).is_none());
        }
        let speech = vad.feed(&[600; FRAME], now, false).unwrap();
        assert_eq!(speech.pcm.len(), (8 + 12 + END_FRAMES) * FRAME);
        assert_eq!(speech.pcm[8 * FRAME], 2_000);
    }

    #[test]
    fn reset_preserves_noise_floor_and_clears_inflight_recording() {
        let mut vad = Vad::default();
        let now = Instant::now();
        for _ in 0..700 {
            let _ = vad.feed(&[600; FRAME], now, false);
        }
        // Start an in-flight utterance, then reset as on a state/mode transition.
        for _ in 0..4 {
            assert!(vad.feed(&[2_000; FRAME], now, false).is_none());
        }
        vad.reset();

        // Ambient 600 stays below the preserved floor and does not trigger a new clip.
        for _ in 0..20 {
            assert!(vad.feed(&[600; FRAME], now, false).is_none());
        }
        for volume in [2_000; 10].into_iter().chain([600; END_FRAMES - 1]) {
            assert!(vad.feed(&[volume; FRAME], now, false).is_none());
        }
        let speech = vad.feed(&[600; FRAME], now, false).unwrap();
        assert_eq!(speech.pcm.len(), (8 + 10 + END_FRAMES) * FRAME);
        assert_eq!(speech.pcm[8 * FRAME], 2_000);

        // Reset also clears saturation without discarding the learned noise floor.
        for _ in 0..MAX_FRAMES {
            let _ = vad.feed(&[3_000; FRAME], now, false);
        }
        assert!(vad.saturated);
        let floor = vad.noise_power;
        vad.reset();
        assert!(!vad.saturated);
        assert_eq!(vad.noise_power, floor);
    }
}
