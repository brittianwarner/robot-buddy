//! The only path to torque enable; maintenance and service share the same checks.
use super::{
    calibration::{Joint, Joints},
    sts3215::{Bus, GOAL, LIMITS, LOCK, MODE, PRESENT, TORQUE},
};
use anyhow::{Context, Result, ensure};
use std::time::Duration;

pub(super) const HEALTH_STEPS: usize = 15;
const CONFIRM_ATTEMPTS: usize = 4;
const CONFIRM_RETRY_DELAY: Duration = Duration::from_millis(10);

fn read<const N: usize>(bus: &mut impl Bus, id: u8, reg: u8) -> Result<[u8; N]> {
    let mut bytes = [0; N];
    bus.read(id, reg, &mut bytes)?;
    Ok(bytes)
}
fn check(bus: &mut impl Bus, id: u8, reg: u8, expected: &[u8]) -> Result<()> {
    let mut actual = vec![0; expected.len()];
    bus.read(id, reg, &mut actual)?;
    ensure!(
        actual == expected,
        "servo {id} register {reg} mismatch: got {actual:?}, expected {expected:?}"
    );
    Ok(())
}
/// Bounded read-only confirmation retries for a write already transmitted. Never re-sends writes.
fn confirm(bus: &mut impl Bus, id: u8, reg: u8, expected: &[u8]) -> Result<()> {
    let mut last = None;
    for attempt in 0..CONFIRM_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(CONFIRM_RETRY_DELAY);
        }
        match check(bus, id, reg, expected) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    Err(last.expect("CONFIRM_ATTEMPTS > 0"))
}
fn store(bus: &mut impl Bus, id: u8, reg: u8, data: &[u8]) -> Result<()> {
    bus.write(id, reg, data)?;
    if reg == LIMITS || reg == LOCK {
        std::thread::sleep(Duration::from_millis(10));
    }
    confirm(bus, id, reg, data)
}
fn limits(joint: Joint) -> [u8; 4] {
    let [min_low, min_high] = joint.min_steps.to_le_bytes();
    let [max_low, max_high] = joint.max_steps.to_le_bytes();
    [min_low, min_high, max_low, max_high]
}
fn present(bus: &mut impl Bus, j: Joint) -> Result<u16> {
    let pos = u16::from_le_bytes(read(bus, j.id, PRESENT)?);
    ensure!(
        (j.min_steps..=j.max_steps).contains(&pos),
        "servo {} at {pos}, outside calibrated range; reposition with torque off",
        j.id
    );
    Ok(pos)
}

pub(super) struct Drive<B: Bus> {
    pub bus: B,
    pub joints: Joints,
}

impl<B: Bus> Drive<B> {
    // Try every ID before reporting a failure: one disconnected joint must never
    // prevent the other two from receiving the disable command.
    pub fn disable(&mut self) -> Result<()> {
        let mut failure = None;
        for j in self.joints {
            if let Err(e) = self.bus.write(j.id, TORQUE, &[0]) {
                failure = Some(e);
            }
        }
        for j in self.joints {
            if let Err(e) = confirm(&mut self.bus, j.id, TORQUE, &[0]) {
                failure = Some(e);
            }
        }
        failure.map_or(Ok(()), Err)
    }
    pub fn verify_joint(&mut self, i: usize) -> Result<u16> {
        let j = self.joints[i];
        check(&mut self.bus, j.id, MODE, &[0])?;
        check(&mut self.bus, j.id, LOCK, &[1])?;
        check(&mut self.bus, j.id, LIMITS, &limits(j))?;
        present(&mut self.bus, j)
    }
    pub fn arm(&mut self) -> Result<[u16; 3]> {
        self.disable()?;
        let mut current = [0; 3];
        for (i, p) in current.iter_mut().enumerate() {
            *p = self.verify_joint(i)?;
        }
        // The old target in RAM can predate the last shutdown. Hold the measured
        // pose and verify the addressed goals before enabling a single motor.
        self.bus.goals(&self.joints, current)?;
        for (j, p) in self.joints.iter().zip(current) {
            confirm(&mut self.bus, j.id, GOAL, &p.to_le_bytes())?;
        }
        for j in self.joints {
            store(&mut self.bus, j.id, TORQUE, &[1])?;
        }
        Ok(current)
    }
    /// Staggered single-transaction health probe (3 axes x 5 checks = `HEALTH_STEPS`).
    pub fn health_step(&mut self, step: usize) -> Result<()> {
        let j = self.joints[step % 3];
        match (step / 3) % 5 {
            0 => present(&mut self.bus, j).map(|_| ()),
            1 => check(&mut self.bus, j.id, TORQUE, &[1]),
            2 => check(&mut self.bus, j.id, MODE, &[0]),
            3 => check(&mut self.bus, j.id, LOCK, &[1]),
            4 => check(&mut self.bus, j.id, LIMITS, &limits(j)),
            _ => unreachable!(),
        }
    }
    pub fn provision(&mut self, stop: &std::sync::atomic::AtomicBool) -> Result<()> {
        self.disable()?;
        // Preflight the entire bus before writing any EEPROM. Require position
        // mode and physical placement within the proposed range on all axes.
        for j in self.joints {
            check(&mut self.bus, j.id, MODE, &[0])?;
            present(&mut self.bus, j)?;
        }
        for j in self.joints {
            ensure!(
                !stop.load(std::sync::atomic::Ordering::Acquire),
                "EEPROM provisioning interrupted"
            );
            let wanted = limits(j);
            if read::<4>(&mut self.bus, j.id, LIMITS)? == wanted {
                if read::<1>(&mut self.bus, j.id, LOCK)? != [1] {
                    store(&mut self.bus, j.id, LOCK, &[1])?;
                }
                continue;
            }
            let result = store(&mut self.bus, j.id, LOCK, &[0])
                .and_then(|()| store(&mut self.bus, j.id, LIMITS, &wanted));
            // Even an unlock/readback timeout might have unlocked the device.
            store(&mut self.bus, j.id, LOCK, &[1])
                .context("could not confirm EEPROM relock; inspect servo before use")?;
            result?;
        }
        for i in 0..3 {
            self.verify_joint(i)?;
        }
        Ok(())
    }
}
impl<B: Bus> Drop for Drive<B> {
    fn drop(&mut self) {
        if let Err(e) = self.disable() {
            tracing::error!(error=%e,"could not confirm neck torque off");
        }
    }
}
