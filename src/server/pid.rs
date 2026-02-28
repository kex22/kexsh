use std::path::PathBuf;

use crate::error::{KexshError, Result};
use crate::ipc;

pub fn pid_path() -> PathBuf {
    ipc::socket_dir().join("kexsh.pid")
}

pub fn write_pid() -> Result<()> {
    ipc::ensure_socket_dir()?;
    std::fs::write(pid_path(), std::process::id().to_string())?;
    Ok(())
}

pub fn read_pid() -> Result<Option<u32>> {
    let path = pid_path();
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)?;
    content
        .trim()
        .parse::<u32>()
        .map(Some)
        .map_err(|e| KexshError::Server(format!("invalid pid file: {e}")))
}

pub fn is_server_running() -> bool {
    match read_pid() {
        Ok(Some(pid)) => {
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
        }
        _ => false,
    }
}

pub fn remove_pid() -> Result<()> {
    let path = pid_path();
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_lifecycle() {
        // Clean state
        let _ = remove_pid();
        assert_eq!(read_pid().unwrap(), None);
        assert!(!is_server_running());

        // Write and verify
        write_pid().unwrap();
        assert_eq!(read_pid().unwrap(), Some(std::process::id()));
        assert!(is_server_running());

        // Cleanup
        remove_pid().unwrap();
        assert_eq!(read_pid().unwrap(), None);
    }
}
