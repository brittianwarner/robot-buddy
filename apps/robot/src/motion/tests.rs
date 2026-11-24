use super::*;
use calibration::{Joint, Joints};
use std::{cell::RefCell, rc::Rc};
use sts3215::{GOAL, LIMITS, LOCK, MODE, PRESENT, TORQUE};

pub(super) fn fixture() -> Joints {
    std::array::from_fn(|i| Joint {
        id: [1, 2, 3][i],
        center: 2048,
        min_steps: 1888,
        max_steps: 2208,
        invert: i == 1,
        max_speed_deg_s: 45.,
        accel: 20,
    })
}

struct Mock {
    mem: [[u8; 64]; 3],
    writes: Vec<(u8, u8, Vec<u8>)>,
    goals: Vec<[u16; 3]>,
    forget: Option<(u8, u8)>,
}
impl Mock {
    fn new() -> Self {
        let mut mem = [[0; 64]; 3];
        for (ram, j) in mem.iter_mut().zip(fixture()) {
            ram[9..11].copy_from_slice(&j.min_steps.to_le_bytes());
            ram[11..13].copy_from_slice(&j.max_steps.to_le_bytes());
            ram[55] = 1;
            ram[56..58].copy_from_slice(&(j.center + 10).to_le_bytes());
        }
        Self {
            mem,
            writes: vec![],
            goals: vec![],
            forget: None,
        }
    }
}
impl Bus for Rc<RefCell<Mock>> {
    fn ping(&mut self, _: u8) -> Result<()> {
        Ok(())
    }
    fn read(&mut self, id: u8, reg: u8, out: &mut [u8]) -> Result<()> {
        let idx = usize::from(reg);
        out.copy_from_slice(&self.borrow().mem[usize::from(id - 1)][idx..idx + out.len()]);
        Ok(())
    }
    fn write(&mut self, id: u8, reg: u8, bytes: &[u8]) -> Result<()> {
        let mut m = self.borrow_mut();
        let idx = usize::from(reg);
        m.writes.push((id, reg, bytes.to_vec()));
        if m.forget != Some((id, reg)) {
            m.mem[usize::from(id - 1)][idx..idx + bytes.len()].copy_from_slice(bytes);
        }
        Ok(())
    }
    fn goals(&mut self, joints: &Joints, steps: [u16; 3]) -> Result<()> {
        for (j, p) in joints.iter().zip(steps) {
            self.write(j.id, GOAL, &p.to_le_bytes())?;
        }
        self.borrow_mut().goals.push(steps);
        Ok(())
    }
}

#[test]
fn malformed_or_missing_calibration_is_never_replaced_by_defaults() {
    assert!(calibration::load(Path::new("/nonexistent/no-calibration.json")).is_err());
    assert!(calibration::parse(b"{}").is_err());
    let joint = serde_json::json!({"id":1,"min_steps":1888,"max_steps":2208,"center":2048,"invert":false,"max_speed_deg_s":45,"accel":12});
    let mut config = serde_json::json!({"yaw":joint,"pitch":joint,"roll":joint});
    let valid = |c: &serde_json::Value| calibration::parse(&serde_json::to_vec(c).unwrap()).is_ok();
    assert!(!valid(&config));
    config["roll"]["id"] = 3.into();
    config["pitch"]["id"] = 2.into();
    assert!(valid(&config));
    for (field, value) in [
        ("max_steps", 4095),
        ("min_steps", 2050),
        ("accel", 0),
        ("max_speed_deg_s", 0),
        ("id", 254),
    ] {
        let mut bad = config.clone();
        bad["roll"][field] = value.into();
        assert!(!valid(&bad));
    }
}

#[test]
fn admission_rejects_unlocked_wrong_mode_limits_and_outside_position() {
    for (reg, value) in [(LOCK, 0), (MODE, 3), (LIMITS, 0), (PRESENT + 1, 0)] {
        let bus = Rc::new(RefCell::new(Mock::new()));
        bus.borrow_mut().mem[2][usize::from(reg)] = value;
        let mut d = Drive {
            bus: bus.clone(),
            joints: fixture(),
        };
        assert!(d.arm().is_err());
        drop(d);
        let mock = bus.borrow();
        assert!(mock.goals.is_empty());
        assert!(
            mock.writes
                .iter()
                .all(|(_, r, v)| *r == TORQUE && v == &[0])
        );
    }
}

#[test]
fn arm_uses_measured_position_and_drop_disables_every_axis() {
    let bus = Rc::new(RefCell::new(Mock::new()));
    {
        let mut d = Drive {
            bus: bus.clone(),
            joints: fixture(),
        };
        assert_eq!(d.arm().unwrap(), [2058; 3]);
        assert!(bus.borrow().mem.iter().all(|m| m[40] == 1));
    }
    assert_eq!(bus.borrow().goals, [[2058; 3]]);
    assert!(bus.borrow().mem.iter().all(|m| m[40] == 0));
}

#[test]
fn failed_arm_readback_does_not_leave_partially_enabled_neck() {
    let bus = Rc::new(RefCell::new(Mock::new()));
    bus.borrow_mut().forget = Some((2, TORQUE));
    {
        let mut d = Drive {
            bus: bus.clone(),
            joints: fixture(),
        };
        assert!(d.arm().is_err());
    }
    assert!(bus.borrow().mem.iter().all(|m| m[40] == 0));
}

#[test]
fn provision_skips_matching_eeprom_and_relocks_after_readback_error() {
    for ignore in [false, true] {
        let bus = Rc::new(RefCell::new(Mock::new()));
        bus.borrow_mut().mem[1][9] = 0;
        if ignore {
            bus.borrow_mut().forget = Some((2, LIMITS));
        }
        let mut d = Drive {
            bus: bus.clone(),
            joints: fixture(),
        };
        assert_eq!(d.provision(&AtomicBool::new(false)).is_err(), ignore);
        let mock = bus.borrow();
        assert_eq!(mock.mem[1][55], 1);
        assert!(mock.mem.iter().all(|m| m[40] == 0));
        assert_eq!(
            mock.writes.iter().filter(|(_, r, _)| *r == LIMITS).count(),
            1
        );
        assert!(
            mock.writes
                .iter()
                .all(|(id, r, _)| *r == TORQUE || *id == 2)
        );
    }
}

#[test]
fn tracking_moves_then_recenters_and_manual_pose_wins_over_gesture() {
    let now = Instant::now();
    let joints = fixture();
    let mut plan = Planner::new(joints, [2048; 3], now);
    let mut inputs = Inputs {
        face: Some((now, 1., 0.)),
        ..Inputs::default()
    };
    let sample =
        |p: &mut Planner, t: u64, s: &Inputs| p.step(now + PERIOD * u32::try_from(t).unwrap(), s);
    let mut last = [2048; 3];
    for tick in 0..1500 {
        let steps = sample(&mut plan, tick, &inputs);
        for (i, j) in joints.iter().enumerate() {
            assert!((j.min_steps..=j.max_steps).contains(&steps[i]));
            assert!(steps[i].abs_diff(last[i]) <= 11);
        }
        if tick == 100 {
            assert!(steps[0] < 1950);
        }
        last = steps;
    }
    assert!(last[0].abs_diff(2048) < 2);
    let t = now + Duration::from_secs(30);
    inputs.gesture = Some((Gesture::Look, t, 1.));
    inputs.manual = Some(([-1000., 1000., 1000.], t + Duration::from_secs(3)));
    for tick in 1500..1650 {
        last = sample(&mut plan, tick, &inputs);
    }
    assert_eq!(last, [1888, 1888, 2208]);
}

#[test]
fn provisioning_needs_explicit_consent_before_hardware_access() {
    let args = ["provision", "/absent/port", "/absent/cal"].map(String::from);
    assert!(
        servo_cli(&args)
            .unwrap_err()
            .to_string()
            .contains("--write-eeprom")
    );
}
