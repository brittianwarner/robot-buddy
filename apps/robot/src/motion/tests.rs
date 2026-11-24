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
            let (lo, hi) = j.plan_bounds_steps();
            assert!((lo..=hi).contains(&steps[i]));
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
    let inset = calibration::PLAN_LIMIT_INSET_STEPS;
    assert_eq!(last, [1888 + inset, 1888 + inset, 2208 - inset]);
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

#[test]
fn loop_timing_tracks_p95_and_missed_20ms_deadlines_across_windows() {
    let origin = Instant::now();
    let mut timing = LoopTiming::new(origin);
    assert_eq!(timing.flush(origin), None);

    // 20 on-time ticks: cycle_us = 1000..=20000 step 1000 (last is 19_500 so no miss).
    for i in 0..20u64 {
        let scheduled = origin + PERIOD * u32::try_from(i).unwrap();
        let started = scheduled + Duration::from_micros(200);
        let cycle = if i == 19 { 19_500 } else { (i + 1) * 1_000 };
        let finished = scheduled + Duration::from_micros(cycle);
        assert_eq!(timing.record(scheduled, started, finished), None);
    }
    let first = timing.flush(origin + Duration::from_millis(400)).unwrap();
    assert_eq!(
        first,
        TimingReport {
            ticks: 20,
            missed: 0,
            skipped_slots: 0,
            total_ticks: 20,
            total_missed: 0,
            total_skipped_slots: 0,
            p95_cycle_us: 19_000,
            max_cycle_us: 19_500,
            max_work_us: 19_300,
            max_overrun_us: 0,
        }
    );

    // Second window: one exact-boundary miss (20 ms -> 1 late cycle, 1 skipped slot) and
    // one multi-slot stall (55 ms -> 1 late cycle, 2 skipped 20 ms slots, 35 ms overrun),
    // plus an on-time tick past TIMING_REPORT_INTERVAL that triggers automatic emission.
    let w2 = origin + Duration::from_millis(400);
    assert_eq!(
        timing.record(
            w2,
            w2 + Duration::from_micros(500),
            w2 + Duration::from_millis(20),
        ),
        None
    );
    assert_eq!(
        timing.record(
            w2 + PERIOD,
            w2 + PERIOD + Duration::from_millis(2),
            w2 + PERIOD + Duration::from_millis(55),
        ),
        None
    );
    let end = w2 + TIMING_REPORT_INTERVAL;
    let second = timing
        .record(
            end,
            end + Duration::from_micros(100),
            end + Duration::from_millis(1),
        )
        .unwrap();
    assert_eq!(
        second,
        TimingReport {
            ticks: 3,
            missed: 2,
            skipped_slots: 3,
            total_ticks: 23,
            total_missed: 2,
            total_skipped_slots: 3,
            p95_cycle_us: 55_000,
            max_cycle_us: 55_000,
            max_work_us: 53_000,
            max_overrun_us: 35_000,
        }
    );
    assert_eq!(timing.flush(end), None);
}

#[test]
fn run_emits_missed_deadline_warning_when_bus_exceeds_20ms_period() {
    use std::collections::BTreeMap;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::prelude::*;

    struct SlowBus {
        inner: Rc<RefCell<Mock>>,
        stop: Arc<AtomicBool>,
        delay_once: bool,
    }
    impl Bus for SlowBus {
        fn ping(&mut self, id: u8) -> Result<()> {
            self.inner.ping(id)
        }
        fn read(&mut self, id: u8, reg: u8, out: &mut [u8]) -> Result<()> {
            self.inner.read(id, reg, out)
        }
        fn write(&mut self, id: u8, reg: u8, bytes: &[u8]) -> Result<()> {
            self.inner.write(id, reg, bytes)
        }
        fn goals(&mut self, joints: &Joints, steps: [u16; 3]) -> Result<()> {
            self.inner.goals(joints, steps)?;
            if self.delay_once {
                self.delay_once = false;
                thread::sleep(Duration::from_millis(25));
                self.stop.store(true, Ordering::Release);
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct Fields(BTreeMap<String, String>);
    impl Visit for Fields {
        fn record_u64(&mut self, field: &Field, value: u64) {
            self.0.insert(field.name().into(), value.to_string());
        }
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.insert(
                field.name().into(),
                format!("{value:?}").trim_matches('"').into(),
            );
        }
    }

    type CapturedEvents = Arc<Mutex<Vec<(tracing::Level, BTreeMap<String, String>)>>>;

    struct Capture(CapturedEvents);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = Fields::default();
            event.record(&mut fields);
            self.0
                .lock()
                .unwrap()
                .push((*event.metadata().level(), fields.0));
        }
    }

    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(Capture(events.clone()));
    let mock = Rc::new(RefCell::new(Mock::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let mut drive = Drive {
        bus: SlowBus {
            inner: mock,
            stop: stop.clone(),
            delay_once: false,
        },
        joints: fixture(),
    };
    let start = drive.arm().unwrap();
    // Arm the slow tick only after `arm()` finishes its initial hold-pose write.
    drive.bus.delay_once = true;
    let input = MotionInput::default();
    tracing::subscriber::with_default(subscriber, || {
        run(&mut drive, &input, &stop, start).unwrap();
    });
    let recorded = events.lock().unwrap();
    let (level, fields) = recorded
        .iter()
        .find(|(_, f)| {
            f.get("message").map(String::as_str) == Some("neck loop missed 20ms deadline")
        })
        .expect("expected missed deadline timing event");
    assert_eq!(*level, tracing::Level::WARN);
    assert_eq!(fields.get("period_ms").map(String::as_str), Some("20"));
    assert_eq!(fields.get("missed").map(String::as_str), Some("1"));
    assert_eq!(fields.get("total_missed").map(String::as_str), Some("1"));
    assert!(
        fields
            .get("skipped_slots")
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|v| v >= 1),
        "expected >=1 skipped 20ms slot: {fields:?}"
    );
    assert_eq!(fields.get("ticks").map(String::as_str), Some("1"));
    assert!(
        fields
            .get("max_overrun_us")
            .and_then(|v| v.parse::<u32>().ok())
            .is_some_and(|v| v >= 4_000),
        "expected >=4ms overrun: {fields:?}"
    );
}

#[test]
fn planned_goals_stay_inset_bounded_by_center_clearance_while_initial_goal_is_preserved() {
    // Narrow calibration: center clearance smaller than PLAN_LIMIT_INSET_STEPS never inverts.
    let narrow = Joint {
        id: 1,
        center: 2048,
        min_steps: 2045,
        max_steps: 2051,
        invert: false,
        max_speed_deg_s: 45.,
        accel: 20,
    };
    assert_eq!(narrow.plan_bounds_steps(), (2048, 2048));
    let asymmetric = Joint {
        min_steps: 2045,
        max_steps: 2208,
        ..narrow
    };
    assert_eq!(
        asymmetric.plan_bounds_steps(),
        (2048, 2208 - calibration::PLAN_LIMIT_INSET_STEPS)
    );

    // Arming right at hard EEPROM limits preserves the exact measured initial GOAL
    // and enforces strict out-of-envelope disarm on crossed limits.
    let joints = fixture();
    let bus = Rc::new(RefCell::new(Mock::new()));
    bus.borrow_mut().mem[0][56..58].copy_from_slice(&joints[0].min_steps.to_le_bytes());
    bus.borrow_mut().mem[1][56..58].copy_from_slice(&joints[1].max_steps.to_le_bytes());
    let mut drive = Drive {
        bus: bus.clone(),
        joints,
    };
    let start = drive.arm().unwrap();
    assert_eq!(start[0], joints[0].min_steps);
    assert_eq!(start[1], joints[1].max_steps);
    assert_eq!(bus.borrow().goals[0], start);

    // Planner eases smoothly inward from a hard-limit start without jumping to the inset,
    // and even extreme outward manual commands settle strictly at the inset bounds.
    let origin = Instant::now();
    let mut plan = Planner::new(joints, start, origin);
    let push_out = Inputs {
        manual: Some(([-1000., -1000., 1000.], origin + Duration::from_secs(5))),
        ..Inputs::default()
    };
    let first = plan.step(origin, &push_out);
    assert!(first[0].abs_diff(start[0]) <= 1);
    assert!(first[1].abs_diff(start[1]) <= 1);
    let mut last = first;
    for tick in 1..200 {
        last = plan.step(origin + PERIOD * tick, &push_out);
        for (i, j) in joints.iter().enumerate() {
            assert!((j.min_steps..=j.max_steps).contains(&last[i]));
        }
    }
    let inset = calibration::PLAN_LIMIT_INSET_STEPS;
    assert_eq!(
        last,
        [
            joints[0].min_steps + inset,
            joints[1].max_steps - inset,
            joints[2].max_steps - inset
        ]
    );

    // Crossed envelope triggers strict disarm.
    bus.borrow_mut().mem[0][56..58].copy_from_slice(&(joints[0].min_steps - 1).to_le_bytes());
    assert!(drive.health_step(0).is_err());
    drop(drive);
    assert!(bus.borrow().mem.iter().all(|m| m[usize::from(TORQUE)] == 0));
}

#[test]
fn confirm_retries_read_only_without_extra_write() {
    struct FlakyReadBus {
        inner: Rc<RefCell<Mock>>,
        fail_reads_left: usize,
        reads: usize,
    }
    impl Bus for FlakyReadBus {
        fn ping(&mut self, id: u8) -> Result<()> {
            self.inner.ping(id)
        }
        fn read(&mut self, id: u8, reg: u8, out: &mut [u8]) -> Result<()> {
            self.reads += 1;
            if reg == TORQUE && id == 1 && self.fail_reads_left > 0 {
                self.fail_reads_left -= 1;
                anyhow::bail!("transient busy read");
            }
            self.inner.read(id, reg, out)
        }
        fn write(&mut self, id: u8, reg: u8, bytes: &[u8]) -> Result<()> {
            self.inner.write(id, reg, bytes)
        }
        fn goals(&mut self, joints: &Joints, steps: [u16; 3]) -> Result<()> {
            self.inner.goals(joints, steps)
        }
    }

    // Two transient readback failures succeed on attempt 3 with only one TORQUE=1 write.
    let mock = Rc::new(RefCell::new(Mock::new()));
    let mut drive = Drive {
        bus: FlakyReadBus {
            inner: mock.clone(),
            fail_reads_left: 2,
            reads: 0,
        },
        joints: fixture(),
    };
    drive.arm().unwrap();
    let torque_on_writes = mock
        .borrow()
        .writes
        .iter()
        .filter(|(id, r, v)| *id == 1 && *r == TORQUE && v == &[1])
        .count();
    assert_eq!(torque_on_writes, 1);
    drop(drive);

    // Exhausting bounded confirmation retries fails with still only one write.
    let mock = Rc::new(RefCell::new(Mock::new()));
    let mut drive = Drive {
        bus: FlakyReadBus {
            inner: mock.clone(),
            fail_reads_left: 10,
            reads: 0,
        },
        joints: fixture(),
    };
    assert!(drive.arm().is_err());
    let torque_on_writes = mock
        .borrow()
        .writes
        .iter()
        .filter(|(id, r, v)| *id == 1 && *r == TORQUE && v == &[1])
        .count();
    assert_eq!(torque_on_writes, 0); // failed during initial disable confirm before arm
    let torque_off_writes = mock
        .borrow()
        .writes
        .iter()
        .filter(|(id, r, v)| *id == 1 && *r == TORQUE && v == &[0])
        .count();
    assert_eq!(torque_off_writes, 1);
}

#[test]
fn staggered_health_reads_once_per_tick_and_latches_every_safety_fault() {
    struct CountingBus {
        inner: Rc<RefCell<Mock>>,
        reads: usize,
    }
    impl Bus for CountingBus {
        fn ping(&mut self, id: u8) -> Result<()> {
            self.inner.ping(id)
        }
        fn read(&mut self, id: u8, reg: u8, out: &mut [u8]) -> Result<()> {
            self.reads += 1;
            self.inner.read(id, reg, out)
        }
        fn write(&mut self, id: u8, reg: u8, bytes: &[u8]) -> Result<()> {
            self.inner.write(id, reg, bytes)
        }
        fn goals(&mut self, joints: &Joints, steps: [u16; 3]) -> Result<()> {
            self.inner.goals(joints, steps)
        }
    }

    // Each health_step performs exactly 1 bus read, covering all 15 (axis, check) slots.
    let mock = Rc::new(RefCell::new(Mock::new()));
    let mut drive = Drive {
        bus: CountingBus {
            inner: mock,
            reads: 0,
        },
        joints: fixture(),
    };
    drive.arm().unwrap();
    for step in 0..drive::HEALTH_STEPS {
        drive.bus.reads = 0;
        drive.health_step(step).unwrap();
        assert_eq!(drive.bus.reads, 1);
    }

    // Every safety invariant across all 3 axes faults within one full staggered cycle.
    for axis in 0..3usize {
        for (reg, bad_byte) in [
            (PRESENT + 1, 0u8),
            (TORQUE, 0),
            (MODE, 3),
            (LOCK, 0),
            (LIMITS, 0),
        ] {
            let m = Rc::new(RefCell::new(Mock::new()));
            let mut d = Drive {
                bus: m.clone(),
                joints: fixture(),
            };
            d.arm().unwrap();
            m.borrow_mut().mem[axis][usize::from(reg)] = bad_byte;
            let caught = (0..drive::HEALTH_STEPS).any(|s| d.health_step(s).is_err());
            assert!(caught, "missed fault axis={axis} reg={reg}");
            drop(d);
            assert!(
                m.borrow()
                    .mem
                    .iter()
                    .all(|ram| ram[usize::from(TORQUE)] == 0)
            );
        }
    }
}

struct SyncMock {
    mem: Arc<Mutex<[[u8; 64]; 3]>>,
    fail_goals: Arc<AtomicBool>,
    panic_goals: Arc<AtomicBool>,
}
impl SyncMock {
    fn new() -> (Self, Arc<Mutex<[[u8; 64]; 3]>>) {
        let mut mem = [[0u8; 64]; 3];
        for (ram, j) in mem.iter_mut().zip(fixture()) {
            ram[9..11].copy_from_slice(&j.min_steps.to_le_bytes());
            ram[11..13].copy_from_slice(&j.max_steps.to_le_bytes());
            ram[55] = 1;
            ram[56..58].copy_from_slice(&j.center.to_le_bytes());
        }
        let shared = Arc::new(Mutex::new(mem));
        (
            Self {
                mem: shared.clone(),
                fail_goals: Arc::new(AtomicBool::new(false)),
                panic_goals: Arc::new(AtomicBool::new(false)),
            },
            shared,
        )
    }
}
impl Bus for SyncMock {
    fn ping(&mut self, _: u8) -> Result<()> {
        Ok(())
    }
    fn read(&mut self, id: u8, reg: u8, out: &mut [u8]) -> Result<()> {
        let idx = usize::from(reg);
        let mem = self.mem.lock().unwrap();
        out.copy_from_slice(&mem[usize::from(id - 1)][idx..idx + out.len()]);
        Ok(())
    }
    fn write(&mut self, id: u8, reg: u8, bytes: &[u8]) -> Result<()> {
        let idx = usize::from(reg);
        let mut mem = self.mem.lock().unwrap();
        mem[usize::from(id - 1)][idx..idx + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }
    fn goals(&mut self, joints: &Joints, steps: [u16; 3]) -> Result<()> {
        assert!(
            !self.panic_goals.load(Ordering::Acquire),
            "injected worker panic"
        );
        if self.fail_goals.load(Ordering::Acquire) {
            anyhow::bail!("injected bus fault");
        }
        for (j, p) in joints.iter().zip(steps) {
            self.write(j.id, GOAL, &p.to_le_bytes())?;
        }
        Ok(())
    }
}

#[test]
fn worker_alive_flips_false_on_stop_fault_and_panic_with_torque_off() {
    let wait_dead = |input: &MotionInput| {
        let deadline = Instant::now() + Duration::from_secs(2);
        while input.alive() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!input.alive());
    };

    assert!(!MotionInput::default().alive());

    // Clean stop flips alive false and disables torque.
    let (bus, mem) = SyncMock::new();
    let mut drive = Drive {
        bus,
        joints: fixture(),
    };
    let start = drive.arm().unwrap();
    let motion = spawn_worker(drive, start).unwrap();
    let input = motion.input();
    assert!(input.alive());
    drop(motion);
    assert!(!input.alive());
    assert!(
        mem.lock()
            .unwrap()
            .iter()
            .all(|m| m[usize::from(TORQUE)] == 0)
    );

    // Bus fault flips alive false while Motion is still held and disables torque.
    let (bus, mem) = SyncMock::new();
    let fail = bus.fail_goals.clone();
    let mut drive = Drive {
        bus,
        joints: fixture(),
    };
    let start = drive.arm().unwrap();
    let motion = spawn_worker(drive, start).unwrap();
    let input = motion.input();
    assert!(input.alive());
    fail.store(true, Ordering::Release);
    wait_dead(&input);
    assert!(
        mem.lock()
            .unwrap()
            .iter()
            .all(|m| m[usize::from(TORQUE)] == 0)
    );
    drop(motion);

    // Worker panic flips alive false while Motion is still held and disables torque.
    let (bus, mem) = SyncMock::new();
    let boom = bus.panic_goals.clone();
    let mut drive = Drive {
        bus,
        joints: fixture(),
    };
    let start = drive.arm().unwrap();
    let motion = spawn_worker(drive, start).unwrap();
    let input = motion.input();
    assert!(input.alive());
    boom.store(true, Ordering::Release);
    wait_dead(&input);
    assert!(
        mem.lock()
            .unwrap()
            .iter()
            .all(|m| m[usize::from(TORQUE)] == 0)
    );
    drop(motion);
}

#[test]
fn cli_reports_unconfirmed_torque_off_to_stderr_on_finish_error_and_panic() {
    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl SharedBuf {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }
    struct PanickingBus {
        inner: Rc<RefCell<Mock>>,
        panic_on_goals: bool,
    }
    impl Bus for PanickingBus {
        fn ping(&mut self, id: u8) -> Result<()> {
            self.inner.ping(id)
        }
        fn read(&mut self, id: u8, reg: u8, out: &mut [u8]) -> Result<()> {
            self.inner.read(id, reg, out)
        }
        fn write(&mut self, id: u8, reg: u8, bytes: &[u8]) -> Result<()> {
            self.inner.write(id, reg, bytes)
        }
        fn goals(&mut self, joints: &Joints, steps: [u16; 3]) -> Result<()> {
            if self.panic_on_goals && steps == joints.map(|j| j.center) {
                // Make torque-off unconfirmable before panicking.
                self.inner.borrow_mut().forget = Some((2, TORQUE));
                panic!("simulated panic while centering");
            }
            self.inner.goals(joints, steps)
        }
    }

    // 1. Error path (verify fails, and disable cannot confirm torque off).
    let mock = Rc::new(RefCell::new(Mock::new()));
    mock.borrow_mut().forget = Some((1, TORQUE));
    mock.borrow_mut().mem[0][usize::from(TORQUE)] = 1;
    let buf = SharedBuf::default();
    let cli = CliDrive::new(
        Drive {
            bus: mock,
            joints: fixture(),
        },
        buf.clone(),
    );
    assert!(maintenance(cli, "verify", &AtomicBool::new(false)).is_err());
    assert!(
        buf.text().contains("could not confirm neck torque off"),
        "expected stderr warning on error, got: {:?}",
        buf.text()
    );

    // 2. Panic path (unwind drops CliDrive while torque-off cannot be confirmed).
    let mock = Rc::new(RefCell::new(Mock::new()));
    let buf = SharedBuf::default();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let cli = CliDrive::new(
            Drive {
                bus: PanickingBus {
                    inner: mock.clone(),
                    panic_on_goals: true,
                },
                joints: fixture(),
            },
            buf.clone(),
        );
        let _ = maintenance(cli, "center", &AtomicBool::new(false));
    }));
    assert!(result.is_err());
    assert!(
        buf.text().contains("could not confirm neck torque off"),
        "expected stderr warning on panic, got: {:?}",
        buf.text()
    );
}

#[test]
fn center_accepts_reference_tolerance_eight_and_preserves_stall_cutoff() {
    // Settling at distance 8 (reference <= 8 steps) succeeds and disables torque.
    let mock = Rc::new(RefCell::new(Mock::new()));
    for (ram, j) in mock.borrow_mut().mem.iter_mut().zip(fixture()) {
        ram[56..58].copy_from_slice(&(j.center + 8).to_le_bytes());
    }
    let cli = CliDrive::new(
        Drive {
            bus: mock.clone(),
            joints: fixture(),
        },
        Vec::new(),
    );
    maintenance(cli, "center", &AtomicBool::new(false)).unwrap();
    assert!(
        mock.borrow()
            .mem
            .iter()
            .all(|m| m[usize::from(TORQUE)] == 0)
    );

    // Distance 9 (> 8) with no progress still hits the 1s stall cutoff and disables torque.
    let mock = Rc::new(RefCell::new(Mock::new()));
    for (ram, j) in mock.borrow_mut().mem.iter_mut().zip(fixture()) {
        ram[56..58].copy_from_slice(&(j.center + 9).to_le_bytes());
    }
    let cli = CliDrive::new(
        Drive {
            bus: mock.clone(),
            joints: fixture(),
        },
        Vec::new(),
    );
    let err = maintenance(cli, "center", &AtomicBool::new(false)).unwrap_err();
    assert!(
        err.to_string().contains("stalled while centering"),
        "unexpected error: {err:#}"
    );
    assert!(
        mock.borrow()
            .mem
            .iter()
            .all(|m| m[usize::from(TORQUE)] == 0)
    );
}

#[test]
fn tracking_does_not_absorb_yaw_or_pitch_gestures() {
    // Wide yaw range so Look (+35 deg) is not saturated at the limit.
    let mut joints = fixture();
    joints[0].min_steps = 2048 - 600;
    joints[0].max_steps = 2048 + 600;
    joints[0].max_speed_deg_s = 120.;

    let origin = Instant::now();
    let mut plan = Planner::new(joints, [2048; 3], origin);
    let mut inputs = Inputs {
        gesture: Some((Gesture::Look, origin, 1.)),
        ..Inputs::default()
    };
    // Let Look hold at +35 deg yaw (around t = 1.0s, tick 50).
    for tick in 0..50u32 {
        plan.step(origin + PERIOD * tick, &inputs);
    }
    // Deliver a small right-side face error while Look is holding +35 deg yaw:
    // yaw_err = -60 * (-0.15) = +9 deg -> 0.35 * 9 = +3.15 deg.
    // Without gesture subtraction, track[0] would absorb ~+35 deg.
    let face_t = origin + PERIOD * 50;
    inputs.face = Some((face_t, -0.15, 0.));
    plan.step(face_t, &inputs);

    // Let Look finish (3.2s = 160 ticks) and ease to the post-gesture tracking angle (~+3 deg).
    let mut steps = [2048; 3];
    for tick in 51..260u32 {
        steps = plan.step(origin + PERIOD * tick, &inputs);
    }
    let settled_yaw = joints[0].degrees(steps[0]);
    assert!(
        settled_yaw.abs() < 10.,
        "yaw tracking absorbed Look gesture: settled at {settled_yaw} deg"
    );

    // Pitch: play Nod (-12 deg at t = 0.25s, tick 12) and deliver a downward face error.
    let mut plan = Planner::new(joints, [2048; 3], origin);
    let mut inputs = Inputs {
        gesture: Some((Gesture::Nod, origin, 1.)),
        ..Inputs::default()
    };
    for tick in 0..13u32 {
        plan.step(origin + PERIOD * tick, &inputs);
    }
    let face_t = origin + PERIOD * 13;
    // pitch_err = -45 * (-0.2) = +9 deg -> 0.35 * 9 = +3.15 deg (upward!).
    // If Nod's -12 deg were absorbed from pos[1], pitch tracking would end up negative.
    inputs.face = Some((face_t, 0., -0.2));
    plan.step(face_t, &inputs);
    for tick in 14..140u32 {
        steps = plan.step(origin + PERIOD * tick, &inputs);
    }
    let settled_pitch = joints[1].degrees(steps[1]);
    assert!(
        settled_pitch > 1.0,
        "pitch tracking absorbed Nod gesture: settled at {settled_pitch} deg"
    );
}

#[test]
fn tracking_pitch_clamp_is_relative_to_active_expression() {
    let joints = fixture();
    let (pitch_lo, pitch_hi) = joints[1].plan_bounds_degrees();
    let origin = Instant::now();

    // Under "surprised" (+12 deg pitch), repeated upward faces must not wind up track[1]
    // past (pitch_hi - 12); a subsequent downward face must immediately lower pitch.
    let mut plan = Planner::new(joints, [2048; 3], origin);
    let (surprised_pose, _) = poses::expression("surprised", 1.);
    let mut inputs = Inputs {
        pose: surprised_pose,
        ..Inputs::default()
    };
    for k in 0..5u32 {
        let t = origin + PERIOD * k;
        inputs.face = Some((t, 0., -1.));
        plan.step(t, &inputs);
    }
    // Single downward face: pitch_err = -45 * 0.35 = -15.75 deg -> delta = -5.51 deg.
    let down_t = origin + PERIOD * 5;
    inputs.face = Some((down_t, 0., 0.35));
    let mut steps = [2048; 3];
    for tick in 5..120u32 {
        steps = plan.step(origin + PERIOD * tick, &inputs);
    }
    let settled = joints[1].degrees(steps[1]);
    assert!(
        settled < pitch_hi - 2.5,
        "upward pitch tracking wound up past surprised base: settled={settled}, hi={pitch_hi}"
    );

    // Under "sad" (-12 deg pitch), repeated upward faces must still reach near pitch_hi
    // (track[1] is allowed up to pitch_hi - (-12)).
    let mut plan = Planner::new(joints, [2048; 3], origin);
    let (sad_pose, _) = poses::expression("sad", 1.);
    let mut inputs = Inputs {
        pose: sad_pose,
        ..Inputs::default()
    };
    for k in 0..4u32 {
        let t = origin + PERIOD * k;
        inputs.face = Some((t, 0., -1.));
        plan.step(t, &inputs);
    }
    for tick in 4..140u32 {
        steps = plan.step(origin + PERIOD * tick, &inputs);
    }
    let settled = joints[1].degrees(steps[1]);
    assert!(
        settled > pitch_hi - 2.5 && settled >= pitch_lo,
        "sad expression blocked upward tracking: settled={settled}, hi={pitch_hi}"
    );
}

#[test]
fn cue_expires_after_four_seconds_reenabling_tracking_while_express_persists_and_gesture_completes()
{
    let joints = fixture();
    let origin = Instant::now();

    // 1. Model/operator cue("asleep") suppresses tracking for < 4s, then resets and re-enables tracking.
    let input = MotionInput::default();
    input.cue("asleep", 1.);
    input.track(1., 0.);
    assert!(
        input.0.lock().unwrap().face.is_none(),
        "tracking must stay suppressed during active asleep cue"
    );
    let mut state = input.0.lock().unwrap().clone();
    // Anchor the cue deadline deterministically to `origin + 4s` for Planner ticks.
    let (asleep_pose, asleep_flag) = poses::expression("asleep", 1.);
    state.cue = Some((asleep_pose, asleep_flag, origin + poses::CUE_TTL));
    let mut plan = Planner::new(joints, [2048; 3], origin);
    let mut steps = [2048; 3];
    // At t = 2s (inside 4s TTL): pitch has moved strongly negative toward asleep pose and ignores face.
    state.face = Some((origin + Duration::from_secs(2), 1., 0.));
    for tick in 0..100u32 {
        steps = plan.step(origin + PERIOD * tick, &state);
    }
    assert!(joints[1].degrees(steps[1]) < -8.);
    assert!(joints[0].degrees(steps[0]).abs() < 1.);

    // After 4s TTL expires: pitch returns near neutral and a fresh face observation steers yaw.
    let wake_t = origin + poses::CUE_TTL + Duration::from_millis(100);
    state.face = Some((wake_t, 1., 0.));
    for tick in 205..320u32 {
        steps = plan.step(origin + PERIOD * tick, &state);
    }
    assert!(
        joints[1].degrees(steps[1]).abs() < 3.,
        "asleep cue did not reset pitch after 4s: {}",
        joints[1].degrees(steps[1])
    );
    assert!(
        joints[0].degrees(steps[0]) < -5.,
        "tracking did not re-enable after asleep cue expired: {}",
        joints[0].degrees(steps[0])
    );

    // Live MotionInput::track also re-enables once the cue deadline passes.
    input.0.lock().unwrap().cue = Some((
        asleep_pose,
        true,
        Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap(),
    ));
    input.track(0.5, 0.);
    assert!(input.0.lock().unwrap().face.is_some());

    // 2. Permanent express("asleep") never expires past 4s and keeps tracking suppressed.
    let perm = MotionInput::default();
    perm.express("asleep", 1.);
    perm.track(1., 0.);
    assert!(perm.0.lock().unwrap().face.is_none());
    let perm_state = perm.0.lock().unwrap().clone();
    assert_eq!(
        perm_state.effective_expression(origin + Duration::from_secs(60)),
        (asleep_pose, true)
    );

    // 3. Expression cue expires at 4s without truncating a longer gesture (Dance = 14s).
    let (happy_pose, _) = poses::expression("happy", 1.);
    let cue_with_dance = Inputs {
        cue: Some((happy_pose, false, origin + poses::CUE_TTL)),
        gesture: Some((Gesture::Dance, origin, 1.)),
        ..Inputs::default()
    };
    let mut plan = Planner::new(joints, [2048; 3], origin);
    // Step into t = 5.5s (past 4s cue TTL, inside Dance groove where roll swings up to 20 deg).
    let mut max_roll_after_cue = 0.0f32;
    for tick in 0..350u32 {
        let t = origin + PERIOD * tick;
        let out = plan.step(t, &cue_with_dance);
        if t > origin + poses::CUE_TTL + Duration::from_secs(1) {
            max_roll_after_cue = max_roll_after_cue.max(joints[2].degrees(out[2]).abs());
        }
    }
    assert!(
        max_roll_after_cue > 8.,
        "gesture was truncated when 4s expression cue expired: max_roll={max_roll_after_cue}"
    );
}
