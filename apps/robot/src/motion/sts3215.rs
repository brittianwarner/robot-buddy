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
const IO_TIMEOUT: Duration = Duration::from_millis(20);

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
        #[cfg(target_os = "linux")]
        termios::cfsetspeed(&mut cfg, termios::BaudRate::B1000000)?;
        #[cfg(not(target_os = "linux"))]
        configure_megabaud(&mut cfg)?;
        termios::tcsetattr(&file, SetArg::TCSANOW, &cfg)?;
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
    fn request(&mut self, id: u8, opcode: u8, args: &[u8], out: &mut [u8]) -> Result<()> {
        let req = packet(id, opcode, args)?;
        let deadline = Instant::now() + self.1;
        termios::tcflush(&self.0, FlushArg::TCIFLUSH)?;
        self.send(&req, deadline)?;
        let mut stream = Vec::with_capacity(64);
        loop {
            ensure!(Instant::now() < deadline, "servo {id} status timeout");
            let mut chunk = [0; 64];
            match self.0.read(&mut chunk) {
                Ok(n) if n > 0 => {
                    stream.extend_from_slice(&chunk[..n]);
                    if let Some(data) = status(&stream, &req, out.len())? {
                        out.copy_from_slice(data);
                        return Ok(());
                    }
                    ensure!(stream.len() < 128, "oversize servo response");
                }
                Ok(_) => thread::sleep(Duration::from_micros(200)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_micros(200));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                Err(e) => return Err(e.into()),
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn configure_megabaud(_: &mut termios::Termios) -> Result<()> {
    bail!("neck hardware requires Linux 1 Mbaud termios")
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
        // A half-duplex bridge can echo both this request and a preceding broadcast.
        if frame == req || (frame[2] == 254 && frame[4] == 0x83) {
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
    packet(254, 0x83, &params)
}

impl Bus for Serial {
    fn ping(&mut self, id: u8) -> Result<()> {
        self.request(id, 1, &[], &mut [])
    }
    fn read(&mut self, id: u8, reg: u8, out: &mut [u8]) -> Result<()> {
        self.request(id, 2, &[reg, u8::try_from(out.len())?], out)
    }
    fn write(&mut self, id: u8, reg: u8, bytes: &[u8]) -> Result<()> {
        // Address only this ID in a sync write: no write-status ACK can be confused
        // with the following register read. Control/EEPROM writes use readback.
        let mut params = vec![reg, u8::try_from(bytes.len())?, id];
        params.extend_from_slice(bytes);
        self.send(&packet(254, 0x83, &params)?, Instant::now() + IO_TIMEOUT)
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
}
