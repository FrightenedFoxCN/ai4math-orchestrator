use ai4math_broker::run_task_process;
use std::{path::Path, time::Duration};

#[cfg(not(target_os = "macos"))]
#[test]
fn no_unsandboxed_fallback() {
    let error = run_task_process(
        Path::new("/usr/bin/true"),
        &"0".repeat(64),
        b"{}",
        Duration::from_secs(1),
    )
    .unwrap_err();
    assert!(error.to_string().contains("requires macOS Seatbelt"));
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{
        fs::{self, File},
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        os::unix::net::UnixListener,
        process::Command,
        time::Instant,
    };

    fn digest(path: &Path) -> String {
        format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
    }

    fn non_cloexec(fd: &impl AsRawFd) -> OwnedFd {
        let duplicate = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD, 128) };
        assert!(duplicate >= 128, "duplicate fixture descriptor");
        assert_eq!(unsafe { libc::fcntl(duplicate, libc::F_GETFD) }, 0);
        unsafe { OwnedFd::from_raw_fd(duplicate) }
    }

    #[test]
    #[ignore = "requires native macOS Seatbelt host"]
    fn native_worker_isolated_and_bounded() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"));
        let room = tempfile::Builder::new()
            .prefix("task-test-")
            .tempdir_in("/private/tmp")
            .unwrap();
        let fixture = room.path().join("native-fixture");
        let compiler = Command::new("/usr/bin/clang")
            .args(["-Wall", "-Wextra", "-Werror"])
            .arg(workspace.join("tests/fixtures/task_process.c"))
            .arg("-o")
            .arg(&fixture)
            .output()
            .unwrap();
        assert!(
            compiler.status.success(),
            "fixture compilation: {}",
            String::from_utf8_lossy(&compiler.stderr)
        );
        let pin = digest(&fixture);
        let timeout = Duration::from_secs(5);

        let protected = workspace.join("Cargo.toml");
        let readable = File::open(&protected).unwrap();
        let file_fd = non_cloexec(&readable);
        let socket_path = room.path().join("socket");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let socket_fd = non_cloexec(&listener);
        let input = serde_json::to_vec(&(
            protected,
            socket_path,
            file_fd.as_raw_fd(),
            socket_fd.as_raw_fd(),
        ))
        .unwrap();
        let result = run_task_process(&fixture, &pin, &input, timeout).unwrap();
        assert_eq!(result, br#"{"isolated":true}"#);

        let failed = run_task_process(&fixture, &pin, br#""exit""#, timeout)
            .unwrap_err()
            .to_string();
        assert!(failed.contains("42"), "{failed}");
        assert!(
            !failed.contains("stderr-is-not-result")
                && !failed.contains("stdout-is-not-an-error-diagnostic")
        );
        assert_eq!(
            run_task_process(&fixture, &pin, br#""full""#, timeout)
                .unwrap()
                .len(),
            1024 * 1024
        );
        let overflow = run_task_process(&fixture, &pin, br#""overflow""#, timeout)
            .unwrap_err()
            .to_string();
        assert!(overflow.contains("stdout exceeds"), "{overflow}");

        let start = Instant::now();
        let blocked = run_task_process(&fixture, &pin, br#""wait""#, Duration::from_millis(150))
            .unwrap_err()
            .to_string();
        assert!(blocked.contains("timed out"), "{blocked}");
        assert!(start.elapsed() < timeout, "timeout cleanup took too long");

        let wrong_pin = run_task_process(&fixture, &"0".repeat(64), b"{}", timeout)
            .unwrap_err()
            .to_string();
        assert!(wrong_pin.contains("SHA-256 mismatch"), "{wrong_pin}");
        assert!(
            run_task_process(Path::new("native-fixture"), &pin, b"{}", timeout)
                .unwrap_err()
                .to_string()
                .contains("must be absolute")
        );
        assert!(
            run_task_process(&fixture, &pin, &vec![0; 1024 * 1024 + 1], timeout)
                .unwrap_err()
                .to_string()
                .contains("input exceeds")
        );
        assert!(
            run_task_process(&fixture, &pin, b"{}", Duration::from_secs(61))
                .unwrap_err()
                .to_string()
                .contains("timeout must be")
        );
        let script = room.path().join("script");
        fs::write(&script, b"#!/bin/sh\necho untrusted\n").unwrap();
        let not_native = run_task_process(&script, &digest(&script), b"{}", timeout)
            .unwrap_err()
            .to_string();
        assert!(not_native.contains("native Mach-O"), "{not_native}");
        let oversized = room.path().join("oversized");
        File::create(&oversized)
            .unwrap()
            .set_len(64 * 1024 * 1024 + 1)
            .unwrap();
        assert!(
            run_task_process(&oversized, &pin, b"{}", timeout)
                .unwrap_err()
                .to_string()
                .contains("executable exceeds")
        );
    }
}
