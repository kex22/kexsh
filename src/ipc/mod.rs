pub mod client;
pub mod codec;
pub mod message;
pub mod mux;

use std::path::PathBuf;

use crate::error::Result;

pub fn socket_dir() -> PathBuf {
    let uid = nix::unistd::getuid();
    PathBuf::from(format!("/tmp/kexsh-{uid}"))
}

pub fn socket_path() -> PathBuf {
    socket_dir().join("kexsh.sock")
}

pub fn ensure_socket_dir() -> Result<()> {
    let dir = socket_dir();
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_contains_uid() {
        let uid = nix::unistd::getuid();
        let path = socket_path();
        assert!(path.to_str().unwrap().contains(&uid.to_string()));
        assert!(path.to_str().unwrap().ends_with("kexsh.sock"));
    }
}
