//! The libei sender connection to KWin's EIS: the handshake, binding the seat, and emulating
//! pointer, button, scroll and key events on the devices KWin resumes.

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use reis::PendingRequestResult;
use reis::ei::{self, button::ButtonState, keyboard::KeyState};
use reis::enumflags2::BitFlags;
use reis::event::{DeviceCapability, EiEvent, EiEventConverter};
use rustix::event::{PollFd, PollFlags, Timespec};
use zbus::blocking::Connection;

use crate::SessionError;
use crate::keymap::{Chord, Keymap};

const READY_TIMEOUT: Duration = Duration::from_secs(2);
/// How long each frame is pumped after its flush, so KWin keeps pace with the events.
const FRAME_PUMP: Duration = Duration::from_millis(10);
/// The RemoteDesktop device types KWin may grant: keyboard, pointer and touch.
const ALL_DEVICE_TYPES: i32 = 7;

/// A resumed EIS connection with the devices the session drives.
pub(crate) struct Eis {
    ctx: ei::Context,
    conv: EiEventConverter,
    devices: Vec<(reis::event::Device, bool)>,
    sequence: u32,
}

impl Eis {
    /// Asks KWin for an EIS socket, completes the sender handshake, binds every capability and
    /// waits up to 2 s for resumed pointer, button and keyboard devices (stage `eis`).
    pub(crate) fn connect(bus: &Connection) -> Result<(Eis, Keymap), SessionError> {
        let fail = |message: String| SessionError::LaunchFailed {
            stage: "eis",
            message,
        };
        let (fd, _cookie): (zbus::zvariant::OwnedFd, i32) = bus
            .call_method(
                Some("org.kde.KWin"),
                "/org/kde/KWin/EIS/RemoteDesktop",
                Some("org.kde.KWin.EIS.RemoteDesktop"),
                "connectToEIS",
                &(ALL_DEVICE_TYPES,),
            )
            .and_then(|reply| reply.body().deserialize())
            .map_err(|e| fail(format!("connectToEIS: {e}")))?;
        let ctx = ei::Context::new(UnixStream::from(OwnedFd::from(fd)))
            .map_err(|e| fail(e.to_string()))?;
        let resp = reis::handshake::ei_handshake_blocking(
            &ctx,
            "gpui-pilot",
            ei::handshake::ContextType::Sender,
        )
        .map_err(|e| fail(format!("handshake: {e}")))?;
        let conv = EiEventConverter::new(&ctx, resp);
        let mut eis = Eis {
            ctx,
            conv,
            devices: Vec::new(),
            sequence: 0,
        };
        let deadline = Instant::now() + READY_TIMEOUT;
        while !eis.ready() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(fail(format!(
                    "no resumed pointer, button and keyboard devices within 2 s: {:?}",
                    eis.devices
                )));
            }
            eis.pump(left.min(Duration::from_millis(50)))?;
        }
        let keyboard = eis.device(DeviceCapability::Keyboard)?;
        let keymap = keyboard
            .keymap()
            .ok_or_else(|| fail("the keyboard device has no keymap".into()))?;
        let fd = keymap.fd.try_clone()?;
        let keymap = Keymap::from_fd(fd, keymap.size as usize)
            .map_err(|e| fail(format!("the EIS keymap: {e}")))?;
        Ok((eis, keymap))
    }

    /// Moves the absolute pointer to (`x`, `y`) in KWin logical pixels.
    pub(crate) fn motion(&mut self, x: f32, y: f32) -> Result<(), SessionError> {
        let device = self.device(DeviceCapability::PointerAbsolute)?;
        interface::<ei::PointerAbsolute>(&device)?.motion_absolute(x, y);
        self.frame(&device)
    }

    /// Presses and releases the evdev `button` code once.
    pub(crate) fn button(&mut self, button: u32) -> Result<(), SessionError> {
        self.press(button)?;
        self.release(button)
    }

    /// Presses the evdev `button` code and leaves it held, in its own frame.
    pub(crate) fn press(&mut self, button: u32) -> Result<(), SessionError> {
        self.button_state(button, ButtonState::Press)
    }

    /// Releases the evdev `button` code, in its own frame.
    pub(crate) fn release(&mut self, button: u32) -> Result<(), SessionError> {
        self.button_state(button, ButtonState::Released)
    }

    fn button_state(&mut self, button: u32, state: ButtonState) -> Result<(), SessionError> {
        let device = self.device(DeviceCapability::Button)?;
        interface::<ei::Button>(&device)?.button(button, state);
        self.frame(&device)
    }

    /// Scrolls by about (`dx`, `dy`) logical pixels as one wheel event (see `wheel_units`).
    /// KWin forwards smooth EIS scroll without an axis source, which GPUI reads as a wheel and
    /// drops for lacking `axis_value120`, so only the discrete form reaches the app.
    pub(crate) fn scroll(&mut self, dx: f32, dy: f32) -> Result<(), SessionError> {
        let device = self.device(DeviceCapability::Scroll)?;
        interface::<ei::Scroll>(&device)?.scroll_discrete(wheel_units(dx), wheel_units(dy));
        self.frame(&device)
    }

    /// One wheel notch: `scroll_discrete(x, y)` in its own frame, flushed, then only what KWin
    /// already sent is drained; unlike `frame` it never waits `FRAME_PUMP`, so paced notches
    /// keep their deadlines.
    pub(crate) fn notch(&mut self, x: i32, y: i32) -> Result<(), SessionError> {
        let device = self.device(DeviceCapability::Scroll)?;
        interface::<ei::Scroll>(&device)?.scroll_discrete(x, y);
        device
            .device()
            .frame(self.conv.connection().serial(), now_micros());
        self.flush()?;
        self.pump(Duration::ZERO)
    }

    /// Presses the chord's modifiers, taps its key, then releases the modifiers in reverse.
    pub(crate) fn chord(&mut self, chord: &Chord) -> Result<(), SessionError> {
        let device = self.device(DeviceCapability::Keyboard)?;
        let keyboard = interface::<ei::Keyboard>(&device)?;
        for &m in &chord.mods {
            keyboard.key(m, KeyState::Press);
        }
        keyboard.key(chord.key, KeyState::Press);
        self.frame(&device)?;
        keyboard.key(chord.key, KeyState::Released);
        for &m in chord.mods.iter().rev() {
            keyboard.key(m, KeyState::Released);
        }
        self.frame(&device)
    }

    /// Presses every evdev key in `codes` in one keyboard frame and leaves them held.
    pub(crate) fn keys_down(&mut self, codes: &[u32]) -> Result<(), SessionError> {
        self.keys_state(codes.iter().copied(), KeyState::Press)
    }

    /// Releases every evdev key in `codes`, last first, in one keyboard frame.
    pub(crate) fn keys_up(&mut self, codes: &[u32]) -> Result<(), SessionError> {
        self.keys_state(codes.iter().rev().copied(), KeyState::Released)
    }

    fn keys_state(
        &mut self,
        codes: impl Iterator<Item = u32>,
        state: KeyState,
    ) -> Result<(), SessionError> {
        let device = self.device(DeviceCapability::Keyboard)?;
        let keyboard = interface::<ei::Keyboard>(&device)?;
        for code in codes {
            keyboard.key(code, state);
        }
        self.frame(&device)
    }

    fn ready(&self) -> bool {
        [
            DeviceCapability::PointerAbsolute,
            DeviceCapability::Button,
            DeviceCapability::Keyboard,
        ]
        .into_iter()
        .all(|cap| self.device(cap).is_ok())
    }

    /// The first resumed device with `cap`, preferring one that also moves the absolute
    /// pointer so buttons and scrolls land where the pointer is.
    fn device(&self, cap: DeviceCapability) -> Result<reis::event::Device, SessionError> {
        let resumed = || {
            self.devices
                .iter()
                .filter(|(d, on)| *on && d.has_capability(cap))
        };
        resumed()
            .find(|(d, _)| d.has_capability(DeviceCapability::PointerAbsolute))
            .or_else(|| resumed().next())
            .map(|(d, _)| d.clone())
            .ok_or_else(|| {
                SessionError::Internal(format!("no resumed EIS device can send {cap:?}"))
            })
    }

    /// Ends the device's frame, sends it and gives KWin 10 ms to answer.
    fn frame(&mut self, device: &reis::event::Device) -> Result<(), SessionError> {
        device
            .device()
            .frame(self.conv.connection().serial(), now_micros());
        self.flush()?;
        self.pump(FRAME_PUMP)
    }

    fn flush(&self) -> Result<(), SessionError> {
        self.ctx
            .flush()
            .map_err(|e| SessionError::Internal(format!("EIS flush: {e}")))
    }

    /// Handles everything KWin sends for `wait`: binds each new seat, starts emulating on each
    /// resumed device and forgets paused or removed ones.
    fn pump(&mut self, wait: Duration) -> Result<(), SessionError> {
        let eis = |e: String| SessionError::Internal(format!("EIS: {e}"));
        let deadline = Instant::now() + wait;
        loop {
            // The handshake may have buffered events already, so drain before polling.
            while let Some(result) = self.ctx.pending_event() {
                if let PendingRequestResult::Request(event) = result {
                    self.conv
                        .handle_event(event)
                        .map_err(|e| eis(e.to_string()))?;
                }
            }
            while let Some(event) = self.conv.next_event() {
                self.on(event)?;
            }
            self.flush()?;
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            let timeout = Timespec {
                tv_sec: left.as_secs() as _,
                tv_nsec: left.subsec_nanos() as _,
            };
            let mut fds = [PollFd::new(&self.ctx, PollFlags::IN)];
            let ready = rustix::io::retry_on_intr(|| rustix::event::poll(&mut fds, Some(&timeout)))
                .map_err(|e| eis(e.to_string()))?;
            if ready > 0 {
                self.ctx.read().map_err(|e| eis(e.to_string()))?;
            }
        }
    }

    fn on(&mut self, event: EiEvent) -> Result<(), SessionError> {
        match event {
            EiEvent::SeatAdded(added) => added.seat.bind_capabilities(BitFlags::all()),
            EiEvent::DeviceAdded(added) => self.devices.push((added.device, false)),
            EiEvent::DeviceResumed(resumed) => {
                resumed
                    .device
                    .device()
                    .start_emulating(resumed.serial, self.sequence);
                self.sequence += 1;
                self.set_resumed(&resumed.device, true);
            }
            EiEvent::DevicePaused(paused) => self.set_resumed(&paused.device, false),
            EiEvent::DeviceRemoved(removed) => self.devices.retain(|(d, _)| *d != removed.device),
            EiEvent::Disconnected(gone) => {
                return Err(SessionError::Internal(format!(
                    "KWin closed the EIS connection: {:?} {}",
                    gone.reason,
                    gone.explanation.unwrap_or_default()
                )));
            }
            _ => {}
        }
        Ok(())
    }

    fn set_resumed(&mut self, device: &reis::event::Device, on: bool) {
        for (d, state) in &mut self.devices {
            if d == device {
                *state = on;
            }
        }
    }
}

fn interface<T: ei::Interface>(device: &reis::event::Device) -> Result<T, SessionError> {
    device.interface::<T>().ok_or_else(|| {
        SessionError::Internal(format!("the EIS device lacks the {} interface", T::NAME))
    })
}

/// Logical pixels as 1/120 wheel notches. GPUI scrolls a notch as 3 lines of the element's line
/// height, so this assumes 20 px lines (60 px per notch); a nonzero scroll keeps one unit.
fn wheel_units(px: f32) -> i32 {
    const UNITS_PER_PX: f32 = 120. / (3. * 20.);
    let units = (px * UNITS_PER_PX).round() as i32;
    match units {
        0 if px > 0. => 1,
        0 if px < 0. => -1,
        units => units,
    }
}

/// The monotonic clock in microseconds, the timestamp EIS frames carry.
fn now_micros() -> u64 {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    now.tv_sec as u64 * 1_000_000 + now.tv_nsec as u64 / 1_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_pixels_become_wheel_units_gpui_scrolls_as_the_same_pixels_on_20px_lines() {
        assert_eq!(
            wheel_units(60.),
            120,
            "GPUI scrolls a notch as 3 lines, 60 px on a 20 px line"
        );
        assert_eq!(wheel_units(300.), 600, "300 px is 5 notches, not 20");
        assert_eq!(wheel_units(-40.), -80);
        assert_eq!(wheel_units(0.75), 2, "units keep sub-notch precision");
        assert_eq!(wheel_units(0.), 0);
        assert_eq!(
            wheel_units(0.01),
            1,
            "a nonzero scroll is never rounded away"
        );
        assert_eq!(wheel_units(-0.01), -1);
    }
}
