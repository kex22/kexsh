use crate::error::{KexshError, Result};

pub fn daemonize() -> Result<()> {
    use nix::unistd::{ForkResult, fork, setsid};

    match unsafe { fork() }.map_err(|e| KexshError::Server(format!("fork failed: {e}")))? {
        ForkResult::Parent { .. } => std::process::exit(0),
        ForkResult::Child => {}
    }

    setsid().map_err(|e| KexshError::Server(format!("setsid failed: {e}")))?;

    // Redirect stdio to /dev/null using libc
    unsafe {
        let fd = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if fd >= 0 {
            libc::dup2(fd, 0);
            libc::dup2(fd, 1);
            libc::dup2(fd, 2);
            if fd > 2 {
                libc::close(fd);
            }
        }
    }

    Ok(())
}
