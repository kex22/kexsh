pub mod daemon;
pub mod pid;
pub mod state;

use std::collections::{HashMap, HashSet};
use std::io::{Read as _, Write as _};
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::cloud::manager::{CloudCommand, CloudManager};
use crate::error::{KexshError, Result};
use crate::ipc;
use crate::ipc::codec::{read_binary_frame, read_message, write_binary_frame, write_message};
use crate::ipc::message::{BinaryFrame, MuxRequest, MuxResponse, Request, Response, ViewInfo};
use crate::server::state::StatePersister;
use crate::terminal::manager::TerminalManager;
use crate::terminal::pty::PtyResizer;
use crate::view::manager::ViewManager;

pub struct Server {
    listener: UnixListener,
    shutdown: Arc<Notify>,
    manager: Arc<Mutex<TerminalManager>>,
    view_manager: Arc<Mutex<ViewManager>>,
    persister: StatePersister,
    cloud_tx: tokio::sync::mpsc::Sender<CloudCommand>,
}

impl Server {
    pub async fn start() -> Result<()> {
        if pid::is_server_running() {
            return Err(KexshError::Server("server is already running".into()));
        }

        ipc::ensure_socket_dir()?;

        let sock_path = ipc::socket_path();
        if sock_path.exists() {
            std::fs::remove_file(&sock_path)?;
        }

        let listener = UnixListener::bind(&sock_path)?;
        pid::write_pid()?;

        // Restore synced terminals from previous run before cleaning stale state
        let prev_synced: HashSet<String> = state::ServerState::load()
            .synced_terminals
            .into_iter()
            .collect();

        // Clean stale state from previous run (PTYs are dead after restart)
        let _ = std::fs::remove_file(state::state_path());

        let manager = Arc::new(Mutex::new(TerminalManager::new()));
        let view_manager = Arc::new(Mutex::new(ViewManager::new()));
        let synced = Arc::new(Mutex::new(prev_synced));
        let persister =
            StatePersister::spawn(manager.clone(), view_manager.clone(), synced.clone());
        let cloud_tx = CloudManager::spawn(synced, manager.clone());

        let server = Server {
            listener,
            shutdown: Arc::new(Notify::new()),
            manager,
            view_manager,
            persister,
            cloud_tx,
        };
        server.run().await
    }

    async fn run(self) -> Result<()> {
        let shutdown = self.shutdown.clone();

        let sig_shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            let Ok(mut sigterm) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            else {
                eprintln!("failed to register SIGTERM handler");
                return;
            };
            let Ok(mut sigint) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            else {
                eprintln!("failed to register SIGINT handler");
                return;
            };
            tokio::select! {
                _ = sigterm.recv() => {}
                _ = sigint.recv() => {}
            }
            sig_shutdown.notify_one();
        });

        loop {
            tokio::select! {
                result = self.listener.accept() => {
                    let (stream, _) = result?;
                    let notify = shutdown.clone();
                    let mgr = self.manager.clone();
                    let vmgr = self.view_manager.clone();
                    let persist = self.persister.clone();
                    let cloud = self.cloud_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, notify, mgr, vmgr, persist, cloud).await {
                            eprintln!("connection error: {e}");
                        }
                    });
                }
                _ = shutdown.notified() => {
                    self.shutdown().await?;
                    return Ok(());
                }
            }
        }
    }

    async fn shutdown(self) -> Result<()> {
        // Save final state
        self.persister.notify();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Kill all terminals (sends SIGHUP to shell processes)
        let mut mgr = self.manager.lock().await;
        let ids: Vec<String> = mgr.list().into_iter().map(|t| t.id).collect();
        for id in ids {
            let _ = mgr.kill(&id);
        }
        drop(mgr);

        // Clean up socket and PID
        let _ = std::fs::remove_file(ipc::socket_path());
        pid::remove_pid()?;
        Ok(())
    }
}

async fn handle_connection(
    mut stream: UnixStream,
    shutdown: Arc<Notify>,
    manager: Arc<Mutex<TerminalManager>>,
    view_manager: Arc<Mutex<ViewManager>>,
    persister: StatePersister,
    cloud_tx: tokio::sync::mpsc::Sender<CloudCommand>,
) -> Result<()> {
    let req: Request = read_message(&mut stream).await?;
    let is_mutation = matches!(
        req,
        Request::TerminalCreate { .. }
            | Request::TerminalKill { .. }
            | Request::TerminalSync { .. }
            | Request::TerminalUnsync { .. }
            | Request::ViewCreate { .. }
            | Request::ViewDelete { .. }
            | Request::ViewAddTerminal { .. }
            | Request::ViewUpdateLayout { .. }
            | Request::ViewRemoveTerminal { .. }
            | Request::ProxyExpose { .. }
            | Request::ProxyUnexpose { .. }
    );
    let resp = match req {
        Request::ServerStop => {
            shutdown.notify_one();
            Response::Ok
        }
        Request::TerminalCreate { name } => {
            let mut mgr = manager.lock().await;
            match mgr.create(name) {
                Ok(id) => Response::TerminalCreated { id },
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }
        Request::TerminalList => {
            let mgr = manager.lock().await;
            Response::TerminalList {
                terminals: mgr.list(),
            }
        }
        Request::TerminalKill { id } => {
            let mut mgr = manager.lock().await;
            match mgr.kill(&id) {
                Ok(()) => {
                    let mut vmgr = view_manager.lock().await;
                    vmgr.remove_terminal(&id);
                    Response::Ok
                }
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }
        Request::TerminalAttach { id } => {
            return handle_attach(stream, manager, &id).await;
        }
        Request::ViewCreate { name, terminal_id } => {
            let mgr = manager.lock().await;
            if mgr.get(&terminal_id).is_none() {
                Response::Error {
                    message: format!("terminal not found: {terminal_id}"),
                }
            } else {
                drop(mgr);
                let mut vmgr = view_manager.lock().await;
                let id = vmgr.create(name, terminal_id);
                Response::ViewCreated { id }
            }
        }
        Request::ViewList => {
            let vmgr = view_manager.lock().await;
            Response::ViewList { views: vmgr.list() }
        }
        Request::ViewDelete { id } => {
            let mut vmgr = view_manager.lock().await;
            match vmgr.delete(&id) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }
        Request::ViewShow { id } => {
            let vmgr = view_manager.lock().await;
            match vmgr.get(&id) {
                Some(v) => Response::ViewShow {
                    view: ViewInfo {
                        id: v.id.clone(),
                        name: v.name.clone(),
                        terminal_ids: v.terminal_ids.clone(),
                        created_at: v.created_at.clone(),
                    },
                },
                None => Response::Error {
                    message: format!("view not found: {id}"),
                },
            }
        }
        Request::ViewAddTerminal {
            view_id,
            terminal_id,
        } => {
            let mgr = manager.lock().await;
            if mgr.get(&terminal_id).is_none() {
                Response::Error {
                    message: format!("terminal not found: {terminal_id}"),
                }
            } else {
                drop(mgr);
                let mut vmgr = view_manager.lock().await;
                match vmgr.resolve_id(&view_id) {
                    Some(resolved) => {
                        vmgr.add_terminal(&resolved, &terminal_id);
                        Response::Ok
                    }
                    None => Response::Error {
                        message: format!("view not found: {view_id}"),
                    },
                }
            }
        }
        Request::ViewAttach { id } => {
            let vmgr = view_manager.lock().await;
            match vmgr.get(&id) {
                Some(v) => Response::ViewAttach {
                    terminal_ids: v.terminal_ids.clone(),
                    layout: Some(v.layout.clone()),
                    focused: Some(v.focused.clone()),
                },
                None => Response::Error {
                    message: format!("view not found: {id}"),
                },
            }
        }
        Request::ViewUpdateLayout {
            view_id,
            layout,
            focused,
        } => {
            let mut vmgr = view_manager.lock().await;
            vmgr.update_layout(&view_id, layout, focused);
            Response::Ok
        }
        Request::ViewRemoveTerminal {
            view_id,
            terminal_id,
        } => {
            let mut vmgr = view_manager.lock().await;
            vmgr.remove_terminal_from_view(&view_id, &terminal_id);
            Response::Ok
        }
        Request::MultiplexAttach {
            terminal_ids,
            view_id,
        } => {
            return handle_multiplex_attach(
                stream,
                manager,
                view_manager,
                persister,
                terminal_ids,
                view_id,
            )
            .await;
        }
        Request::TerminalSync { id } => {
            let mgr = manager.lock().await;
            let Some(t) = mgr.get(&id) else {
                return write_message(
                    &mut stream,
                    &Response::Error {
                        message: format!("terminal not found: {id}"),
                    },
                )
                .await;
            };
            let name = t.name.clone();
            let pty_reader = match t.pty.clone_reader() {
                Ok(r) => r,
                Err(e) => {
                    return write_message(
                        &mut stream,
                        &Response::Error {
                            message: e.to_string(),
                        },
                    )
                    .await;
                }
            };
            let pty_writer = t.pty.clone_writer();
            let pty_resizer = t.pty.clone_resizer();
            drop(mgr);
            let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel(1);
            let _ = cloud_tx
                .send(CloudCommand::Sync {
                    id,
                    name,
                    pty_reader,
                    pty_writer,
                    pty_resizer,
                    reply: reply_tx,
                })
                .await;
            match reply_rx.recv().await {
                Some(Ok(())) => Response::SyncStatus { synced: true },
                Some(Err(e)) => Response::Error { message: e },
                None => Response::Error {
                    message: "cloud manager unavailable".into(),
                },
            }
        }
        Request::TerminalUnsync { id } => {
            let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel(1);
            let _ = cloud_tx
                .send(CloudCommand::Unsync {
                    id,
                    reply: reply_tx,
                })
                .await;
            match reply_rx.recv().await {
                Some(Ok(())) => Response::SyncStatus { synced: false },
                Some(Err(e)) => Response::Error { message: e },
                None => Response::Error {
                    message: "cloud manager unavailable".into(),
                },
            }
        }
        Request::ProxyExpose { port, public } => {
            let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel(1);
            let _ = cloud_tx
                .send(CloudCommand::ProxyExpose {
                    port,
                    public,
                    reply: reply_tx,
                })
                .await;
            match reply_rx.recv().await {
                Some(Ok(url)) => Response::ProxyExposed { port, url },
                Some(Err(e)) => Response::Error { message: e },
                None => Response::Error {
                    message: "cloud manager unavailable".into(),
                },
            }
        }
        Request::ProxyUnexpose { port } => {
            let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel(1);
            let _ = cloud_tx
                .send(CloudCommand::ProxyUnexpose {
                    port,
                    reply: reply_tx,
                })
                .await;
            match reply_rx.recv().await {
                Some(Ok(())) => Response::Ok,
                Some(Err(e)) => Response::Error { message: e },
                None => Response::Error {
                    message: "cloud manager unavailable".into(),
                },
            }
        }
        Request::ProxyList => {
            let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel(1);
            let _ = cloud_tx
                .send(CloudCommand::ProxyList { reply: reply_tx })
                .await;
            match reply_rx.recv().await {
                Some(ports) => Response::ProxyList { ports },
                None => Response::ProxyList { ports: vec![] },
            }
        }
    };
    let result = write_message(&mut stream, &resp).await;
    if is_mutation {
        persister.notify();
    }
    result
}

async fn handle_attach(
    mut stream: UnixStream,
    manager: Arc<Mutex<TerminalManager>>,
    id: &str,
) -> Result<()> {
    // Validate terminal exists and clone reader/writer (non-destructive)
    let (mut pty_reader, pty_writer, pty_resizer) = {
        let mgr = manager.lock().await;
        let terminal = match mgr.get(id) {
            Some(t) => t,
            None => {
                let resp = Response::Error {
                    message: format!("terminal not found: {id}"),
                };
                return write_message(&mut stream, &resp).await;
            }
        };
        let reader = terminal.pty.clone_reader()?;
        let writer = terminal.pty.clone_writer();
        let resizer = terminal.pty.clone_resizer();
        (reader, writer, resizer)
    };

    write_message(&mut stream, &Response::Ok).await?;

    let (mut sock_read, mut sock_write) = stream.into_split();

    // PTY → socket: blocking read in spawn_blocking, send via channel
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
    let pty_read_task = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) => break,
                Err(e) => {
                    eprintln!("pty read error: {e}");
                    break;
                }
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Channel → socket: forward PTY output as binary frames
    let tid_write = id.to_string();
    let sock_write_task = tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            if write_binary_frame(&mut sock_write, &tid_write, &BinaryFrame::Data(data))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Socket → PTY: read binary frames, write to PTY
    let sock_read_task = tokio::spawn(async move {
        loop {
            match read_binary_frame(&mut sock_read).await {
                Ok((_, BinaryFrame::Data(data))) => {
                    let Ok(mut w) = pty_writer.lock() else { break };
                    if w.write_all(&data).is_err() {
                        break;
                    }
                }
                Ok((_, BinaryFrame::Resize { cols, rows })) => {
                    let _ = pty_resizer.resize(cols, rows);
                }
                Ok((_, BinaryFrame::Detach | BinaryFrame::Control(_))) | Err(_) => break,
            }
        }
    });

    // Wait for any task to finish, then clean up
    tokio::select! {
        _ = pty_read_task => {}
        _ = sock_write_task => {}
        _ = sock_read_task => {}
    }

    Ok(())
}

/// Spawn a blocking pty_read task that sends data frames into the shared channel.
fn spawn_pty_reader(
    terminal_id: String,
    mut pty_reader: Box<dyn std::io::Read + Send>,
    tx: tokio::sync::mpsc::Sender<(String, BinaryFrame)>,
) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let frame = BinaryFrame::Data(buf[..n].to_vec());
                    if tx.blocking_send((terminal_id.clone(), frame)).is_err() {
                        break;
                    }
                }
            }
        }
    })
}

async fn handle_multiplex_attach(
    mut stream: UnixStream,
    manager: Arc<Mutex<TerminalManager>>,
    view_manager: Arc<Mutex<ViewManager>>,
    persister: StatePersister,
    terminal_ids: Vec<String>,
    _view_id: Option<String>,
) -> Result<()> {
    // Validate terminals and collect PTY handles
    let mut pty_writers: HashMap<String, Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>> =
        HashMap::new();
    let mut pty_resizers: HashMap<String, PtyResizer> = HashMap::new();
    let mut valid_ids = Vec::new();

    {
        let mgr = manager.lock().await;
        for id in &terminal_ids {
            if let Some(t) = mgr.get(id)
                && let Ok(reader) = t.pty.clone_reader()
            {
                pty_writers.insert(id.clone(), t.pty.clone_writer());
                pty_resizers.insert(id.clone(), t.pty.clone_resizer());
                valid_ids.push((id.clone(), reader));
            }
        }
    }

    let attached_ids: Vec<String> = valid_ids.iter().map(|(id, _)| id.clone()).collect();
    write_message(
        &mut stream,
        &Response::MultiplexAttached {
            terminal_ids: attached_ids,
        },
    )
    .await?;

    if valid_ids.is_empty() {
        return Ok(());
    }

    let (sock_read, mut sock_write) = stream.into_split();

    // Shared channel: pty_read tasks send Data frames, sock_read sends Control responses
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, BinaryFrame)>(64);

    // Spawn pty_read tasks, track by terminal_id
    let readers = Arc::new(Mutex::new(HashMap::<String, JoinHandle<()>>::new()));
    for (id, pty_reader) in valid_ids {
        let handle = spawn_pty_reader(id.clone(), pty_reader, tx.clone());
        readers.lock().await.insert(id, handle);
    }

    // Keep one tx clone for sock_read task (to spawn new readers + send control responses).
    // Drop the original so channel closes when sock_read + all readers finish.
    let tx_for_sock_read = tx.clone();
    drop(tx);

    // sock_write task: drain channel, write binary frames to socket
    let sock_write_task = tokio::spawn(async move {
        while let Some((tid, frame)) = rx.recv().await {
            if write_binary_frame(&mut sock_write, &tid, &frame)
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // sock_read task: route frames by terminal_id, handle Control frames
    let sock_read_task = tokio::spawn(mux_sock_read(
        sock_read,
        pty_writers,
        pty_resizers,
        readers.clone(),
        tx_for_sock_read,
        manager,
        view_manager,
        persister,
    ));

    tokio::select! {
        _ = sock_write_task => {}
        _ = sock_read_task => {}
    }

    // Abort remaining reader tasks on disconnect
    for (_, handle) in readers.lock().await.drain() {
        handle.abort();
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn mux_sock_read(
    mut sock_read: tokio::net::unix::OwnedReadHalf,
    mut pty_writers: HashMap<String, Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>>,
    mut pty_resizers: HashMap<String, PtyResizer>,
    readers: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
    tx: tokio::sync::mpsc::Sender<(String, BinaryFrame)>,
    manager: Arc<Mutex<TerminalManager>>,
    view_manager: Arc<Mutex<ViewManager>>,
    persister: StatePersister,
) {
    loop {
        match read_binary_frame(&mut sock_read).await {
            Ok((tid, BinaryFrame::Data(data))) => {
                if let Some(w) = pty_writers.get(&tid) {
                    let Ok(mut w) = w.lock() else { continue };
                    let _ = w.write_all(&data);
                }
            }
            Ok((tid, BinaryFrame::Resize { cols, rows })) => {
                if let Some(r) = pty_resizers.get(&tid) {
                    let _ = r.resize(cols, rows);
                }
            }
            Ok((_, BinaryFrame::Control(payload))) => {
                let req: MuxRequest = match serde_json::from_slice(&payload) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let resp = handle_mux_request(
                    req,
                    &mut pty_writers,
                    &mut pty_resizers,
                    &readers,
                    &tx,
                    &manager,
                    &view_manager,
                    &persister,
                )
                .await;
                // Send control response through the shared channel to sock_write task
                let resp_payload = match serde_json::to_vec(&resp) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                let _ = tx
                    .send((
                        "\0\0\0\0\0\0\0\0".into(),
                        BinaryFrame::Control(resp_payload),
                    ))
                    .await;
            }
            Ok((_, BinaryFrame::Detach)) | Err(_) => break,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_mux_request(
    req: MuxRequest,
    pty_writers: &mut HashMap<String, Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>>,
    pty_resizers: &mut HashMap<String, PtyResizer>,
    readers: &Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
    tx: &tokio::sync::mpsc::Sender<(String, BinaryFrame)>,
    manager: &Arc<Mutex<TerminalManager>>,
    view_manager: &Arc<Mutex<ViewManager>>,
    persister: &StatePersister,
) -> MuxResponse {
    match req {
        MuxRequest::CreateTerminal { name } => {
            let mut mgr = manager.lock().await;
            let id = match mgr.create(name) {
                Ok(id) => id,
                Err(e) => {
                    return MuxResponse::Error {
                        message: e.to_string(),
                    };
                }
            };
            // Wire up the new terminal's PTY
            if let Some(t) = mgr.get(&id) {
                let reader = match t.pty.clone_reader() {
                    Ok(r) => r,
                    Err(e) => {
                        return MuxResponse::Error {
                            message: e.to_string(),
                        };
                    }
                };
                pty_writers.insert(id.clone(), t.pty.clone_writer());
                pty_resizers.insert(id.clone(), t.pty.clone_resizer());
                drop(mgr);
                let handle = spawn_pty_reader(id.clone(), reader, tx.clone());
                if let Some(old) = readers.lock().await.insert(id.clone(), handle) {
                    old.abort();
                }
                persister.notify();
                MuxResponse::TerminalCreated { id }
            } else {
                MuxResponse::Error {
                    message: "terminal vanished after create".into(),
                }
            }
        }
        MuxRequest::AddTerminal { id } => {
            let mgr = manager.lock().await;
            let Some(t) = mgr.get(&id) else {
                return MuxResponse::Error {
                    message: format!("terminal not found: {id}"),
                };
            };
            let reader = match t.pty.clone_reader() {
                Ok(r) => r,
                Err(e) => {
                    return MuxResponse::Error {
                        message: e.to_string(),
                    };
                }
            };
            pty_writers.insert(id.clone(), t.pty.clone_writer());
            pty_resizers.insert(id.clone(), t.pty.clone_resizer());
            drop(mgr);
            let handle = spawn_pty_reader(id.clone(), reader, tx.clone());
            if let Some(old) = readers.lock().await.insert(id.clone(), handle) {
                old.abort();
            }
            MuxResponse::Ok
        }
        MuxRequest::RemoveTerminal { id } => {
            pty_writers.remove(&id);
            pty_resizers.remove(&id);
            if let Some(handle) = readers.lock().await.remove(&id) {
                handle.abort();
            }
            MuxResponse::Ok
        }
        MuxRequest::KillTerminal { id } => {
            // Remove from mux first
            pty_writers.remove(&id);
            pty_resizers.remove(&id);
            if let Some(handle) = readers.lock().await.remove(&id) {
                handle.abort();
            }
            // Kill the terminal process
            let mut mgr = manager.lock().await;
            if let Err(e) = mgr.kill(&id) {
                return MuxResponse::Error {
                    message: e.to_string(),
                };
            }
            drop(mgr);
            let mut vmgr = view_manager.lock().await;
            vmgr.remove_terminal(&id);
            drop(vmgr);
            persister.notify();
            MuxResponse::Ok
        }
        MuxRequest::UpdateLayout {
            view_id,
            layout,
            focused,
        } => {
            let mut vmgr = view_manager.lock().await;
            vmgr.update_layout(&view_id, layout, focused);
            drop(vmgr);
            persister.notify();
            MuxResponse::Ok
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixStream;

    async fn paired_streams() -> (UnixStream, UnixStream) {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let client = UnixStream::connect(&sock).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        (client, server)
    }

    fn test_managers() -> (
        Arc<Mutex<TerminalManager>>,
        Arc<Mutex<ViewManager>>,
        StatePersister,
    ) {
        let mgr = Arc::new(Mutex::new(TerminalManager::new()));
        let vmgr = Arc::new(Mutex::new(ViewManager::new()));
        let synced = Arc::new(Mutex::new(HashSet::new()));
        let persister = StatePersister::spawn(mgr.clone(), vmgr.clone(), synced);
        (mgr, vmgr, persister)
    }

    #[tokio::test]
    async fn multiplex_attach_filters_invalid_terminals() {
        let (client, server) = paired_streams().await;
        let (mgr, vmgr, persister) = test_managers();

        // Create one real terminal
        let real_id = mgr.lock().await.create(None).unwrap();

        let handle = tokio::spawn(handle_multiplex_attach(
            server,
            mgr,
            vmgr,
            persister,
            vec![real_id.clone(), "nonexist".into()],
            None,
        ));

        let (mut client_read, mut client_write) = client.into_split();
        let resp: Response = read_message(&mut client_read).await.unwrap();

        // Should only contain the valid terminal
        match resp {
            Response::MultiplexAttached { terminal_ids } => {
                assert_eq!(terminal_ids, vec![real_id.clone()]);
            }
            other => panic!("expected MultiplexAttached, got {other:?}"),
        }

        // Send Detach to cleanly close
        write_binary_frame(&mut client_write, &real_id, &BinaryFrame::Detach)
            .await
            .unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn multiplex_attach_empty_returns_immediately() {
        let (client, server) = paired_streams().await;
        let (mgr, vmgr, persister) = test_managers();

        let handle = tokio::spawn(handle_multiplex_attach(
            server,
            mgr,
            vmgr,
            persister,
            vec!["nonexist".into()],
            None,
        ));

        let mut client_read = client;
        let resp: Response = read_message(&mut client_read).await.unwrap();
        match resp {
            Response::MultiplexAttached { terminal_ids } => {
                assert!(terminal_ids.is_empty());
            }
            other => panic!("expected MultiplexAttached, got {other:?}"),
        }

        // Server should return immediately since no valid terminals
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn multiplex_attach_receives_pty_output() {
        let (client, server) = paired_streams().await;
        let (mgr, vmgr, persister) = test_managers();

        let tid = mgr.lock().await.create(None).unwrap();

        let handle = tokio::spawn(handle_multiplex_attach(
            server,
            mgr.clone(),
            vmgr,
            persister,
            vec![tid.clone()],
            None,
        ));

        let (mut client_read, mut client_write) = client.into_split();
        let _resp: Response = read_message(&mut client_read).await.unwrap();

        // The shell should produce some output (prompt, etc.)
        // Read at least one data frame within a timeout
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            read_binary_frame(&mut client_read),
        )
        .await;

        assert!(result.is_ok(), "should receive PTY output");
        let (frame_tid, frame) = result.unwrap().unwrap();
        assert_eq!(frame_tid, tid);
        assert!(matches!(frame, BinaryFrame::Data(_)));

        // Clean up
        write_binary_frame(&mut client_write, &tid, &BinaryFrame::Detach)
            .await
            .unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn multiplex_control_create_terminal() {
        let (client, server) = paired_streams().await;
        let (mgr, vmgr, persister) = test_managers();

        let tid = mgr.lock().await.create(None).unwrap();

        let handle = tokio::spawn(handle_multiplex_attach(
            server,
            mgr.clone(),
            vmgr,
            persister,
            vec![tid.clone()],
            None,
        ));

        let (mut client_read, mut client_write) = client.into_split();
        let _resp: Response = read_message(&mut client_read).await.unwrap();

        // Send CreateTerminal control frame
        let req = MuxRequest::CreateTerminal {
            name: Some("new-term".into()),
        };
        let payload = serde_json::to_vec(&req).unwrap();
        write_binary_frame(
            &mut client_write,
            "\0\0\0\0\0\0\0\0",
            &BinaryFrame::Control(payload),
        )
        .await
        .unwrap();

        // Read frames until we get a Control response (skip Data frames from PTY)
        let resp = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let (_, frame) = read_binary_frame(&mut client_read).await.unwrap();
                if let BinaryFrame::Control(payload) = frame {
                    return serde_json::from_slice::<MuxResponse>(&payload).unwrap();
                }
            }
        })
        .await
        .expect("should receive control response");

        match resp {
            MuxResponse::TerminalCreated { id } => {
                // Verify the terminal actually exists in the manager
                assert!(mgr.lock().await.get(&id).is_some());
            }
            other => panic!("expected TerminalCreated, got {other:?}"),
        }

        // Clean up
        write_binary_frame(&mut client_write, &tid, &BinaryFrame::Detach)
            .await
            .unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn multiplex_control_remove_terminal() {
        let (client, server) = paired_streams().await;
        let (mgr, vmgr, persister) = test_managers();

        let t1 = mgr.lock().await.create(None).unwrap();
        let t2 = mgr.lock().await.create(None).unwrap();

        let handle = tokio::spawn(handle_multiplex_attach(
            server,
            mgr,
            vmgr,
            persister,
            vec![t1.clone(), t2.clone()],
            None,
        ));

        let (mut client_read, mut client_write) = client.into_split();
        let _resp: Response = read_message(&mut client_read).await.unwrap();

        // Remove t2 from the mux
        let req = MuxRequest::RemoveTerminal { id: t2.clone() };
        let payload = serde_json::to_vec(&req).unwrap();
        write_binary_frame(
            &mut client_write,
            "\0\0\0\0\0\0\0\0",
            &BinaryFrame::Control(payload),
        )
        .await
        .unwrap();

        // Read until we get the Control response
        let resp = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let (_, frame) = read_binary_frame(&mut client_read).await.unwrap();
                if let BinaryFrame::Control(payload) = frame {
                    return serde_json::from_slice::<MuxResponse>(&payload).unwrap();
                }
            }
        })
        .await
        .expect("should receive control response");

        assert!(matches!(resp, MuxResponse::Ok));

        // Clean up
        write_binary_frame(&mut client_write, &t1, &BinaryFrame::Detach)
            .await
            .unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn multiplex_control_kill_terminal() {
        let (client, server) = paired_streams().await;
        let (mgr, vmgr, persister) = test_managers();

        let t1 = mgr.lock().await.create(None).unwrap();
        let t2 = mgr.lock().await.create(None).unwrap();

        let handle = tokio::spawn(handle_multiplex_attach(
            server,
            mgr.clone(),
            vmgr,
            persister,
            vec![t1.clone(), t2.clone()],
            None,
        ));

        let (mut client_read, mut client_write) = client.into_split();
        let _resp: Response = read_message(&mut client_read).await.unwrap();

        // Kill t2
        let req = MuxRequest::KillTerminal { id: t2.clone() };
        let payload = serde_json::to_vec(&req).unwrap();
        write_binary_frame(
            &mut client_write,
            "\0\0\0\0\0\0\0\0",
            &BinaryFrame::Control(payload),
        )
        .await
        .unwrap();

        let resp = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let (_, frame) = read_binary_frame(&mut client_read).await.unwrap();
                if let BinaryFrame::Control(payload) = frame {
                    return serde_json::from_slice::<MuxResponse>(&payload).unwrap();
                }
            }
        })
        .await
        .expect("should receive control response");

        assert!(matches!(resp, MuxResponse::Ok));
        // Terminal should be gone from manager
        assert!(mgr.lock().await.get(&t2).is_none());

        write_binary_frame(&mut client_write, &t1, &BinaryFrame::Detach)
            .await
            .unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn multiplex_control_update_layout() {
        let (client, server) = paired_streams().await;
        let (mgr, vmgr, persister) = test_managers();

        let tid = mgr.lock().await.create(None).unwrap();
        let view_id = vmgr.lock().await.create(None, tid.clone());

        let handle = tokio::spawn(handle_multiplex_attach(
            server,
            mgr,
            vmgr.clone(),
            persister,
            vec![tid.clone()],
            None,
        ));

        let (mut client_read, mut client_write) = client.into_split();
        let _resp: Response = read_message(&mut client_read).await.unwrap();

        let req = MuxRequest::UpdateLayout {
            view_id: view_id.clone(),
            layout: serde_json::json!({"type": "leaf", "terminal_id": &tid}),
            focused: tid.clone(),
        };
        let payload = serde_json::to_vec(&req).unwrap();
        write_binary_frame(
            &mut client_write,
            "\0\0\0\0\0\0\0\0",
            &BinaryFrame::Control(payload),
        )
        .await
        .unwrap();

        let resp = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let (_, frame) = read_binary_frame(&mut client_read).await.unwrap();
                if let BinaryFrame::Control(payload) = frame {
                    return serde_json::from_slice::<MuxResponse>(&payload).unwrap();
                }
            }
        })
        .await
        .expect("should receive control response");

        assert!(matches!(resp, MuxResponse::Ok));
        // Verify layout was persisted
        let v = vmgr.lock().await;
        let view = v.get(&view_id).unwrap();
        assert_eq!(view.focused, tid);

        drop(v);
        write_binary_frame(&mut client_write, &tid, &BinaryFrame::Detach)
            .await
            .unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn sync_nonexistent_terminal_returns_error() {
        let (mut client, server) = paired_streams().await;
        let (mgr, vmgr, persister) = test_managers();
        let synced = Arc::new(Mutex::new(HashSet::new()));
        let cloud_tx = CloudManager::spawn(synced, mgr.clone());

        let handle = tokio::spawn(handle_connection(
            server,
            Arc::new(Notify::new()),
            mgr,
            vmgr,
            persister,
            cloud_tx,
        ));

        write_message(&mut client, &Request::TerminalSync { id: "nope".into() })
            .await
            .unwrap();
        let resp: Response = read_message(&mut client).await.unwrap();
        match resp {
            Response::Error { message } => assert!(message.contains("not found")),
            other => panic!("expected Error, got {other:?}"),
        }
        let _ = handle.await;
    }

    #[tokio::test]
    async fn sync_without_credentials_returns_error() {
        let (mut client, server) = paired_streams().await;
        let (mgr, vmgr, persister) = test_managers();
        let synced = Arc::new(Mutex::new(HashSet::new()));
        let cloud_tx = CloudManager::spawn(synced.clone(), mgr.clone());

        let tid = mgr.lock().await.create(None).unwrap();

        let handle = tokio::spawn(handle_connection(
            server,
            Arc::new(Notify::new()),
            mgr,
            vmgr,
            persister,
            cloud_tx,
        ));

        write_message(&mut client, &Request::TerminalSync { id: tid })
            .await
            .unwrap();
        let resp: Response = read_message(&mut client).await.unwrap();
        match resp {
            Response::Error { message } => assert!(message.contains("login")),
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(synced.lock().await.is_empty());
        let _ = handle.await;
    }
}
