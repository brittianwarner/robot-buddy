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
    pub fn feed(&mut self, frame: [i16; FRAME], now: Instant, busy: bool) -> Option<Clip> {
        let power = frame
            .iter()
            .map(|&s| u64::from(s.unsigned_abs()).pow(2))
            .sum::<u64>()
            / FRAME as u64;
        let threshold = self.noise_power.saturating_mul(4).max(500 * 500);
        let speech = power >= threshold * if busy { 2 } else { 1 };
        self.quiet = if speech {
            0
        } else {
            self.quiet.saturating_add(1)
        };
        if self.saturated {
            // Never interpret the tail of a truncated utterance as a new command.
            self.saturated = self.quiet < END_FRAMES;
            return None;
        }
        if self.recording.is_none() {
            if !speech {
                self.noise_power = (self.noise_power * 31 + power) / 32;
                if self.lead.len() == 8 {
                    self.lead.pop_front();
                }
                self.lead.push_back(frame);
                return None;
            }
            self.recording = Some(Clip {
                started: now,
                pcm: self.lead.drain(..).flatten().collect(),
            });
            self.voiced = 0;
        }
        let clip = self.recording.as_mut()?;
        clip.pcm.extend_from_slice(&frame);
        self.voiced += usize::from(speech);
        self.saturated = clip.pcm.len() >= MAX_FRAMES * FRAME && self.quiet < END_FRAMES;
        if !self.saturated && self.quiet < END_FRAMES {
            return None;
        }
        self.recording.take().filter(|_| self.voiced >= 8)
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
            assert!(vad.feed([volume; FRAME], now, false).is_none());
        }
        let clip = vad.feed([0; FRAME], now, false).unwrap();
        assert_eq!(clip.pcm.len(), (4 + 8 + 35 + 8 + 40) * FRAME);
        assert_eq!(clip.pcm[4 * FRAME], 750);
    }

    #[test]
    fn noise_click_and_continuous_input_are_bounded() {
        let mut vad = Vad::default();
        let now = Instant::now();
        for v in [450; 100].into_iter().chain([32_000; 1]).chain([0; 40]) {
            assert!(vad.feed([v; FRAME], now, false).is_none());
        }
        let lengths: Vec<_> = (0..2000)
            .filter_map(|_| vad.feed([i16::MIN; FRAME], now, false))
            .map(|c| c.pcm.len())
            .collect();
        assert_eq!(lengths, [MAX_FRAMES * FRAME]);
    }
}
