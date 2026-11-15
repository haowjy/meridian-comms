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

/// `$XDG_RUNTIME_DIR/meridian-comms.sock`; without that variable, a socket in a
/// per-user directory under the state directory, which the daemon makes 0700.
pub fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v).join("meridian-comms.sock"),
        _ => state_dir().join("run").join("meridian-comms.sock"),
    }
}
