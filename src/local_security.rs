#[cfg(unix)]
#[path = "local_security_unix.rs"]
mod unix;
#[cfg(windows)]
#[path = "local_security_windows.rs"]
mod windows;

#[cfg(unix)]
pub(crate) use unix::{
    bind_unix_listener, validate_unix_socket_path, verify_unix_peer,
};
#[cfg(windows)]
pub(crate) use windows::{connect_named_pipe, create_named_pipe};
