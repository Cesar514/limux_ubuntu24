// summary: Authorize peers that connect to Limux Unix control sockets.
// purpose: Enforce explicit socket access policy before terminal-control commands run.
// inputs: Peer credentials from SO_PEERCRED plus LIMUX_SOCKET_MODE or CMUX_SOCKET_MODE.
// returns/effects: Returns peer identity for authorized clients or explicit permission/config errors.

use std::io;
use std::mem::size_of;
use std::os::fd::AsRawFd;
use std::path::PathBuf;

/// Information about the connected peer process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerInfo {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

/// Access policy for the Limux Unix control socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketControlMode {
    /// Allow only descendant processes of the Limux server from the same user.
    LimuxOnly,
    /// Allow any connection from the same local user.
    LocalUser,
    /// Allow same-user clients only after a CMUX-compatible password handshake.
    Password,
    /// Allow any local connection that can reach the socket path.
    AllowAll,
}

impl SocketControlMode {
    fn from_env() -> io::Result<Self> {
        match std::env::var("LIMUX_SOCKET_MODE")
            .ok()
            .or_else(|| std::env::var("CMUX_SOCKET_MODE").ok())
        {
            Some(value) => Self::parse(&value),
            None => Ok(Self::LimuxOnly),
        }
    }

    fn parse(value: &str) -> io::Result<Self> {
        match value.trim() {
            "allowAll" | "allow-all" | "allow_all" => Ok(Self::AllowAll),
            "localUser" | "local-user" | "local_user" => Ok(Self::LocalUser),
            "password" | "passwordMode" | "password-mode" | "password_mode" => Ok(Self::Password),
            "cmuxOnly" | "limuxOnly" | "descendantOnly" | "descendant-only" | "descendant_only" => {
                Ok(Self::LimuxOnly)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "invalid LIMUX_SOCKET_MODE/CMUX_SOCKET_MODE value {value:?}; expected limuxOnly, localUser, password, or allowAll"
                ),
            )),
        }
    }

    // purpose: Identify modes that must keep the socket path owner-only.
    // inputs: The parsed socket control mode.
    // returns/effects: Returns true when filesystem permissions should restrict socket access.
    pub fn requires_owner_only_socket(self) -> bool {
        matches!(self, Self::LimuxOnly | Self::LocalUser | Self::Password)
    }

    // purpose: Identify whether accepted peers must complete a password handshake.
    // inputs: The parsed socket control mode.
    // returns/effects: Returns true only for CMUX-compatible password mode.
    pub fn requires_password(self) -> bool {
        matches!(self, Self::Password)
    }
}

// purpose: Resolve socket control mode from the process environment.
// inputs: Reads `LIMUX_SOCKET_MODE` first, then `CMUX_SOCKET_MODE`.
// returns/effects: Returns the parsed mode or an explicit invalid-mode error.
pub fn socket_control_mode_from_env() -> io::Result<SocketControlMode> {
    SocketControlMode::from_env()
}

// purpose: Validate a Unix socket peer against the configured control mode.
// inputs: A connected Unix stream plus the resolved socket control mode.
// returns/effects: Returns peer credentials or a permission/configuration error.
pub fn authorize_peer<S: AsRawFd>(stream: &S, mode: SocketControlMode) -> io::Result<PeerInfo> {
    let peer = peer_info(stream)?;
    if is_authorized(&peer, mode) {
        Ok(peer)
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("unauthorized peer uid={} pid={}", peer.uid, peer.pid),
        ))
    }
}

// purpose: Evaluate already-read peer credentials against a socket control mode.
// inputs: Peer pid/uid/gid plus the resolved socket control mode.
// returns/effects: Returns whether the peer passes the Unix credential gate.
pub fn is_authorized(peer: &PeerInfo, mode: SocketControlMode) -> bool {
    match mode {
        SocketControlMode::AllowAll => true,
        SocketControlMode::LimuxOnly => peer.uid == current_uid() && is_descendant(peer.pid),
        SocketControlMode::LocalUser | SocketControlMode::Password => peer.uid == current_uid(),
    }
}

// purpose: Resolve Limux's CMUX-compatible socket password file path.
// inputs: `XDG_STATE_HOME` or `HOME` from the process environment.
// returns/effects: Returns the state-file path or a missing-home error.
pub fn socket_password_file_path() -> io::Result<PathBuf> {
    if let Some(state_home) = std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(state_home).join("limux/socket-control-password"));
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "HOME or XDG_STATE_HOME is required for socket password storage",
            )
        })?;
    Ok(PathBuf::from(home).join(".local/state/limux/socket-control-password"))
}

// purpose: Resolve the password a CLI should send using CMUX precedence.
// inputs: Optional explicit password, `CMUX_SOCKET_PASSWORD`, and the password file.
// returns/effects: Returns a normalized password, no password, or a file read error.
pub fn socket_password_from_env_or_file(explicit: Option<&str>) -> io::Result<Option<String>> {
    if let Some(value) = normalize_password(explicit) {
        return Ok(Some(value));
    }
    if let Some(value) = normalize_password(std::env::var("CMUX_SOCKET_PASSWORD").ok().as_deref()) {
        return Ok(Some(value));
    }
    let path = match socket_password_file_path() {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    match std::fs::read_to_string(path) {
        Ok(value) => Ok(normalize_password(Some(&value))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

// purpose: Load the password required by password socket mode.
// inputs: `CMUX_SOCKET_PASSWORD` and the Limux socket password file.
// returns/effects: Returns the configured password or a fatal configuration error.
pub fn configured_socket_password() -> io::Result<String> {
    socket_password_from_env_or_file(None)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "password socket mode requires CMUX_SOCKET_PASSWORD or socket-control-password file",
        )
    })
}

// purpose: Compare a provided socket password with the configured password.
// inputs: Raw caller-provided password and expected configured password.
// returns/effects: Returns true only for a normalized exact match.
pub fn password_matches(provided: &str, expected: &str) -> bool {
    normalize_password(Some(provided)).is_some_and(|value| value == expected)
}

fn normalize_password(value: Option<&str>) -> Option<String> {
    value
        .map(|value| value.trim_matches(['\n', '\r']))
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn peer_info<S: AsRawFd>(stream: &S) -> io::Result<PeerInfo> {
    let fd = stream.as_raw_fd();
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut cred_len = size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut cred_len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if cred_len != size_of::<libc::ucred>() as libc::socklen_t {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected peer credential size",
        ));
    }

    Ok(PeerInfo {
        pid: u32::try_from(cred.pid).unwrap_or(0),
        uid: cred.uid,
        gid: cred.gid,
    })
}

fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

fn is_descendant(pid: u32) -> bool {
    let ancestor_pid = std::process::id();
    if pid == 0 {
        return false;
    }

    let mut current = pid;
    for _ in 0..64 {
        if current == ancestor_pid {
            return true;
        }
        if current <= 1 {
            return false;
        }
        match read_ppid(current) {
            Some(parent) if parent != current => current = parent,
            _ => return false,
        }
    }

    false
}

fn read_ppid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::Mutex;

    static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let old = std::env::var_os(key);
            match value {
                Some(value) => unsafe { std::env::set_var(key, value) },
                None => unsafe { std::env::remove_var(key) },
            }
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn socket_mode_defaults_to_limux_only() {
        let _lock = ENV_TEST_LOCK.lock().expect("env lock");
        let _limux = EnvGuard::set("LIMUX_SOCKET_MODE", None);
        let _cmux = EnvGuard::set("CMUX_SOCKET_MODE", None);
        assert_eq!(
            socket_control_mode_from_env().unwrap(),
            SocketControlMode::LimuxOnly
        );
    }

    #[test]
    fn descendant_aliases_map_to_limux_only() {
        let _lock = ENV_TEST_LOCK.lock().expect("env lock");
        let _limux = EnvGuard::set("LIMUX_SOCKET_MODE", Some("cmuxOnly"));
        let _cmux = EnvGuard::set("CMUX_SOCKET_MODE", None);
        assert_eq!(
            socket_control_mode_from_env().unwrap(),
            SocketControlMode::LimuxOnly
        );
    }

    #[test]
    fn invalid_socket_mode_is_rejected() {
        let _lock = ENV_TEST_LOCK.lock().expect("env lock");
        let _limux = EnvGuard::set("LIMUX_SOCKET_MODE", Some("public-ish"));
        let _cmux = EnvGuard::set("CMUX_SOCKET_MODE", None);
        let error = socket_control_mode_from_env().expect_err("invalid mode must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn password_socket_mode_is_supported_and_owner_only() {
        let _lock = ENV_TEST_LOCK.lock().expect("env lock");
        let _limux = EnvGuard::set("LIMUX_SOCKET_MODE", Some("password"));
        let _cmux = EnvGuard::set("CMUX_SOCKET_MODE", None);
        let mode = socket_control_mode_from_env().unwrap();
        assert_eq!(mode, SocketControlMode::Password);
        assert!(mode.requires_owner_only_socket());
        assert!(mode.requires_password());
    }

    #[test]
    fn allow_all_accepts_any_uid() {
        let peer = PeerInfo {
            pid: 42,
            uid: current_uid().saturating_add(1),
            gid: 7,
        };
        assert!(is_authorized(&peer, SocketControlMode::AllowAll));
    }

    #[test]
    fn password_mode_still_requires_same_uid_before_handshake() {
        let peer = PeerInfo {
            pid: std::process::id(),
            uid: current_uid().saturating_add(1),
            gid: 0,
        };

        assert!(!is_authorized(&peer, SocketControlMode::Password));
    }

    #[test]
    fn socket_password_resolver_prefers_explicit_then_env_then_file() {
        let _lock = ENV_TEST_LOCK.lock().expect("env lock");
        let dir = tempfile::tempdir().expect("tempdir");
        let state_home = dir.path().join("state");
        let password_dir = state_home.join("limux");
        std::fs::create_dir_all(&password_dir).expect("password dir");
        std::fs::write(
            password_dir.join("socket-control-password"),
            "file-secret\n",
        )
        .expect("password file");
        let _state = EnvGuard::set("XDG_STATE_HOME", state_home.to_str());
        let _env = EnvGuard::set("CMUX_SOCKET_PASSWORD", Some("env-secret"));

        assert_eq!(
            socket_password_from_env_or_file(Some("explicit-secret\n"))
                .expect("explicit")
                .as_deref(),
            Some("explicit-secret")
        );
        assert_eq!(
            socket_password_from_env_or_file(None)
                .expect("env")
                .as_deref(),
            Some("env-secret")
        );
        drop(_env);
        assert_eq!(
            socket_password_from_env_or_file(None)
                .expect("file")
                .as_deref(),
            Some("file-secret")
        );
    }

    #[test]
    fn limux_only_allows_current_process() {
        let peer = PeerInfo {
            pid: std::process::id(),
            uid: current_uid(),
            gid: unsafe { libc::getgid() },
        };
        assert!(is_authorized(&peer, SocketControlMode::LimuxOnly));
    }

    #[test]
    fn limux_only_rejects_non_descendant_pid() {
        let peer = PeerInfo {
            pid: 1,
            uid: current_uid(),
            gid: unsafe { libc::getgid() },
        };
        assert!(!is_authorized(&peer, SocketControlMode::LimuxOnly));
    }

    #[test]
    fn authorize_peer_reads_same_user_credentials() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let socket_path = temp_dir.path().join("auth.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind listener");
        let client = UnixStream::connect(&socket_path).expect("connect client");
        let (server, _) = listener.accept().expect("accept client");

        let peer = authorize_peer(&server, SocketControlMode::LocalUser).expect("authorize");

        assert_eq!(peer.uid, current_uid());
        assert_eq!(peer.gid, unsafe { libc::getgid() });

        drop(client);
    }
}
