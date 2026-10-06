//! `read-pass` — masked (echo-off) single-line terminal read.
//!
//! A secret (an API key, a password) must be enterable without appearing on
//! screen, in terminal scrollback, or in shell history. `read-pass` turns the
//! terminal's echo off for the duration of the read, reads one line (the user
//! presses Enter to submit), restores the original termios, and returns the
//! line as a string.
//!
//! # Interpreter-only
//!
//! Like `http-get`/`http-post`, `read-pass` is refused by the AOT C backend
//! and all three transpilers — see [`crate::interpreter_only`]. A compiled
//! binary or a transpiled host program has no AINL termios machinery, and
//! shelling out to a helper would break the standalone-binary guarantee. The
//! two interpreters (tree-walk and the bytecode VM) share this one
//! implementation through the shared prelude, so their masking behaviour is
//! identical by construction.
//!
//! # Inline termios FFI (no `libc` crate)
//!
//! The termios dance is declared as inline FFI rather than pulled in via the
//! `libc` crate. On macOS the `libc` crate unconditionally links `libiconv`
//! (libc 0.2.x `src/unix/bsd/apple/mod.rs` carries `#[link(name = "iconv")]`
//! on the `iconv` extern block, with a comment admitting it cannot make the
//! link conditional on use), which would break the hard `otool -L` zero-dep
//! gate (libSystem-only). Declaring only `tcgetattr`/`tcsetattr` here emits
//! the same symbols and links against libSystem — the binary's only dylib —
//! so no new dylib is added.
//!
//! # The prompt
//!
//! A short prompt is written to **stderr**, never stdout, so the returned
//! string is clean and the prompt can never leak into captured output. Both
//! backends use the exact same prompt string.
//!
//! # Non-tty fallback
//!
//! When there is no controlling terminal (a pipe, CI), `/dev/tty` cannot be
//! opened (or `tcgetattr` fails). Rather than crash, `read-pass` falls back to
//! a plain line read from stdin (echo on). A secret typed into a pipe is not
//! visible on a screen anyway, so the fallback is safe; it just is not masked.

use crate::error::{Error, Result};
use crate::eval::Env;
use crate::value::Value;
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;

/// The name of the builtin this module installs. Kept as a constant because the
/// backend refusal scans for exactly this symbol, and a copy in three places
/// would drift.
pub const READ_PASS: &str = "read-pass";

/// The prompt written to stderr before the read. Both backends use this exact
/// string, so the masking behaviour is identical by construction.
const PROMPT: &str = "read-pass: ";

// ---- inline termios FFI (macOS / BSD) -------------------------------------
// Declared inline rather than via the `libc` crate: on macOS the `libc` crate
// unconditionally links `libiconv` (libc 0.2.x src/unix/bsd/apple/mod.rs,
// `#[link(name = "iconv")]` on the iconv extern block), which breaks the hard
// `otool -L` zero-dep gate (libSystem-only). These two symbols resolve against
// libSystem — the binary's only dylib — so declaring them here adds no dylib.
// (Verified: a minimal C program using tcgetattr/tcsetattr links to
// libSystem.B.dylib only.)

// `struct termios` as laid out by the C ABI. The flag/speed field width and
// the `c_cc` array length differ by platform, and getting them wrong
// misaligns the struct so `tcsetattr` silently writes to the wrong field
// (echo stays on). The layout is therefore pinned per-platform and guarded by
// a compile-time offset check below.
//
//   macOS/BSD : tcflag_t = c_ulong (u64), speed_t = c_ulong (u64), NCCS = 20
//   Linux     : tcflag_t = c_uint  (u32), speed_t = c_uint  (u32), NCCS = 32
//
// (Verified against the macOS SDK: sizeof(termios)=72, offsetof(c_lflag)=24,
// offsetof(c_cc)=32, NCCS=20, ECHO=0o10.)
#[cfg(target_os = "macos")]
type Flag = u64;
#[cfg(not(target_os = "macos"))]
type Flag = u32;

#[cfg(target_os = "macos")]
const NCCS: usize = 20;
#[cfg(not(target_os = "macos"))]
const NCCS: usize = 32;

#[repr(C)]
#[derive(Clone, Copy)]
struct Termios {
    c_iflag: Flag,
    c_oflag: Flag,
    c_cflag: Flag,
    c_lflag: Flag,
    c_cc: [u8; NCCS],
    c_ispeed: Flag,
    c_ospeed: Flag,
}

// Compile-time guard on the FFI layout: a wrong `c_lflag` offset makes
// `tcsetattr` clear a bit in the wrong field (echo silently stays on). This
// catches layout drift at build time; the pty test catches the behaviour.
#[cfg(target_os = "macos")]
const _: () = assert!(std::mem::offset_of!(Termios, c_lflag) == 24);
#[cfg(not(target_os = "macos"))]
const _: () = assert!(std::mem::offset_of!(Termios, c_lflag) == 12);

extern "C" {
    fn tcgetattr(fd: i32, termios_p: *mut Termios) -> i32;
    fn tcsetattr(fd: i32, optional_actions: i32, termios_p: *const Termios) -> i32;
}

/// The `ECHO` bit in `c_lflag` (`0o0000010` = 8, the same value on macOS and
/// Linux). Clearing it turns echo off.
const ECHO: Flag = 0o0000010;
/// `TCSANOW`: apply the new termios immediately.
const TCSANOW: i32 = 0;

/// Bind `read-pass` into `env`.
pub fn install(env: &Env) {
    env.define(
        READ_PASS,
        Value::Builtin {
            name: READ_PASS,
            f: builtin_read_pass,
        },
    );
}

/// `(read-pass)` → the line the user types, masked (echo off) while it is
/// entered. Takes no arguments. The user presses Enter to submit.
pub fn builtin_read_pass(args: &[Value]) -> Result<Value> {
    if !args.is_empty() {
        return Err(Error::runtime("read-pass expects (read-pass)"));
    }
    // Try the controlling terminal first: it is the right thing to mask, and it
    // works even when stdin is redirected.
    match read_masked_line() {
        Ok(s) => Ok(Value::str(s)),
        // No controlling terminal (a pipe, CI): fall back to a plain line read
        // from stdin. Echo stays on, so it is not masked — but it does not
        // crash, and a secret typed into a pipe is not visible on a screen.
        Err(()) => read_plain_line(),
    }
}

/// Read one masked line from the controlling terminal (`/dev/tty`): turn echo
/// off, read until Enter, restore the original termios, and return the line.
/// Returns `Err(())` when there is no controlling terminal (the caller falls
/// back to a plain stdin read).
fn read_masked_line() -> std::result::Result<String, ()> {
    // Open the controlling terminal. Failing here means there is no tty, so the
    // caller falls back rather than crashing.
    let mut tty = match std::fs::File::open("/dev/tty") {
        Ok(f) => f,
        Err(_) => return Err(()),
    };
    let fd = tty.as_raw_fd();
    // Save the current termios. If this is not a tty, `tcgetattr` fails and we
    // fall back (do not crash).
    let mut orig = unsafe { std::mem::zeroed::<Termios>() };
    if unsafe { tcgetattr(fd, &mut orig) } != 0 {
        return Err(());
    }
    // Turn echo off. Canonical mode is left on, so the read blocks until the
    // user presses Enter — the "Enter to submit" contract.
    let mut masked = orig;
    masked.c_lflag &= !ECHO;
    if unsafe { tcsetattr(fd, TCSANOW, &masked) } != 0 {
        // Best-effort restore, then fall back: we could not mask, so do not
        // pretend to have read a masked line.
        unsafe { tcsetattr(fd, TCSANOW, &orig) };
        return Err(());
    }
    // The prompt goes to stderr, never stdout, so the returned string is clean.
    let _ = write!(std::io::stderr(), "{PROMPT}");
    let _ = std::io::stderr().flush();
    // Read one line (canonical mode: blocks until Enter or EOF).
    let bytes = read_line_bytes(&mut tty);
    // Restore the original termios ALWAYS before returning, so a later read
    // (or the user's shell) sees echo back on.
    unsafe { tcsetattr(fd, TCSANOW, &orig) };
    match bytes {
        Ok(b) => Ok(String::from_utf8_lossy(&b).into_owned()),
        Err(()) => Err(()),
    }
}

/// Read bytes from `tty` until a newline (exclusive) or EOF. Returns the line
/// bytes without the trailing newline.
fn read_line_bytes(tty: &mut std::fs::File) -> std::result::Result<Vec<u8>, ()> {
    let mut line = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        let n = tty.read(&mut buf).map_err(|_| ())?;
        if n == 0 {
            return Ok(line);
        }
        let chunk = &buf[..n];
        if let Some(pos) = chunk.iter().position(|&b| b == b'\n') {
            line.extend_from_slice(&chunk[..pos]);
            return Ok(line);
        }
        line.extend_from_slice(chunk);
    }
}

/// The non-tty fallback: read one line from stdin (echo on). A missing/EOF
/// stdin yields an empty string rather than an error, so a script that pipes an
/// empty secret does not crash.
fn read_plain_line() -> Result<Value> {
    use std::io::BufRead;
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) => Ok(Value::str(String::new())),
        Ok(_) => {
            line.pop(); // strip the trailing newline
            Ok(Value::str(line))
        }
        Err(e) => Err(Error::runtime(format!("read-pass: cannot read stdin: {e}"))),
    }
}
