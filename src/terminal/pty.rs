use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::error::{KexshError, Result};

pub struct Pty {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

#[derive(Clone)]
pub struct PtyResizer {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
}

impl PtyResizer {
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let master = self
            .master
            .lock()
            .map_err(|_| KexshError::Server("pty mutex poisoned".into()))?;
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| KexshError::Server(format!("resize: {e}")))
    }
}

impl Pty {
    pub fn spawn() -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| KexshError::Server(format!("openpty: {e}")))?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let cmd = CommandBuilder::new(shell);
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| KexshError::Server(format!("spawn: {e}")))?;

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| KexshError::Server(format!("take writer: {e}")))?;

        Ok(Pty {
            master: Arc::new(Mutex::new(pair.master)),
            writer: Arc::new(Mutex::new(writer)),
            child,
        })
    }

    pub fn clone_reader(&self) -> Result<Box<dyn Read + Send>> {
        let master = self
            .master
            .lock()
            .map_err(|_| KexshError::Server("pty mutex poisoned".into()))?;
        master
            .try_clone_reader()
            .map_err(|e| KexshError::Server(format!("clone reader: {e}")))
    }

    pub fn clone_writer(&self) -> Arc<Mutex<Box<dyn Write + Send>>> {
        self.writer.clone()
    }

    pub fn clone_resizer(&self) -> PtyResizer {
        PtyResizer {
            master: self.master.clone(),
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.clone_resizer().resize(cols, rows)
    }

    pub fn kill(&mut self) -> Result<()> {
        self.child
            .kill()
            .map_err(|e| KexshError::Server(format!("kill: {e}")))
    }
}
