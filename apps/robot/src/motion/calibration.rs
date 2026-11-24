use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{io::Read, path::Path};

pub(super) const DEG_PER_STEP: f32 = 360.0 / 4096.0;
/// Planned goals stay inset from the hard EEPROM limits so servo settling jitter
/// never trips the strict out-of-envelope disarm guard. Bounded by center clearance.
pub(super) const PLAN_LIMIT_INSET_STEPS: u16 = 8;

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Joint {
    pub id: u8,
    pub center: u16,
    pub min_steps: u16,
    pub max_steps: u16,
    pub invert: bool,
    pub max_speed_deg_s: f32,
    pub accel: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Axes {
    yaw: Joint,
    pitch: Joint,
    roll: Joint,
}

pub(super) type Joints = [Joint; 3];

pub(super) fn load(path: &Path) -> Result<Joints> {
    let mut json = Vec::new();
    std::fs::File::open(path)
        .context("open per-robot neck calibration")?
        .take(8193)
        .read_to_end(&mut json)?;
    ensure!(json.len() <= 8192, "oversize calibration");
    parse(&json)
}

pub(super) fn parse(json: &[u8]) -> Result<Joints> {
    let Axes { yaw, pitch, roll } = serde_json::from_slice(json)?;
    let joints = [yaw, pitch, roll];
    for (i, j) in joints.iter().enumerate() {
        ensure!(
            (1..=253).contains(&j.id) && !joints[..i].iter().any(|other| other.id == j.id),
            "invalid or duplicate servo ID"
        );
        ensure!(
            j.min_steps < j.center && j.center < j.max_steps && j.max_steps <= 4095,
            "invalid calibration limits for ID {}",
            j.id
        );
        // Mechanical ceilings are independent of calibration. A misplaced decimal or
        // factory full-turn setting must not grant a whole revolution of travel.
        let radius = [1024, 228, 285][i];
        ensure!(
            j.center - j.min_steps <= radius && j.max_steps - j.center <= radius,
            "travel exceeds neck envelope for ID {}",
            j.id
        );
        ensure!(
            j.max_speed_deg_s.is_finite()
                && (1.0..=[120., 90., 90.][i]).contains(&j.max_speed_deg_s)
                && (1..=254).contains(&j.accel),
            "invalid speed or acceleration for ID {}",
            j.id
        );
    }
    Ok(joints)
}

impl Joint {
    fn sign(self) -> f32 {
        if self.invert { -1. } else { 1. }
    }
    pub fn degrees(self, steps: u16) -> f32 {
        (f32::from(steps) - f32::from(self.center)) * DEG_PER_STEP * self.sign()
    }
    pub fn clamp(self, angle: f32) -> f32 {
        let a = self.degrees(self.min_steps);
        let b = self.degrees(self.max_steps);
        angle.clamp(a.min(b), a.max(b))
    }
    pub fn plan_bounds_steps(self) -> (u16, u16) {
        let lo = self
            .min_steps
            .saturating_add(PLAN_LIMIT_INSET_STEPS)
            .min(self.center);
        let hi = self
            .max_steps
            .saturating_sub(PLAN_LIMIT_INSET_STEPS)
            .max(self.center);
        (lo, hi)
    }
    pub fn plan_bounds_degrees(self) -> (f32, f32) {
        let (min_steps, max_steps) = self.plan_bounds_steps();
        let a = self.degrees(min_steps);
        let b = self.degrees(max_steps);
        (a.min(b), a.max(b))
    }
    pub fn clamp_planned(self, angle: f32) -> f32 {
        let (lo, hi) = self.plan_bounds_degrees();
        angle.clamp(lo, hi)
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "rounded raw position is clamped to validated 12-bit limits"
    )]
    pub fn steps(self, degrees: f32) -> u16 {
        (f32::from(self.center) + degrees * self.sign() / DEG_PER_STEP)
            .round()
            .clamp(f32::from(self.min_steps), f32::from(self.max_steps)) as u16
    }
}
