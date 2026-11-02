//! Local transport between the native host (server) and its clients (the CLI
//! today, the gateway later). Centralised here so both sides cannot drift
//! apart.
//!
//! The endpoint is a plain string with a per-platform meaning:
//!
//! - Unix (macOS, Linux): the path of a Unix domain socket. Resolution rules
//!   (in order):
//!   1. `$XDG_RUNTIME_DIR/cgpt-bridge.sock` if XDG_RUNTIME_DIR is set and
//!      non-empty (typical Linux user-session path).
//!   2. `$TMPDIR/cgpt-bridge.<uid>.sock` if TMPDIR is set and non-empty
//!      (macOS path).
//!   3. `/tmp/cgpt-bridge-<uid>/cgpt-bridge.sock` otherwise; the host creates
//!      the directory 0700 and refuses one that is not ours or not private.
//!
//!   The socket file is 0600. Clients connect, and the host replaces a stale
//!   file, only if it is a socket (not a symlink) owned by the current uid;
//!   for the step 3 path clients also check the directory as the host does.
//!
//!   macOS keeps `sun_path` at 104 bytes, and the standard `$TMPDIR`
//!   (`/var/folders/...`) is already ~49 chars. Our short filename keeps us
//!   well clear of that cap.
//! - Windows: the name of a named pipe, `cgpt-bridge-<user>` (the pipe lives
//!   at `\\.\pipe\cgpt-bridge-<user>`).
//!
//! Streams and listeners are `interprocess::local_socket` types. Its Windows
//! streams have no read/write timeouts, so [`round_trip`] enforces the
//! per-request deadline with a helper thread on every platform.
//!
//! Two interprocess (2.4) behaviours on Windows shape this module:
//! - its local-socket connect ignores the wait mode and waits for a busy pipe
//!   forever, so [`connect`] goes through the named-pipe API, which honours
//!   the timeout;
//! - `flush()` on a local-socket stream is a no-op, and unread data dies with
//!   the server's pipe handle, so a server calls [`flush_to_peer`] before it
//!   lets go of a reply (see there).
//!
//! A client that connects and disconnects before the server's `accept` gets
//! to it is dropped by interprocess on Windows and never reaches `accept`.
//! Probe connections (the liveness probe in [`listen`]) may or may not show
//! up there; servers must not count on them.
//!
//! On Windows the pipe gets an explicit owner-only security descriptor
//! ([`OWNER_ONLY_SDDL`]): only the account that started the host (and
//! SYSTEM) may connect, never another user of the same machine.

#[cfg(unix)]
use std::fs;
use std::io::{self, BufReader, BufWriter, Write};
#[cfg(all(unix, not(target_os = "linux")))]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

#[cfg(unix)]
use interprocess::local_socket::ConnectOptions;
use interprocess::local_socket::{prelude::*, ListenerOptions, Name};
#[cfg(unix)]
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
use interprocess::ConnectWaitMode;

use crate::framing::{read_frame, write_frame, ReadFrameError};

pub use interprocess::local_socket::{Listener as LocalListener, Stream as LocalStream};

/// How long the liveness probe in [`listen`] waits for a connect.
const PROBE_TIMEOUT_MS: u64 = 1_000;

/// DACL of the Windows pipe: protected (no inherited entries), full access
/// for the owner and SYSTEM only.
#[cfg(windows)]
const OWNER_ONLY_SDDL: &str = "D:P(A;;GA;;;OW)(A;;GA;;;SY)";

#[cfg(unix)]
pub fn default_socket_path() -> PathBuf {
    let var = |name| std::env::var(name).ok().filter(|v: &String| !v.is_empty());
    if let Some(xdg) = var("XDG_RUNTIME_DIR") {
        return PathBuf::from(xdg).join("cgpt-bridge.sock");
    }
    match var("TMPDIR") {
        Some(tmp) => PathBuf::from(tmp).join(format!("cgpt-bridge.{}.sock", current_uid())),
        None => fallback_dir().join("cgpt-bridge.sock"),
    }
}

/// Where the socket goes without `XDG_RUNTIME_DIR` and `TMPDIR` (ssh without
/// logind, cron, containers): a per-user directory, so the socket is not in
/// the shared `/tmp` itself. [`listen`] creates it 0700 and checks it.
#[cfg(unix)]
fn fallback_dir() -> PathBuf {
    PathBuf::from(format!("/tmp/cgpt-bridge-{}", current_uid()))
}

#[cfg(unix)]
fn current_uid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

#[cfg(unix)]
fn refuse(path: &Path, why: String) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("refusing {}: {}", path.display(), why),
    )
}

/// The endpoint must be a socket (not a symlink to one) owned by `uid`:
/// anything else may have been planted by another local user.
#[cfg(unix)]
fn check_own_socket(path: &Path, meta: &fs::Metadata, uid: u32) -> io::Result<()> {
    if !meta.file_type().is_socket() {
        return Err(refuse(path, "not a socket".to_string()));
    }
    if meta.uid() != uid {
        return Err(refuse(
            path,
            format!("owned by uid {}, not {}", meta.uid(), uid),
        ));
    }
    Ok(())
}

/// A directory (not a symlink) owned by `uid` with no access for group and
/// others.
#[cfg(unix)]
fn check_private_dir(path: &Path, meta: &fs::Metadata, uid: u32) -> io::Result<()> {
    if !meta.file_type().is_dir() {
        return Err(refuse(path, "not a directory".to_string()));
    }
    if meta.uid() != uid {
        return Err(refuse(
            path,
            format!("owned by uid {}, not {}", meta.uid(), uid),
        ));
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(refuse(
            path,
            format!("mode {:04o}, expected no access for group and others", mode),
        ));
    }
    Ok(())
}

/// The directory [`listen`] creates and both sides check when the endpoint
/// sits directly in it: the `/tmp` fallback on Unix, none on Windows. Shared
/// parents (`/tmp`, a `--socket` folder) are never checked or touched.
fn private_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        Some(fallback_dir())
    }
    #[cfg(windows)]
    {
        None
    }
}

/// `private_dir` if `endpoint` sits directly in it.
#[cfg(unix)]
fn private_parent<'a>(endpoint: &str, private_dir: Option<&'a Path>) -> Option<&'a Path> {
    private_dir.filter(|dir| Path::new(endpoint).parent() == Some(*dir))
}

/// Create `dir` 0700 if missing, then check it with [`check_private_dir`].
/// An existing directory is never loosened or taken over.
#[cfg(unix)]
fn ensure_private_dir(dir: &Path, uid: u32) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    check_private_dir(dir, &fs::symlink_metadata(dir)?, uid)
}

/// Unix: the socket path (same as [`default_socket_path`]). Windows: the pipe
/// name `cgpt-bridge-<user>`, user taken from `USERNAME`.
pub fn default_endpoint() -> String {
    #[cfg(unix)]
    {
        default_socket_path().to_string_lossy().into_owned()
    }
    #[cfg(windows)]
    {
        pipe_name_for_user(&std::env::var("USERNAME").unwrap_or_default())
    }
}

/// `cgpt-bridge-<user>` with everything outside `[A-Za-z0-9_-]` replaced by
/// `_`, so the name is a valid pipe name for any account name.
#[cfg(any(windows, test))]
fn pipe_name_for_user(user: &str) -> String {
    let clean: String = user
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let clean = if clean.is_empty() {
        "user".to_string()
    } else {
        clean
    };
    format!("cgpt-bridge-{}", clean)
}

fn endpoint_name(endpoint: &str) -> io::Result<Name<'_>> {
    #[cfg(unix)]
    {
        endpoint.to_fs_name::<GenericFilePath>()
    }
    #[cfg(windows)]
    {
        endpoint.to_ns_name::<GenericNamespaced>()
    }
}

/// Connect to a listening endpoint, waiting at most `timeout_ms` for the
/// connection itself (a busy endpoint is `ErrorKind::TimedOut` after that).
/// Reads and writes are bounded by [`round_trip`].
pub fn connect(endpoint: &str, timeout_ms: u64) -> io::Result<LocalStream> {
    connect_with(endpoint, timeout_ms, private_dir().as_deref())
}

fn connect_with(
    endpoint: &str,
    timeout_ms: u64,
    private_dir: Option<&Path>,
) -> io::Result<LocalStream> {
    let wait = ConnectWaitMode::Timeout(Duration::from_millis(timeout_ms));
    #[cfg(unix)]
    {
        // A missing directory or endpoint stays `NotFound` ("host not
        // running").
        if let Some(dir) = private_parent(endpoint, private_dir) {
            check_private_dir(dir, &fs::symlink_metadata(dir)?, current_uid())?;
        }
        let path = Path::new(endpoint);
        check_own_socket(path, &fs::symlink_metadata(path)?, current_uid())?;
        ConnectOptions::new()
            .name(endpoint_name(endpoint)?)
            .wait_mode(wait)
            .connect_sync()
    }
    #[cfg(windows)]
    {
        let _ = private_dir;
        // `ConnectOptions::connect_sync` would wait for a busy pipe without
        // a bound; the named-pipe connect takes the timeout.
        use interprocess::os::windows::named_pipe::{
            local_socket::Stream as PipeLocalStream, pipe_mode, DuplexPipeStream,
        };
        use std::os::windows::io::OwnedHandle;
        let pipe = DuplexPipeStream::<pipe_mode::Bytes>::connect_by_path_with_wait_mode(
            format!(r"\\.\pipe\{}", endpoint),
            wait,
        )?;
        let handle = OwnedHandle::try_from(pipe)
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "named pipe stream is split"))?;
        Ok(LocalStream::from(PipeLocalStream::try_from(handle)?))
    }
}

/// Wait until the peer has read everything written to `stream`. A server
/// calls this after its reply and before dropping the stream.
///
/// Unix: nothing to do, written bytes sit in the kernel and outlive us.
/// Windows: unread pipe data is discarded when the server's handle closes,
/// and the process exiting closes it. interprocess hands a dropped stream to
/// background "limbo" threads that flush first, but those die with the
/// process too; flushing here makes the reply safe before we go on.
pub fn flush_to_peer(stream: &LocalStream) -> io::Result<()> {
    #[cfg(unix)]
    {
        let _ = stream;
        Ok(())
    }
    #[cfg(windows)]
    {
        match stream {
            LocalStream::NamedPipe(pipe) => pipe.inner().flush(),
        }
    }
}

/// Start listening on an endpoint. A live listener already there is an
/// `AddrInUse` error (single-instance guarantee); a dead one is taken over:
/// on Unix a stale socket file is removed, on Windows a pipe dies with its
/// process, so there is nothing to clean up.
pub fn listen(endpoint: &str) -> io::Result<LocalListener> {
    listen_with(endpoint, private_dir().as_deref())
}

fn listen_with(endpoint: &str, private_dir: Option<&Path>) -> io::Result<LocalListener> {
    #[cfg(unix)]
    if let Some(dir) = private_parent(endpoint, private_dir) {
        ensure_private_dir(dir, current_uid())?;
    }
    match create_listener(endpoint) {
        Ok(l) => Ok(l),
        Err(e) if is_in_use(&e) => {
            // Probe: if we can connect, another live host owns the endpoint.
            if connect_with(endpoint, PROBE_TIMEOUT_MS, private_dir).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "another cgpt-bridge-host appears to be running",
                ));
            }
            #[cfg(unix)]
            {
                // Stale socket file. Unlink and retry exactly once, but only
                // our own socket: never a file or symlink someone put there.
                let path = Path::new(endpoint);
                check_own_socket(path, &fs::symlink_metadata(path)?, current_uid())?;
                fs::remove_file(endpoint)?;
                create_listener(endpoint)
            }
            #[cfg(not(unix))]
            Err(e)
        }
        Err(e) => Err(e),
    }
}

fn create_listener(endpoint: &str) -> io::Result<LocalListener> {
    let options = ListenerOptions::new().name(endpoint_name(endpoint)?);
    #[cfg(windows)]
    let options = {
        use interprocess::os::windows::local_socket::ListenerOptionsExt;
        use interprocess::os::windows::security_descriptor::SecurityDescriptor;
        let sddl = widestring::U16CString::from_str(OWNER_ONLY_SDDL)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        options.security_descriptor(SecurityDescriptor::deserialize(&sddl)?)
    };
    // Linux: interprocess `fchmod`s the socket to 0600 before bind, so the
    // file never exists with the umask's mode, even in a shared directory.
    #[cfg(target_os = "linux")]
    let options = {
        use interprocess::os::unix::local_socket::ListenerOptionsExt;
        options.mode(0o600)
    };
    let listener = options.create_sync()?;
    // Other Unix (macOS): `fchmod` on a socket is unsupported, so the file is
    // created 0777 & ~umask and tightened right after bind. Until then only
    // the directory protects it (0700 in every default location). On failure
    // the dropped listener unlinks the file.
    #[cfg(all(unix, not(target_os = "linux")))]
    fs::set_permissions(endpoint, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Creating a second instance of a named pipe fails with "access denied",
/// not "address in use".
fn is_in_use(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::AddrInUse
        || (cfg!(windows) && e.kind() == io::ErrorKind::PermissionDenied)
}

/// Wait for the next client connection.
pub fn accept(listener: &LocalListener) -> io::Result<LocalStream> {
    listener.accept()
}

/// Give the endpoint back on shutdown: unlink the socket file on Unix so the
/// next host instance can bind. Nothing to do for a Windows pipe.
pub fn release(endpoint: &str) {
    #[cfg(unix)]
    if let Err(e) = std::fs::remove_file(endpoint) {
        if e.kind() != io::ErrorKind::NotFound {
            eprintln!("cgpt-bridge: failed to remove socket {}: {}", endpoint, e);
        }
    }
    #[cfg(not(unix))]
    let _ = endpoint;
}

/// One request/response round trip on a connected stream: write `request` as
/// a frame, read one frame back. Gives up after `timeout` (a read that does
/// not finish in time is `ReadFrameError::Io` with `ErrorKind::TimedOut`).
///
/// The exchange runs in a helper thread because Windows pipe streams cannot
/// time out natively. After a timeout the thread stays blocked until the
/// server closes the connection; the stream is dropped then.
pub fn round_trip(
    stream: LocalStream,
    request: &[u8],
    timeout: Duration,
) -> Result<Vec<u8>, ReadFrameError> {
    let request = request.to_vec();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = (|| {
            let mut writer = BufWriter::new(&stream);
            write_frame(&mut writer, &request)?;
            writer.flush()?;
            read_frame(&mut BufReader::new(&stream))
        })();
        let _ = tx.send(result);
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(ReadFrameError::Io(io::Error::new(
            io::ErrorKind::TimedOut,
            "no response within the timeout",
        ))),
        Err(RecvTimeoutError::Disconnected) => Err(ReadFrameError::Io(io::Error::other(
            "exchange thread ended without a result",
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    /// Bound for every blocking step in these tests: a hang fails the test
    /// with the stage's name instead of stalling the run.
    const STAGE_LIMIT: Duration = Duration::from_secs(10);

    /// Join `handle`, failing with `stage` in the message if it is still
    /// blocked after [`STAGE_LIMIT`].
    fn join_within<T>(handle: thread::JoinHandle<T>, stage: &str) -> T {
        let started = std::time::Instant::now();
        while !handle.is_finished() {
            assert!(
                started.elapsed() < STAGE_LIMIT,
                "{}: still blocked after {:?}",
                stage,
                STAGE_LIMIT
            );
            thread::sleep(Duration::from_millis(10));
        }
        handle.join().expect(stage)
    }

    // Unique per test and per process, and short: macOS caps `sun_path` at
    // 104 bytes, so use /tmp directly rather than the long `$TMPDIR`.
    fn test_endpoint(tag: &str) -> String {
        #[cfg(unix)]
        {
            format!("/tmp/cgb-ls-{}-{}.sock", std::process::id(), tag)
        }
        #[cfg(windows)]
        {
            format!("cgb-ls-{}-{}", std::process::id(), tag)
        }
    }

    // Same scheme for test directories, without the `.sock` suffix.
    #[cfg(unix)]
    fn test_dir(tag: &str) -> String {
        format!("/tmp/cgb-ls-{}-{}", std::process::id(), tag)
    }

    // Both branches of `default_socket_path` are exercised in a single test
    // because they both mutate process-wide env vars. Splitting them would
    // race under the default parallel test runner.
    #[cfg(unix)]
    #[test]
    fn endpoint_unix_matches_old_socket_path() {
        let prev_xdg = std::env::var("XDG_RUNTIME_DIR").ok();
        let prev_tmp = std::env::var("TMPDIR").ok();

        // Branch 1: XDG_RUNTIME_DIR set → that path wins.
        std::env::set_var("XDG_RUNTIME_DIR", "/tmp/xdg-test");
        assert_eq!(default_endpoint(), "/tmp/xdg-test/cgpt-bridge.sock");

        // Branch 2: XDG_RUNTIME_DIR unset → fall back to TMPDIR + uid.
        std::env::remove_var("XDG_RUNTIME_DIR");
        std::env::set_var("TMPDIR", "/var/folders/test");
        let s = default_endpoint();
        assert!(s.starts_with("/var/folders/test/cgpt-bridge."));
        assert!(s.ends_with(".sock"));
        assert_eq!(s, default_socket_path().to_string_lossy());

        // Branch 3: neither set (or set empty) → the per-user directory in
        // /tmp, never a predictable file in /tmp itself.
        let fallback = format!("/tmp/cgpt-bridge-{}/cgpt-bridge.sock", current_uid());
        std::env::remove_var("TMPDIR");
        assert_eq!(default_endpoint(), fallback);
        std::env::set_var("TMPDIR", "");
        std::env::set_var("XDG_RUNTIME_DIR", "");
        assert_eq!(default_endpoint(), fallback);

        // Restore.
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
        match prev_tmp {
            Some(v) => std::env::set_var("TMPDIR", v),
            None => std::env::remove_var("TMPDIR"),
        }
    }

    #[cfg(windows)]
    #[test]
    fn endpoint_windows_is_pipe_name_with_user() {
        let prev = std::env::var("USERNAME").ok();
        std::env::set_var("USERNAME", "Nikolai Mai");
        assert_eq!(default_endpoint(), "cgpt-bridge-Nikolai_Mai");
        match prev {
            Some(v) => std::env::set_var("USERNAME", v),
            None => std::env::remove_var("USERNAME"),
        }
    }

    #[test]
    fn pipe_name_keeps_safe_chars_and_replaces_the_rest() {
        assert_eq!(pipe_name_for_user("anna_k-2"), "cgpt-bridge-anna_k-2");
        assert_eq!(
            pipe_name_for_user("DOM\\anna k.b"),
            "cgpt-bridge-DOM_anna_k_b"
        );
        assert_eq!(pipe_name_for_user("Иван"), "cgpt-bridge-____");
        assert_eq!(pipe_name_for_user(""), "cgpt-bridge-user");
    }

    #[test]
    fn listen_connect_roundtrip_frame() {
        let endpoint = test_endpoint("rt");
        let listener = listen(&endpoint).expect("listen");

        let server = thread::spawn(move || {
            let stream = accept(&listener).expect("accept");
            let request = read_frame(&mut BufReader::new(&stream)).expect("read request");
            assert_eq!(request, br#"{"type":"ping"}"#);
            let mut writer = BufWriter::new(&stream);
            write_frame(&mut writer, br#"{"type":"pong"}"#).expect("write reply");
            writer.flush().unwrap();
            // Keep the listener (and on Unix its socket file) alive until the
            // client has read the reply.
            listener
        });

        let stream = connect(&endpoint, 3_000).expect("connect");
        let reply =
            round_trip(stream, br#"{"type":"ping"}"#, Duration::from_secs(5)).expect("round trip");
        assert_eq!(reply, br#"{"type":"pong"}"#);

        drop(join_within(server, "server accepts and replies"));
        release(&endpoint);
    }

    #[test]
    fn round_trip_times_out_on_a_silent_server() {
        let endpoint = test_endpoint("to");
        let listener = listen(&endpoint).expect("listen");
        let server = thread::spawn(move || {
            // Accept and then say nothing until the client is gone.
            let stream = accept(&listener).expect("accept");
            let _ = read_frame(&mut BufReader::new(&stream));
            let _ = read_frame(&mut BufReader::new(&stream));
            listener
        });

        let stream = connect(&endpoint, 3_000).expect("connect");
        let started = std::time::Instant::now();
        let err = round_trip(stream, b"{}", Duration::from_millis(200)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(3));
        match err {
            ReadFrameError::Io(e) => assert_eq!(e.kind(), io::ErrorKind::TimedOut),
            other => panic!("expected a timeout, got {:?}", other),
        }

        drop(server); // the helper thread ends when the process does
        release(&endpoint);
    }

    #[test]
    fn second_listener_refused_while_first_alive() {
        let endpoint = test_endpoint("two");
        let first = listen(&endpoint).expect("first listener");
        // The refused attempt probes the endpoint with a connection of its
        // own. Whether that probe reaches `accept` is up to the platform
        // (see the module docs), so the server skips anything that sends no
        // frame and answers the first real request.
        let server = thread::spawn(move || loop {
            let stream = accept(&first).expect("accept");
            if let Ok(request) = read_frame(&mut BufReader::new(&stream)) {
                assert_eq!(request, br#"{"type":"ping"}"#);
                let mut writer = BufWriter::new(&stream);
                write_frame(&mut writer, br#"{"type":"pong"}"#).expect("write reply");
                writer.flush().unwrap();
                flush_to_peer(&stream).expect("reply read");
                return first;
            }
        });

        let err = listen(&endpoint).expect_err("second listener must fail");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);

        // The refused attempt must not have damaged the first listener.
        let stream = connect(&endpoint, 3_000).expect("first still reachable");
        let reply =
            round_trip(stream, br#"{"type":"ping"}"#, Duration::from_secs(5)).expect("round trip");
        assert_eq!(reply, br#"{"type":"pong"}"#);
        drop(join_within(
            server,
            "first listener serves after the refusal",
        ));
        release(&endpoint);
    }

    /// interprocess's own local-socket connect waits for a busy pipe without
    /// a bound; ours must give up after its timeout.
    #[cfg(windows)]
    #[test]
    fn connect_to_busy_pipe_times_out() {
        let endpoint = test_endpoint("busy");
        // No accept: the listener's only pipe instance stays taken by the
        // first client, so the pipe is busy for the second.
        let listener = listen(&endpoint).expect("listen");
        let _first = connect(&endpoint, 3_000).expect("first client");
        let ep = endpoint.clone();
        let second = thread::spawn(move || connect(&ep, 300).map(drop));
        let err = join_within(second, "connect to a busy pipe")
            .err()
            .expect("second client must not connect");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        drop(listener);
    }

    #[cfg(unix)]
    #[test]
    fn stale_unix_socket_file_replaced() {
        let endpoint = test_endpoint("stale");
        let _ = std::fs::remove_file(&endpoint);
        // A std listener leaves its socket file behind when dropped: exactly
        // what a crashed host leaves.
        drop(std::os::unix::net::UnixListener::bind(&endpoint).expect("bind stale"));
        assert!(std::path::Path::new(&endpoint).exists());

        let listener = listen(&endpoint).expect("stale file must be replaced");
        assert_eq!(
            mode_of(&endpoint),
            0o600,
            "replacement socket is owner-only"
        );
        let stream = connect(&endpoint, 3_000).expect("connect to the new listener");
        drop(stream);
        drop(listener);
        release(&endpoint);
    }

    #[cfg(unix)]
    fn mode_of(path: &str) -> u32 {
        std::fs::symlink_metadata(path)
            .expect("stat")
            .permissions()
            .mode()
            & 0o777
    }

    /// `mode_t` is 16 bits on the BSDs (macOS included), 32 on Linux.
    #[cfg(unix)]
    fn set_umask(mask: u32) -> u32 {
        #[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
        type ModeT = u16;
        #[cfg(not(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd")))]
        type ModeT = u32;
        extern "C" {
            fn umask(mask: ModeT) -> ModeT;
        }
        unsafe { umask(mask as ModeT) as u32 }
    }

    /// The only test that touches the process umask. Others running in
    /// parallel may create files meanwhile; none of them asserts a mode that
    /// depends on the umask.
    #[cfg(unix)]
    #[test]
    fn socket_is_owner_only_even_under_umask_000() {
        let endpoint = test_endpoint("mode");
        let _ = std::fs::remove_file(&endpoint);
        let prev = set_umask(0o000);
        let listener = listen(&endpoint);
        set_umask(prev);
        let listener = listener.expect("listen");
        assert_eq!(mode_of(&endpoint), 0o600);
        drop(listener);
        release(&endpoint);
    }

    #[cfg(unix)]
    #[test]
    fn owner_check_accepts_own_socket_and_refuses_the_rest() {
        let endpoint = test_endpoint("own");
        let _ = std::fs::remove_file(&endpoint);
        let _listener = std::os::unix::net::UnixListener::bind(&endpoint).expect("bind");
        let path = std::path::Path::new(&endpoint);
        let meta = std::fs::symlink_metadata(path).expect("stat socket");
        let me = current_uid();

        check_own_socket(path, &meta, me).expect("own socket passes");

        // The same socket seen by another uid: what a socket planted by
        // another local user looks like to us.
        let err = check_own_socket(path, &meta, me.wrapping_add(1)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("owned by uid"), "{}", err);

        // Not a socket at all.
        let file = test_endpoint("own-file");
        std::fs::write(&file, b"x").unwrap();
        let file_meta = std::fs::symlink_metadata(&file).unwrap();
        let err = check_own_socket(std::path::Path::new(&file), &file_meta, me).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("not a socket"), "{}", err);

        std::fs::remove_file(&file).unwrap();
        std::fs::remove_file(&endpoint).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn connect_and_listen_refuse_a_symlink_to_a_live_socket() {
        let target = test_endpoint("lnk-t");
        let link = test_endpoint("lnk");
        let _ = std::fs::remove_file(&link);
        let listener = listen(&target).expect("listen on target");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let err = connect(&link, 1_000).expect_err("symlink must be refused");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        let err = listen(&link).expect_err("listen must not replace a symlink");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());

        // The real socket behind it is untouched.
        drop(connect(&target, 1_000).expect("target still reachable"));
        std::fs::remove_file(&link).unwrap();
        drop(listener);
        release(&target);
    }

    #[cfg(unix)]
    #[test]
    fn listen_leaves_a_non_socket_file_alone() {
        let endpoint = test_endpoint("file");
        std::fs::write(&endpoint, b"not a socket").unwrap();

        let err = listen(&endpoint).expect_err("must not take over a file");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read(&endpoint).unwrap(), b"not a socket");

        std::fs::remove_file(&endpoint).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn private_dir_is_created_0700_and_checked() {
        let dir = test_dir("dir");
        let _ = std::fs::remove_dir(&dir);
        let path = std::path::Path::new(&dir);
        let me = current_uid();

        ensure_private_dir(path, me).expect("create");
        assert_eq!(mode_of(&dir), 0o700);
        ensure_private_dir(path, me).expect("existing own 0700 dir passes");

        // Someone else's directory.
        let meta = std::fs::symlink_metadata(path).unwrap();
        let err = check_private_dir(path, &meta, me.wrapping_add(1)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("owned by uid"), "{}", err);

        // Our directory, but open to others.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = ensure_private_dir(path, me).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("0755"), "{}", err);

        std::fs::remove_dir(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn private_dir_refuses_a_symlink() {
        let real = test_dir("dir-real");
        let link = test_dir("dir-link");
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir(&real);
        ensure_private_dir(std::path::Path::new(&real), current_uid()).expect("real dir");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let err = ensure_private_dir(std::path::Path::new(&link), current_uid()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        std::fs::remove_file(&link).unwrap();
        std::fs::remove_dir(&real).unwrap();
    }

    /// The wiring behind the `/tmp` fallback, on a test directory instead of
    /// the real `/tmp/cgpt-bridge-<uid>/`.
    #[cfg(unix)]
    #[test]
    fn listen_creates_the_private_dir_0700() {
        let dir = test_dir("pdir");
        let _ = std::fs::remove_dir_all(&dir);
        let endpoint = format!("{}/cgpt-bridge.sock", dir);
        let private = std::path::Path::new(&dir);

        let listener = listen_with(&endpoint, Some(private)).expect("listen creates the dir");
        assert_eq!(mode_of(&dir), 0o700);
        assert_eq!(mode_of(&endpoint), 0o600);
        drop(connect_with(&endpoint, 1_000, Some(private)).expect("connect"));

        drop(listener);
        release(&endpoint);
        std::fs::remove_dir(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn connect_checks_the_private_dir() {
        let dir = test_dir("cdir");
        let _ = std::fs::remove_dir_all(&dir);
        let endpoint = format!("{}/cgpt-bridge.sock", dir);
        let private = std::path::Path::new(&dir);

        // No directory yet: still "host not running".
        let err = connect_with(&endpoint, 200, Some(private)).expect_err("nothing there");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);

        let listener = listen_with(&endpoint, Some(private)).expect("listen");
        drop(connect_with(&endpoint, 1_000, Some(private)).expect("private dir passes"));

        // Group-writable: someone else may have swapped the socket in.
        std::fs::set_permissions(private, std::fs::Permissions::from_mode(0o730)).unwrap();
        let err = connect_with(&endpoint, 1_000, Some(private)).expect_err("loose dir refused");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("0730"), "{}", err);

        // Only the private dir itself is checked, never the parent of any
        // other endpoint.
        drop(connect_with(&endpoint, 1_000, None).expect("no private dir, no dir check"));

        std::fs::set_permissions(private, std::fs::Permissions::from_mode(0o700)).unwrap();
        drop(listener);
        release(&endpoint);
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn connect_to_missing_endpoint_is_not_found_or_refused() {
        let err = connect(&test_endpoint("missing"), 200)
            .expect_err("must fail");
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ),
            "unexpected kind {:?}",
            err.kind()
        );
    }
}
