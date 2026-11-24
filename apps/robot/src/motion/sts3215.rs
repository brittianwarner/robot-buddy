//! Feetech little-endian STS bus. Single owner, absolute I/O deadlines, no retries of writes.
use super::calibration::{DEG_PER_STEP, Joints};
use anyhow::{Context, Result, bail, ensure};
use nix::sys::termios::{self, ControlFlags, FlushArg, SetArg};
use std::{
    fs::File,
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    thread,
    time::{Duration, Instant},
};

pub(super) const LIMITS: u8 = 9;
pub(super) const MODE: u8 = 33;
pub(super) const TORQUE: u8 = 40;
pub(super) const GOAL: u8 = 42;
pub(super) const LOCK: u8 = 55;
pub(super) const PRESENT: u8 = 56;
const INST_PING: u8 = 1;
const INST_READ: u8 = 2;
const INST_WRITE: u8 = 3;
const INST_SYNC_WRITE: u8 = 0x83;
const IO_TIMEOUT: Duration = Duration::from_millis(20);

enum ReplyError {
    Timeout,
    Other(anyhow::Error),
}

pub(super) trait Bus {
    fn ping(&mut self, id: u8) -> Result<()>;
    fn read(&mut self, id: u8, reg: u8, out: &mut [u8]) -> Result<()>;
    fn write(&mut self, id: u8, reg: u8, bytes: &[u8]) -> Result<()>;
    fn goals(&mut self, joints: &Joints, steps: [u16; 3]) -> Result<()>;
}

pub(super) struct Serial(File, Duration);

impl Serial {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .custom_flags(nix::libc::O_NOCTTY | nix::libc::O_NONBLOCK)
            .open(path)?;
        file.try_lock().context("servo port already in use")?;
        let mut cfg = termios::tcgetattr(&file)?;
        termios::cfmakeraw(&mut cfg);
        cfg.control_flags
            .insert(ControlFlags::CLOCAL | ControlFlags::CREAD);
        cfg.control_flags
            .remove(ControlFlags::HUPCL | ControlFlags::CRTSCTS);
        configure_megabaud(&mut cfg)?;
        termios::tcsetattr(&file, SetArg::TCSANOW, &cfg)?;
        let applied = termios::tcgetattr(&file)?;
        verify_megabaud(&applied)?;
        Ok(Self(file, IO_TIMEOUT))
    }
    fn send(&mut self, bytes: &[u8], until: Instant) -> Result<()> {
        let mut rest = bytes;
        while !rest.is_empty() {
            ensure!(Instant::now() < until, "servo write deadline elapsed");
            match self.0.write(rest) {
                Ok(0) => bail!("servo port closed during write"),
                Ok(n) => rest = &rest[n..],
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_micros(200));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
    fn read_reply(
        &mut self,
        req: &[u8],
        out: &mut [u8],
        deadline: Instant,
    ) -> std::result::Result<(), ReplyError> {
        let mut stream = Vec::with_capacity(64);
        loop {
            // Sample the deadline before reading, but decide only after draining: a
            // preempted caller whose reply is already buffered must not report a timeout.
            let expired = Instant::now() >= deadline;
            let mut chunk = [0; 64];
            match self.0.read(&mut chunk) {
                Ok(n) if n > 0 => {
                    stream.extend_from_slice(&chunk[..n]);
                    match status(&stream, req, out.len()) {
                        Ok(Some(data)) => {
                            out.copy_from_slice(data);
                            return Ok(());
                        }
                        Ok(None) => {
                            if stream.len() >= 128 {
                                return Err(ReplyError::Other(anyhow::anyhow!(
                                    "oversize servo response"
                                )));
                            }
                        }
                        Err(e) => return Err(ReplyError::Other(e)),
                    }
                }
                Ok(_) if expired => return Err(ReplyError::Timeout),
                Ok(_) => thread::sleep(Duration::from_micros(200)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if expired {
                        return Err(ReplyError::Timeout);
                    }
                    thread::sleep(Duration::from_micros(200));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    if expired {
                        return Err(ReplyError::Timeout);
                    }
                }
                Err(e) => return Err(ReplyError::Other(e.into())),
            }
        }
    }
    fn request(&mut self, id: u8, opcode: u8, args: &[u8], out: &mut [u8]) -> Result<()> {
        let req = packet(id, opcode, args)?;
        let deadline = Instant::now() + self.1;
        termios::tcflush(&self.0, FlushArg::TCIFLUSH)?;
        self.send(&req, deadline)?;
        match self.read_reply(&req, out, deadline) {
            Ok(()) => Ok(()),
            Err(ReplyError::Timeout) => bail!("servo {id} status timeout"),
            Err(ReplyError::Other(e)) => Err(e),
        }
    }
}

#[cfg(target_os = "linux")]
fn configure_megabaud(cfg: &mut termios::Termios) -> Result<()> {
    termios::cfsetspeed(cfg, termios::BaudRate::B1000000)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_megabaud(cfg: &termios::Termios) -> Result<()> {
    let ispeed = termios::cfgetispeed(cfg);
    let ospeed = termios::cfgetospeed(cfg);
    ensure!(
        ispeed == termios::BaudRate::B1000000 && ospeed == termios::BaudRate::B1000000,
        "servo port did not apply 1 Mbaud (in={ispeed:?}, out={ospeed:?})"
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn configure_megabaud(_: &mut termios::Termios) -> Result<()> {
    bail!("neck hardware requires Linux 1 Mbaud termios")
}

#[cfg(not(target_os = "linux"))]
fn verify_megabaud(cfg: &termios::Termios) -> Result<()> {
    let ispeed = termios::cfgetispeed(cfg);
    let ospeed = termios::cfgetospeed(cfg);
    ensure!(
        ispeed == 1_000_000 && ospeed == 1_000_000,
        "servo port did not apply 1 Mbaud (in={ispeed}, out={ospeed})"
    );
    Ok(())
}

fn checksum(bytes: &[u8]) -> u8 {
    !bytes.iter().copied().fold(0u8, u8::wrapping_add)
}

fn packet(id: u8, instruction: u8, params: &[u8]) -> Result<Vec<u8>> {
    let mut bytes = vec![0xff, 0xff, id, u8::try_from(params.len() + 2)?, instruction];
    bytes.extend_from_slice(params);
    bytes.push(checksum(&bytes[2..]));
    Ok(bytes)
}

fn status<'a>(mut bytes: &'a [u8], req: &[u8], count: usize) -> Result<Option<&'a [u8]>> {
    loop {
        let Some(start) = bytes.windows(2).position(|w| w == [255, 255]) else {
            return Ok(None);
        };
        bytes = &bytes[start..];
        // Idle/turnaround bus noise can prepend extra 0xff bytes before the 0xff 0xff header;
        // 0xff is never a valid servo ID.
        while bytes.get(2) == Some(&255) {
            bytes = &bytes[1..];
        }
        if bytes.len() < 4 {
            return Ok(None);
        }
        let n = usize::from(bytes[3]);
        ensure!(n >= 2, "bad STS status length");
        let Some(frame) = bytes.get(..4 + n) else {
            return Ok(None);
        };
        ensure!(
            checksum(&frame[2..frame.len() - 1]) == frame[frame.len() - 1],
            "STS checksum mismatch"
        );
        // A half-duplex bridge can echo this request, a preceding broadcast, or deliver
        // a delayed zero-payload OK ACK from a preceding addressed write before a read reply.
        if frame == req
            || (frame[2] == 254 && frame[4] == INST_SYNC_WRITE)
            || (count > 0 && frame[2] == req[2] && frame[4] == 0 && n == 2)
        {
            bytes = &bytes[frame.len()..];
            continue;
        }
        ensure!(
            frame[2] == req[2] && frame[4] == 0 && n == count + 2,
            "unexpected STS status id={} error={} length={n}",
            frame[2],
            frame[4]
        );
        return Ok(Some(&frame[5..frame.len() - 1]));
    }
}

fn write_packet(id: u8, reg: u8, bytes: &[u8]) -> Result<Vec<u8>> {
    let mut params = Vec::with_capacity(1 + bytes.len());
    params.push(reg);
    params.extend_from_slice(bytes);
    packet(id, INST_WRITE, &params)
}

fn goals_packet(joints: &Joints, steps: [u16; 3]) -> Result<Vec<u8>> {
    let mut params = vec![41, 7];
    for (j, position) in joints.iter().zip(steps) {
        ensure!(
            (j.min_steps..=j.max_steps).contains(&position),
            "goal outside neck limits"
        );
        params.extend([j.id, j.accel]);
        params.extend(position.to_le_bytes());
        params.extend([0, 0]);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "calibration bounds positive speed to 120 degrees/s"
        )]
        let speed = (j.max_speed_deg_s / DEG_PER_STEP).floor() as u16;
        params.extend(speed.to_le_bytes());
    }
    packet(254, INST_SYNC_WRITE, &params)
}

impl Bus for Serial {
    fn ping(&mut self, id: u8) -> Result<()> {
        self.request(id, INST_PING, &[], &mut [])
    }
    fn read(&mut self, id: u8, reg: u8, out: &mut [u8]) -> Result<()> {
        self.request(id, INST_READ, &[reg, u8::try_from(out.len())?], out)
    }
    fn write(&mut self, id: u8, reg: u8, bytes: &[u8]) -> Result<()> {
        // Use addressed INST_WRITE for EEPROM/LOCK/TORQUE. Drain any half-duplex echo and
        // optional status ACK without retrying the write; silent-ACK servos (response level 0)
        // or delayed EEPROM commits are confirmed read-only by callers.
        let req = write_packet(id, reg, bytes)?;
        let deadline = Instant::now() + self.1;
        termios::tcflush(&self.0, FlushArg::TCIFLUSH)?;
        self.send(&req, deadline)?;
        match self.read_reply(&req, &mut [], deadline) {
            Ok(()) | Err(ReplyError::Timeout) => Ok(()),
            Err(ReplyError::Other(e)) => Err(e),
        }
    }
    fn goals(&mut self, joints: &Joints, steps: [u16; 3]) -> Result<()> {
        self.send(&goals_packet(joints, steps)?, Instant::now() + IO_TIMEOUT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn serial_transaction_reads_reply_after_a_real_tty_echo() {
        let pty = nix::pty::openpty(None, None).unwrap();
        let mut config = termios::tcgetattr(&pty.slave).unwrap();
        termios::cfmakeraw(&mut config);
        termios::tcsetattr(&pty.slave, SetArg::TCSANOW, &config).unwrap();
        nix::fcntl::fcntl(
            &pty.slave,
            nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
        )
        .unwrap();
        let mut driver = Serial(pty.slave.into(), Duration::from_secs(5));
        let mut adapter = File::from(pty.master);
        let (done, exit) = std::sync::mpsc::sync_channel(1);
        let simulator = thread::spawn(move || {
            let mut cmd = [0; 8];
            adapter.read_exact(&mut cmd).unwrap();
            adapter.write_all(&cmd).unwrap();
            adapter.write_all(&[255, 255, 1, 4, 0, 0, 8, 242]).unwrap();
            // Keep the master open until the kernel has delivered the final byte.
            let _ = exit.recv();
        });
        let mut result = [0; 2];
        driver.read(1, PRESENT, &mut result).unwrap();
        assert_eq!(result, [0, 8]);
        done.send(()).unwrap();
        simulator.join().unwrap();
    }
    #[test]
    fn reply_already_buffered_when_deadline_expires_is_not_a_timeout() {
        // A 50 Hz worker preempted past its 20 ms I/O deadline must still accept a
        // status reply the bus already delivered (soak: "servo 2 status timeout").
        // A regular file stands in for a fully buffered tty input queue.
        let dir = std::env::temp_dir().join(format!("sts-buffered-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let req = packet(2, INST_READ, &[TORQUE, 1]).unwrap();
        let buffered = dir.join("buffered");
        let mut wire = req.clone();
        wire.extend_from_slice(&packet(2, 0, &[1]).unwrap());
        std::fs::write(&buffered, &wire).unwrap();
        let empty = dir.join("empty");
        std::fs::write(&empty, []).unwrap();
        let expired = Instant::now();

        let mut driver = Serial(File::open(&buffered).unwrap(), IO_TIMEOUT);
        let mut out = [0; 1];
        assert!(driver.read_reply(&req, &mut out, expired).is_ok());
        assert_eq!(out, [1]);

        // Nothing buffered at an expired deadline is still a timeout, never a hang.
        let mut driver = Serial(File::open(&empty).unwrap(), IO_TIMEOUT);
        assert!(matches!(
            driver.read_reply(&req, &mut out, expired),
            Err(ReplyError::Timeout)
        ));
        // Draining after the deadline is bounded: unparseable buffered noise ends as oversize.
        std::fs::write(&buffered, [0x55u8; 4096]).unwrap();
        let mut driver = Serial(File::open(&buffered).unwrap(), IO_TIMEOUT);
        assert!(matches!(
            driver.read_reply(&req, &mut out, expired),
            Err(ReplyError::Other(_))
        ));
        // A corrupt buffered reply still fails closed after the deadline.
        let mut corrupt = packet(2, 0, &[1]).unwrap();
        *corrupt.last_mut().unwrap() ^= 1;
        std::fs::write(&buffered, &corrupt).unwrap();
        let mut driver = Serial(File::open(&buffered).unwrap(), IO_TIMEOUT);
        assert!(matches!(
            driver.read_reply(&req, &mut out, expired),
            Err(ReplyError::Other(_))
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn sync_write_packs_three_bounded_goals_in_one_frame() {
        let joints = super::super::tests::fixture();
        let frame = goals_packet(&joints, [1888, 2048, 2208]).unwrap();
        assert_eq!(
            frame,
            [
                255, 255, 254, 28, 131, 41, 7, 1, 20, 96, 7, 0, 0, 0, 2, 2, 20, 0, 8, 0, 0, 0, 2,
                3, 20, 160, 8, 0, 0, 0, 2, 211
            ]
        );
        assert!(goals_packet(&joints, [0, 2048, 2208]).is_err());
    }
    #[test]
    fn read_frame_and_fragmented_echo_status() {
        let req = packet(1, 2, &[56, 2]).unwrap();
        assert_eq!(req, [255, 255, 1, 4, 2, 56, 2, 190]);
        let mut wire = vec![0];
        wire.extend_from_slice(&req);
        wire.extend([255, 255, 1, 4, 0, 0, 8, 242]);
        for i in 0..wire.len() {
            assert!(status(&wire[..i], &req, 2).unwrap().is_none());
        }
        assert_eq!(status(&wire, &req, 2).unwrap(), Some(&[0, 8][..]));
        *wire.last_mut().unwrap() ^= 1;
        assert!(status(&wire, &req, 2).is_err());
        for invalid in [
            packet(2, 0, &[0, 8]),
            packet(1, 32, &[0, 8]),
            packet(1, 0, &[8]),
        ] {
            assert!(status(&invalid.unwrap(), &req, 2).is_err());
        }
    }
    #[test]
    fn status_tolerates_extra_leading_0xff_before_header_and_echo() {
        let req = packet(1, INST_READ, &[PRESENT, 2]).unwrap();
        let reply = packet(1, 0, &[0, 8]).unwrap();
        for extra in [1usize, 3] {
            let mut wire = vec![0xff; extra];
            wire.extend_from_slice(&req);
            wire.extend(std::iter::repeat_n(0xff, extra));
            wire.extend_from_slice(&reply);
            for i in 0..wire.len() {
                assert!(status(&wire[..i], &req, 2).unwrap().is_none());
            }
            assert_eq!(status(&wire, &req, 2).unwrap(), Some(&[0, 8][..]));
            *wire.last_mut().unwrap() ^= 1;
            assert!(status(&wire, &req, 2).is_err());
        }
    }
    #[test]
    fn write_uses_addressed_inst_write_and_handles_ack_silent_and_error() {
        let frame = write_packet(2, TORQUE, &[1]).unwrap();
        assert_eq!(frame, [0xff, 0xff, 2, 4, INST_WRITE, TORQUE, 1, 205]);
        assert_eq!(
            write_packet(1, LIMITS, &[96, 7, 160, 8]).unwrap(),
            [0xff, 0xff, 1, 7, INST_WRITE, LIMITS, 96, 7, 160, 8, 220]
        );
        // Delayed OK write ACK before a subsequent READ reply is skipped; error ACK fails.
        let read_req = packet(1, INST_READ, &[TORQUE, 1]).unwrap();
        let mut wire = packet(1, 0, &[]).unwrap();
        wire.extend_from_slice(&read_req);
        wire.extend_from_slice(&packet(1, 0, &[1]).unwrap());
        assert_eq!(status(&wire, &read_req, 1).unwrap(), Some(&[1][..]));
        let mut bad = packet(1, 32, &[]).unwrap();
        bad.extend_from_slice(&packet(1, 0, &[1]).unwrap());
        assert!(status(&bad, &read_req, 1).is_err());

        // PTY: echo + OK ACK succeeds cleanly.
        let open_pty = |timeout| {
            let pty = nix::pty::openpty(None, None).unwrap();
            let mut config = termios::tcgetattr(&pty.slave).unwrap();
            termios::cfmakeraw(&mut config);
            termios::tcsetattr(&pty.slave, SetArg::TCSANOW, &config).unwrap();
            nix::fcntl::fcntl(
                &pty.slave,
                nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
            )
            .unwrap();
            (Serial(pty.slave.into(), timeout), File::from(pty.master))
        };
        let (mut driver, mut adapter) = open_pty(Duration::from_secs(5));
        let (done, exit) = std::sync::mpsc::sync_channel(1);
        let sim = thread::spawn(move || {
            let mut cmd = [0; 8];
            adapter.read_exact(&mut cmd).unwrap();
            assert_eq!(cmd[2], 2);
            assert_eq!(cmd[4], INST_WRITE);
            adapter.write_all(&cmd).unwrap();
            adapter.write_all(&packet(2, 0, &[]).unwrap()).unwrap();
            let _ = exit.recv();
        });
        driver.write(2, TORQUE, &[1]).unwrap();
        done.send(()).unwrap();
        sim.join().unwrap();

        // PTY: echo + silent write (response level 0) succeeds once without extra write.
        let (mut driver, mut adapter) = open_pty(Duration::from_millis(20));
        let (done, exit) = std::sync::mpsc::sync_channel(1);
        let sim = thread::spawn(move || {
            let mut cmd = [0; 8];
            adapter.read_exact(&mut cmd).unwrap();
            adapter.write_all(&cmd).unwrap();
            let _ = exit.recv();
            // Confirm no second write arrived.
            let mut extra = [0; 1];
            nix::fcntl::fcntl(
                &adapter,
                nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
            )
            .unwrap();
            assert_eq!(
                adapter.read(&mut extra).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        });
        driver.write(2, TORQUE, &[1]).unwrap();
        done.send(()).unwrap();
        sim.join().unwrap();

        // PTY: error ACK is rejected immediately.
        let (mut driver, mut adapter) = open_pty(Duration::from_secs(5));
        let (done, exit) = std::sync::mpsc::sync_channel(1);
        let sim = thread::spawn(move || {
            let mut cmd = [0; 8];
            adapter.read_exact(&mut cmd).unwrap();
            adapter.write_all(&cmd).unwrap();
            adapter.write_all(&packet(2, 32, &[]).unwrap()).unwrap();
            let _ = exit.recv();
        });
        assert!(driver.write(2, TORQUE, &[1]).is_err());
        done.send(()).unwrap();
        sim.join().unwrap();
    }
    #[test]
    fn baud_verification_fails_closed_when_rate_is_not_one_megabaud() {
        let pty = nix::pty::openpty(None, None).unwrap();
        let mut cfg = termios::tcgetattr(&pty.slave).unwrap();
        termios::cfsetspeed(&mut cfg, termios::BaudRate::B115200).unwrap();
        assert!(verify_megabaud(&cfg).is_err());
        #[cfg(target_os = "linux")]
        {
            termios::cfsetspeed(&mut cfg, termios::BaudRate::B1000000).unwrap();
            assert!(verify_megabaud(&cfg).is_ok());
        }
    }
}
