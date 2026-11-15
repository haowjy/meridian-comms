use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(fallback),
    }
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("meridian-comms")
}

pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("meridian-comms")
}

/// `/run/user/<uid>/meridian-comms.sock` where that directory exists, else a
/// socket under the state directory, which the daemon makes 0700. Derived from
/// the uid, never from `XDG_RUNTIME_DIR`: a session started without that
/// variable must find the daemon's socket.
pub fn socket_path() -> PathBuf {
    let run = PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() }));
    if run.is_dir() {
        run.join("meridian-comms.sock")
    } else {
        state_dir().join("run").join("meridian-comms.sock")
    }
}
