//! `read-pass` pty acceptance test — the crux of the card.
//!
//! Drives the real `ainl` binary in a real pty (via `forkpty`), types a
//! secret + Enter, and asserts the three card behaviours:
//!   (a) the secret does NOT appear in the captured pty stream (echo was off);
//!   (b) the program's returned string == the secret (the read actually works);
//!   (c) a second line typed AFTER the read IS echoed (echo was restored).
//!
//! Run on both backends (the bytecode VM and the tree-walk interpreter) to
//! prove their masking behaviour is identical — the 2-backend parity gate.
//!
//! `libc` is a *dev-dependency* here (only the test harness uses `forkpty`); it
//! is never linked into the release `ainl` binary, so the `otool -L` zero-dep
//! gate is unaffected. The `read-pass` builtin itself uses inline termios FFI
//! in `ainl-core`.

#[cfg(unix)]
mod pty {
    use std::ffi::CString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// The AINL program run under the pty. It:
    ///   1. reads a masked line (`read-pass`) — the secret;
    ///   2. writes that returned string to a file (the "returned string"
    ///      channel, so the test can assert it equals the secret without
    ///      parsing stdout);
    ///   3. sleeps, keeping the pty open so the test can type a second line
    ///      and observe whether echo has been restored.
    ///
    /// The file path is absolute (the test substitutes it in) so it is
    /// independent of the child's cwd.
    const PROG_TEMPLATE: &str = "\
; read-pass pty acceptance program.
(def s (read-pass))
(write-file \"__OUT__\" s)
(sleep 5)
";

    /// The exact prompt `read-pass` writes to stderr before the masked read
    /// (see `ainl-core::read_pass::PROMPT`). It is written *after* echo is
    /// turned off, so its arrival on the pty is the "echo is now off" signal.
    const PROMPT: &str = "read-pass: ";

    /// Locate the `ainl` binary. The test binary lives in `target/<profile>/
    /// deps/`, and `ainl` is one level up in `target/<profile>/`.
    fn ainl_bin() -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let deps = exe.parent().unwrap(); // target/<profile>/deps
        let profile = deps.parent().unwrap(); // target/<profile>
        let bin = profile.join("ainl");
        assert!(
            bin.exists(),
            "ainl binary not found at {bin:?}; cargo builds the bin before the test"
        );
        bin
    }

    /// Write the AINL program into `dir`, substituting the absolute `out_file`
    /// path, and return the program path.
    fn write_prog(dir: &Path, out_file: &Path) -> PathBuf {
        let prog = dir.join("prog.ainl");
        let src = PROG_TEMPLATE.replace("__OUT__", out_file.to_str().unwrap());
        fs::write(&prog, src).unwrap();
        prog
    }

    /// Read whatever is available on the pty master right now (non-blocking).
    fn drain(master: libc::c_int, out: &mut Vec<u8>) {
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(master, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n > 0 {
                out.extend_from_slice(&buf[..n as usize]);
            } else {
                break; // 0 (EOF) or EAGAIN/EIO — nothing more right now
            }
        }
    }

    /// Write `bytes` to the pty master, retrying on EAGAIN.
    fn pty_write(master: libc::c_int, bytes: &[u8]) {
        let mut p = bytes.as_ptr();
        let mut remaining = bytes.len();
        while remaining > 0 {
            let n = unsafe { libc::write(master, p as *const libc::c_void, remaining) };
            if n > 0 {
                let n = n as usize;
                p = unsafe { p.add(n) };
                remaining -= n;
            } else {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::WouldBlock {
                    break;
                }
            }
        }
    }

    /// Run `ainl run <prog> [--tree-walk]` in a fresh pty, type `secret` +
    /// Enter, wait for the program to write `out_file`, then type `visible` +
    /// Enter and capture the whole pty stream. Returns the captured bytes.
    fn run_in_pty(
        ainl: &Path,
        prog: &Path,
        out_file: &Path,
        tree_walk: bool,
        secret: &str,
    ) -> Vec<u8> {
        // Build the argv (kept alive for the duration of the child branch).
        let mut args: Vec<CString> = vec![
            CString::new(ainl.to_str().unwrap()).unwrap(),
            CString::new("run").unwrap(),
            CString::new(prog.to_str().unwrap()).unwrap(),
        ];
        if tree_walk {
            args.push(CString::new("--tree-walk").unwrap());
        }
        let mut argv: Vec<*const libc::c_char> = args.iter().map(|c| c.as_ptr()).collect();
        argv.push(std::ptr::null());

        let mut master: libc::c_int = 0;
        let mut ws = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pid = unsafe {
            libc::forkpty(
                &mut master,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut ws,
            )
        };
        assert!(
            pid >= 0,
            "forkpty failed: {}",
            std::io::Error::last_os_error()
        );

        if pid == 0 {
            // Child: `forkpty` has already made the pty slave the controlling
            // terminal, so `/dev/tty` inside the child resolves to this pty —
            // exactly the situation `read-pass` is meant to handle. exec now.
            unsafe {
                libc::execv(argv[0], argv.as_ptr());
            }
            // execv only returns on failure.
            unsafe {
                libc::_exit(127);
            }
        }

        // Parent: make the master non-blocking so reads can poll.
        unsafe {
            let flags = libc::fcntl(master, libc::F_GETFL);
            libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        let mut out: Vec<u8> = Vec::new();

        // Wait for the `read-pass: ` prompt on the pty. The prompt is written to
        // stderr *after* `tcsetattr` has turned echo off, so its arrival is the
        // exact signal that the masked read is live and echo is off. Typing the
        // secret before this would race the builtin and echo it (a false leak).
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            drain(master, &mut out);
            if out.windows(PROMPT.len()).any(|w| w == PROMPT.as_bytes()) {
                break;
            }
            if Instant::now() > deadline {
                panic!(
                    "read-pass prompt never appeared within 10s; captured so far:\n{}",
                    String::from_utf8_lossy(&out)
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        // Echo is now off. Type the secret + Enter; it must NOT appear in `out`.
        pty_write(master, format!("{secret}\n").as_bytes());

        // Wait for the program to write the returned string to `out_file`.
        // That write happens right after the masked read returns, so once the
        // file exists, the read is done and echo has been restored.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            drain(master, &mut out);
            if out_file.exists() {
                break;
            }
            if Instant::now() > deadline {
                panic!(
                    "program did not write {out_file:?} within 10s; captured so far:\n{}",
                    String::from_utf8_lossy(&out)
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        // Echo is now restored. Type a visible line + Enter; it MUST be echoed.
        pty_write(master, b"visible\n");

        // Let the echo of "visible" arrive, then drain until the child exits.
        std::thread::sleep(Duration::from_millis(300));
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            drain(master, &mut out);
            let mut status: libc::c_int = 0;
            let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if r == pid {
                break;
            }
            if Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Final drain after the child is gone.
        std::thread::sleep(Duration::from_millis(150));
        drain(master, &mut out);

        unsafe {
            libc::close(master);
            // Reap if we broke out before waitpid reported it.
            let mut status: libc::c_int = 0;
            libc::waitpid(pid, &mut status, 0);
        }
        out
    }

    fn check(tag: &str, tree_walk: bool) {
        let ainl = ainl_bin();
        let dir = std::env::temp_dir().join(format!("readpass_pty_{tag}_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let out_file = dir.join("out.txt");
        let _ = fs::remove_file(&out_file);
        let prog = write_prog(&dir, &out_file);

        let secret = "s3cr3t-pass-1234";
        let captured = run_in_pty(&ainl, &prog, &out_file, tree_walk, secret);
        let text = String::from_utf8_lossy(&captured).into_owned();

        // (b) the returned string equals the secret.
        let got = fs::read_to_string(&out_file)
            .unwrap_or_else(|e| panic!("could not read {out_file:?}: {e}"));
        assert_eq!(
            got, secret,
            "[{tag}] returned string != secret (got {got:?})"
        );

        // (a) the secret never appeared on the pty (echo was off during the read).
        assert!(
            !text.contains(secret),
            "[{tag}] SECRET LEAKED into the pty stream — echo was not off.\ncaptured:\n{text}"
        );

        // (c) echo was restored: the line typed after the read is echoed back.
        assert!(
            text.contains("visible"),
            "[{tag}] echo was NOT restored — the post-read line was not echoed.\ncaptured:\n{text}"
        );

        // The prompt was written to stderr (and thus to the pty) — sanity check
        // that the read-pass tty path actually ran (not the non-tty fallback).
        assert!(
            text.contains("read-pass:"),
            "[{tag}] prompt not seen on the pty — did read-pass run on the tty?\ncaptured:\n{text}"
        );
    }

    #[test]
    fn read_pass_pty_vm() {
        check("vm", false);
    }

    #[test]
    fn read_pass_pty_tree_walk() {
        check("tree_walk", true);
    }
}
