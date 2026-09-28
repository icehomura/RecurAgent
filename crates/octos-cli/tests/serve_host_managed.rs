//! Integration test: `octos serve --host-managed` (UPCR-2026-036) with the REAL
//! binary. The host owns the process: closing its end of stdin stops the
//! server, no pairing code is printed, and an inherited listener descriptor is
//! served instead of a freshly bound port.
//!
//! Unix-only (descriptor passing and process control), like `serve_sigterm`.

#[cfg(unix)]
#[allow(unsafe_code)]
mod serve_host_managed {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Child, Command, Stdio};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    static SERIAL: Mutex<()> = Mutex::new(());

    const HOST: &str = "host-managed-e2e-host-token-0123456789abcdef";
    const EXTERNAL: &str = "host-managed-e2e-external-token-0123456789ab";

    fn octos_binary() -> std::path::PathBuf {
        if cfg!(feature = "api") {
            return env!("CARGO_BIN_EXE_octos").into();
        }
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let target_dir = std::path::Path::new(manifest_dir).join("../../target/serve-host-managed");
        let out = Command::new("cargo")
            .args(["build", "-p", "octos-cli", "--features", "api"])
            .current_dir(std::path::Path::new(manifest_dir).join("../.."))
            .env("CARGO_TARGET_DIR", &target_dir)
            .output()
            .expect("failed to bootstrap api-enabled octos binary");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        target_dir.join("debug/octos")
    }

    fn command(data_dir: &std::path::Path, extra: &[&str]) -> Command {
        let mut cmd = Command::new(octos_binary());
        cmd.args([
            "serve",
            "--host-managed",
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--instance-data-dir",
            data_dir.to_str().unwrap(),
        ])
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("OCTOS_AUTH_TOKEN", HOST)
        .env("OCTOS_HOST_EXTERNAL_TOKEN", EXTERNAL)
        .env("NO_COLOR", "1")
        .env_remove("OCTOS_INSTANCE_DATA_DIR")
        .env_remove("OCTOS_HOME")
        .env_remove("OCTOS_DATA_DIR")
        .env_remove("OCTOS_SOLO_LOGIN");
        cmd
    }

    /// Read stdout until the listener announcement; collect every line.
    fn announced_port(child: &mut Child) -> (u16, std::sync::mpsc::Receiver<String>) {
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let line = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("the server did not announce its listener");
            assert!(
                !line.contains("Pairing code") && !line.contains("pair="),
                "{line}"
            );
            if let Some(origin) = line.strip_prefix("Listening: http://127.0.0.1:") {
                return (origin.trim().parse().unwrap(), rx);
            }
        }
    }

    fn health(port: u16) -> String {
        let mut tcp = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write!(
            tcp,
            "GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        let _ = tcp.read_to_string(&mut response);
        response.lines().next().unwrap_or_default().to_owned()
    }

    fn exits_within(child: &mut Child, limit: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait().unwrap() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        None
    }

    #[test]
    fn serve_host_managed_stops_when_the_host_closes_stdin() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let mut child = command(dir.path(), &["--port", "0"]).spawn().unwrap();
        let (port, lines) = announced_port(&mut child);
        assert!(health(port).contains(" 200 "));
        // Other bytes on stdin are ignored; only EOF stops the server.
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"ignored\n")
            .unwrap();
        assert!(exits_within(&mut child, Duration::from_secs(1)).is_none());
        drop(child.stdin.take());
        let status = exits_within(&mut child, Duration::from_secs(30)).unwrap_or_else(|| {
            let _ = child.kill();
            panic!("the server outlived its host's stdin");
        });
        assert!(status.success(), "{status}");
        let printed: Vec<String> = lines.try_iter().collect();
        assert!(
            !printed
                .iter()
                .any(|line| line.contains(HOST) || line.contains(EXTERNAL)),
            "no token on stdout: {printed:?}"
        );
    }

    #[test]
    fn serve_host_managed_serves_the_listener_the_host_keeps() {
        use std::os::fd::AsRawFd;
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let fd = listener.as_raw_fd();
        let mut cmd = command(dir.path(), &["--listen-fd", "3"]);
        // SAFETY: only async-signal-safe dup2/fcntl run between fork and
        // exec. They place the host's listener at descriptor 3 and clear its
        // CLOEXEC (dup2 onto itself would keep the flag).
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                if fd != 3 && libc::dup2(fd, 3) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(3, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().unwrap();
        let (announced, _lines) = announced_port(&mut child);
        assert_eq!(announced, port, "the server announces the inherited port");
        assert!(health(port).contains(" 200 "));
        drop(child.stdin.take());
        assert!(
            exits_within(&mut child, Duration::from_secs(30)).is_some_and(|s| s.success()),
            "the server stops on stdin EOF"
        );
        // The host still owns the port: a client connects (queued in the
        // backlog) and no other process could have bound it meanwhile.
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());
        assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_err());
        drop(listener);
    }
}
