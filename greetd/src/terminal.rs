//! Virtual terminal handling.
//!
//! Ported from greetd, whose raw `_IO` numbers are used directly rather than
//! going through libc constants that aren't exposed. The struct layouts have to
//! match the kernel's exactly - see [`vt_setactivate`], where getting one field
//! wrong is a bug greetd carried for years.

use std::io::Write as _;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use nix::fcntl::{OFlag, open};
use nix::sys::stat::Mode;

#[allow(non_camel_case_types)]
mod ffi {
  pub const KDSETMODE: u16 = 0x4B3A;
  pub const KDTEXT: i32 = 0x00;

  pub const VT_OPENQRY: u16 = 0x5600;
  pub const VT_GETSTATE: u16 = 0x5603;
  pub const VT_WAITACTIVE: u16 = 0x5607;
  pub const VT_SETACTIVATE: u16 = 0x560F;

  pub const VT_AUTO: u8 = 0;
  pub const TIOCSCTTY: u16 = 0x540E;

  #[repr(C)]
  #[derive(Default)]
  pub struct vt_mode {
    pub mode: u8,
    pub waitv: u8,
    pub relsig: u16,
    pub acqsig: u16,
    pub frsig: u16,
  }

  /// `console` is a `u32`, not a `usize`.
  ///
  /// greetd had it as a 64-bit field, which made the struct 16 bytes where the
  /// kernel expects 12; it happened to work on little-endian because the extra
  /// four bytes lined up with an all-zero `vt_mode`, and failed everywhere else.
  /// That is greetd's HEAD commit, and it is worth not reintroducing.
  #[repr(C)]
  #[derive(Default)]
  pub struct vt_setactivate {
    pub console: u32,
    pub mode: vt_mode,
  }

  #[repr(C)]
  #[derive(Default)]
  pub struct vt_state {
    pub v_active: u16,
    pub v_signal: u16,
    pub v_state: u16,
  }

  nix::ioctl_write_int_bad!(kd_setmode, KDSETMODE);
  nix::ioctl_write_int_bad!(vt_waitactive, VT_WAITACTIVE);
  nix::ioctl_write_int_bad!(tiocsctty, TIOCSCTTY);
  nix::ioctl_read_bad!(vt_openqry, VT_OPENQRY, i32);
  nix::ioctl_read_bad!(vt_getstate, VT_GETSTATE, vt_state);
  nix::ioctl_write_ptr_bad!(vt_setactivate, VT_SETACTIVATE, vt_setactivate);
}

/// An open terminal device.
///
/// Holds an [`OwnedFd`] rather than greetd's raw descriptor plus an `autoclose`
/// flag, whose `Drop` called `close(fd).unwrap()` - a panic in a destructor,
/// which this crate has no business doing.
pub struct Terminal {
  fd: OwnedFd,
}

impl Terminal {
  pub fn open(path: &Path) -> Result<Self> {
    // No O_CREAT, so the mode is ignored; greetd passes 0o666 here, which reads
    // as though it means something.
    let fd = open(path, OFlag::O_RDWR | OFlag::O_NOCTTY, Mode::empty())
      .with_context(|| format!("opening terminal {}", path.display()))?;

    Ok(Self { fd })
  }

  /// The terminal we were started on, if stdin is one.
  pub fn stdin() -> Result<Self> {
    let fd = std::io::stdin()
      .as_fd()
      .try_clone_to_owned()
      .context("duplicating stdin")?;

    Ok(Self { fd })
  }

  pub fn ttyname(&self) -> Result<String> {
    let name = nix::unistd::ttyname(self.fd.as_fd()).context("reading the terminal name")?;
    Ok(name.to_string_lossy().into_owned())
  }

  /// Puts the console in text mode.
  ///
  /// Other login managers set graphics mode here, which stops textual sessions
  /// from running at all. greetd deliberately supports those, and there is no
  /// reason to drop that.
  pub fn kd_set_text_mode(&self) -> Result<()> {
    // SAFETY: the fd is an open terminal and KDSETMODE takes its argument by
    // value, so there is no buffer for the kernel to write through.
    unsafe { ffi::kd_setmode(self.fd.as_raw_fd(), ffi::KDTEXT) }.context("setting console mode")?;
    Ok(())
  }

  /// Switches to `target` and waits for the switch to land.
  ///
  /// `VT_SETACTIVATE` resets the VT to auto-switching and activates it in one
  /// call, under the kernel's console lock. The reset matters: a previous
  /// session that grabbed the VT with `VT_PROCESS` and then died would
  /// otherwise leave switching wedged.
  pub fn vt_setactivate(&self, target: u32) -> Result<()> {
    let request = ffi::vt_setactivate {
      console: target,
      mode: ffi::vt_mode {
        mode: ffi::VT_AUTO,
        ..Default::default()
      },
    };

    // SAFETY: `request` is a live, correctly-shaped `vt_setactivate` and the
    // kernel only reads from it.
    unsafe { ffi::vt_setactivate(self.fd.as_raw_fd(), &request) }
      .with_context(|| format!("switching to VT {target}"))?;

    self.vt_waitactive(target)
  }

  pub fn vt_waitactive(&self, target: u32) -> Result<()> {
    // SAFETY: takes the VT number by value.
    unsafe { ffi::vt_waitactive(self.fd.as_raw_fd(), target as i32) }
      .with_context(|| format!("waiting for VT {target}"))?;

    Ok(())
  }

  pub fn vt_get_current(&self) -> Result<u32> {
    let mut state = ffi::vt_state::default();

    // SAFETY: the kernel writes one `vt_state` into `state`, which is live and
    // correctly sized.
    unsafe { ffi::vt_getstate(self.fd.as_raw_fd(), &mut state) }
      .context("reading the active VT")?;

    if state.v_active < 1 {
      bail!("the kernel reported VT {}, which cannot be", state.v_active);
    }

    Ok(u32::from(state.v_active))
  }

  /// The lowest unused VT.
  ///
  /// Allocation is not exclusivity: another process can take the VT between
  /// this call and the switch. Nothing here can prevent that, and greetd has
  /// the same gap.
  pub fn vt_get_next(&self) -> Result<u32> {
    let mut vt: i32 = 0;

    // SAFETY: the kernel writes one `i32` into `vt`.
    unsafe { ffi::vt_openqry(self.fd.as_raw_fd(), &mut vt) }.context("asking for an unused VT")?;

    if vt < 1 {
      bail!("no unused VT is available");
    }

    Ok(vt as u32)
  }

  /// Points stdin, stdout and stderr at this terminal.
  pub fn term_connect_pipes(&self) -> Result<()> {
    let fd = self.fd.as_raw_fd();

    for target in 0..=2 {
      // SAFETY: `fd` is open and `target` is a valid descriptor number; dup2
      // closes whatever was there first.
      if unsafe { libc::dup2(fd, target) } == -1 {
        return Err(std::io::Error::last_os_error())
          .with_context(|| format!("redirecting fd {target} to the terminal"));
      }
    }

    Ok(())
  }

  /// Makes this terminal the controlling terminal of the calling process.
  ///
  /// Requires `setsid()` to have run first, or the kernel has nothing to attach
  /// the terminal to.
  pub fn term_take_ctty(&self) -> Result<()> {
    // SAFETY: takes its argument by value; 1 forces the steal from whichever
    // session held it.
    unsafe { ffi::tiocsctty(self.fd.as_raw_fd(), 1) }.context("taking the controlling terminal")?;

    Ok(())
  }

  /// Homes the cursor and clears the screen, so the previous session's output
  /// doesn't flash past before the greeter paints.
  pub fn term_clear(&self) -> Result<()> {
    let mut file = std::fs::File::from(self.fd.try_clone().context("duplicating the terminal")?);
    file
      .write_all(b"\x1B[H\x1B[2J")
      .context("clearing the terminal")
  }
}
