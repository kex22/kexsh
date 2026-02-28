use std::collections::HashMap;
use std::io;

use crossterm::event::{Event, EventStream};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{ExecutableCommand, cursor};
use futures_lite::StreamExt;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::error::Result;
use crate::ipc::client::IpcClient;
use crate::ipc::message::{BinaryFrame, MuxRequest, MuxResponse, Request, Response, ViewInfo};
use crate::ipc::mux::{LocalMuxSender, local_mux_connect};
use crate::tui::input::{Action, InputHandler};
use crate::tui::layout::{PaneLayout, SplitDirection};
use crate::tui::renderer::Renderer;
use crate::tui::screen::Screen;
use crate::tui::vterm::VirtualTerminal;

pub async fn attach(terminal_name: &str) -> Result<()> {
    attach_view(terminal_name, &[], None, None, None).await
}

pub async fn attach_view(
    terminal_name: &str,
    extra_terminals: &[String],
    view_id: Option<&str>,
    saved_layout: Option<serde_json::Value>,
    saved_focused: Option<String>,
) -> Result<()> {
    let config = Config::load().unwrap_or_default();
    let (cols, rows) = terminal::size().unwrap_or((80, 24));

    // Build full terminal list for mux connection
    let mut all_ids = vec![terminal_name.to_string()];
    for t in extra_terminals {
        if !all_ids.contains(t) {
            all_ids.push(t.clone());
        }
    }

    let (mut mux_tx, mux_rx) = local_mux_connect(all_ids, view_id.map(String::from)).await?;

    // Send initial resize for primary terminal
    let screen = Screen::new(rows, cols);
    let pane = screen.pane_area();
    mux_tx
        .send_frame(
            terminal_name,
            &BinaryFrame::Resize {
                cols: pane.width,
                rows: pane.height,
            },
        )
        .await?;

    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    terminal::enable_raw_mode()?;
    stdout.execute(cursor::Hide)?;

    let result = run_tui(TuiInit {
        mux_tx,
        mux_rx,
        terminal_name,
        cols,
        rows,
        config,
        extra_terminals,
        view_id,
        saved_layout,
        saved_focused,
    })
    .await;

    let mut stdout = io::stdout();
    let _ = stdout.execute(cursor::Show);
    let _ = terminal::disable_raw_mode();
    let _ = stdout.execute(LeaveAlternateScreen);

    result
}

struct TuiSession {
    layout: PaneLayout,
    vterms: HashMap<String, VirtualTerminal>,
    mux_tx: LocalMuxSender<tokio::net::unix::OwnedWriteHalf>,
    screen: Screen,
    renderer: Renderer<io::Stdout>,
    input: InputHandler,
    config: Config,
    view_id: Option<String>,
    label: String,
}

impl TuiSession {
    fn render_all(&mut self) -> Result<()> {
        let pane_area = self.screen.pane_area();
        let rects = self.layout.compute_rects(pane_area);
        for (tid, rect) in &rects {
            if let Some(vterm) = self.vterms.get_mut(tid) {
                self.renderer.render_vterm(vterm, rect)?;
                vterm.take_dirty_rows();
            }
        }
        for (is_vert, x, y, len) in self.layout.compute_separators(pane_area) {
            if is_vert {
                self.renderer.render_vsep(x, y, len)?;
            } else {
                self.renderer.render_hsep(x, y, len)?;
            }
        }
        self.render_status()?;
        let focused = self.layout.focused_terminal();
        if let Some(vterm) = self.vterms.get(focused)
            && let Some((_, rect)) = rects.iter().find(|(id, _)| id == focused)
        {
            let (cr, cc) = vterm.cursor_position();
            io::stdout().execute(cursor::MoveTo(rect.x + cc, rect.y + cr))?;
            io::stdout().execute(cursor::Show)?;
        }
        self.renderer.flush()?;
        Ok(())
    }

    fn render_status(&mut self) -> Result<()> {
        if !self.config.status_bar {
            return Ok(());
        }
        let bar = self.screen.status_bar_area();
        self.renderer.render_status_bar(
            &self
                .input
                .mode()
                .status_text(&self.label, &self.config.prefix),
            &bar,
        )?;
        self.renderer.flush()?;
        Ok(())
    }

    fn handle_pty_data(&mut self, tid: &str, data: &[u8]) -> Result<()> {
        let Some(vterm) = self.vterms.get_mut(tid) else {
            return Ok(());
        };
        vterm.process(data);
        let rects = self.layout.compute_rects(self.screen.pane_area());
        let Some((_, rect)) = rects.iter().find(|(id, _)| id == tid) else {
            return Ok(());
        };
        let dirty = vterm.take_dirty_rows();
        if !dirty.is_empty() {
            self.renderer.render_vterm_rows(vterm, rect, &dirty)?;
        }
        if tid == self.layout.focused_terminal() {
            let (cr, cc) = vterm.cursor_position();
            io::stdout().execute(cursor::MoveTo(rect.x + cc, rect.y + cr))?;
            io::stdout().execute(cursor::Show)?;
        }
        self.renderer.flush()?;
        Ok(())
    }

    async fn resize_all_vterms(&mut self) -> Result<()> {
        for (tid, rect) in self.layout.compute_rects(self.screen.pane_area()) {
            if let Some(vterm) = self.vterms.get_mut(&tid) {
                vterm.resize(rect.height, rect.width);
            }
            let _ = self
                .mux_tx
                .send_frame(
                    &tid,
                    &BinaryFrame::Resize {
                        cols: rect.width,
                        rows: rect.height,
                    },
                )
                .await;
        }
        Ok(())
    }

    async fn handle_resize(&mut self, new_rows: u16, new_cols: u16) -> Result<()> {
        self.screen.resize(new_rows, new_cols);
        self.resize_all_vterms().await?;
        self.render_all()
    }

    async fn handle_split(&mut self, direction: SplitDirection) -> Result<()> {
        let focused_rect = self
            .layout
            .compute_rects(self.screen.pane_area())
            .into_iter()
            .find(|(id, _)| id == self.layout.focused_terminal())
            .map(|(_, r)| r);
        if let Some(r) = focused_rect {
            let too_small = match direction {
                SplitDirection::Vertical => r.width < 4,
                SplitDirection::Horizontal => r.height < 4,
            };
            if too_small {
                return Ok(());
            }
        }

        let new_id = match self
            .mux_tx
            .send_control(&MuxRequest::CreateTerminal { name: None })
            .await?
        {
            MuxResponse::TerminalCreated { id } => id,
            _ => return Ok(()),
        };

        match self
            .mux_tx
            .send_control(&MuxRequest::AddTerminal { id: new_id.clone() })
            .await?
        {
            MuxResponse::Ok => {}
            _ => {
                let _ = self
                    .mux_tx
                    .send_control(&MuxRequest::KillTerminal { id: new_id })
                    .await;
                return Ok(());
            }
        }

        let pane = self.screen.pane_area();
        self.vterms.insert(
            new_id.clone(),
            VirtualTerminal::new(pane.height, pane.width),
        );
        self.layout.split(direction, new_id.clone());
        self.resize_all_vterms().await?;
        self.render_all()?;

        if let Some(vid) = &self.view_id {
            let vid = vid.clone();
            if let Ok(mut c) = IpcClient::connect().await {
                let _ = c
                    .send(Request::ViewAddTerminal {
                        view_id: vid.clone(),
                        terminal_id: new_id,
                    })
                    .await;
            }
            self.sync_layout().await;
        }
        Ok(())
    }

    async fn handle_close(&mut self) -> Result<()> {
        if let Some(closed) = self.layout.close_focused() {
            self.vterms.remove(&closed);
            self.mux_tx
                .send_control(&MuxRequest::RemoveTerminal { id: closed.clone() })
                .await?;
            self.resize_all_vterms().await?;
            self.render_all()?;

            if let Some(vid) = &self.view_id {
                let vid = vid.clone();
                if let Ok(mut c) = IpcClient::connect().await {
                    let _ = c
                        .send(Request::ViewRemoveTerminal {
                            view_id: vid.clone(),
                            terminal_id: closed,
                        })
                        .await;
                }
                self.sync_layout().await;
            }
        }
        Ok(())
    }

    async fn handle_pane_resize(&mut self, dir: crate::tui::input::Direction) -> Result<()> {
        self.layout.resize_focused(dir, 0.05);
        self.resize_all_vterms().await?;
        self.render_all()?;
        if self.view_id.is_some() {
            self.sync_layout().await;
        }
        Ok(())
    }

    async fn switch_view(&mut self, terminal_ids: &[String]) -> Result<()> {
        if terminal_ids.is_empty() {
            return Ok(());
        }

        let new_set: std::collections::HashSet<&String> = terminal_ids.iter().collect();
        let old_ids: Vec<String> = self.vterms.keys().cloned().collect();

        // Remove terminals not in new set
        for id in &old_ids {
            if !new_set.contains(id) {
                let _ = self
                    .mux_tx
                    .send_control(&MuxRequest::RemoveTerminal { id: id.clone() })
                    .await;
                self.vterms.remove(id);
            }
        }

        let first = &terminal_ids[0];
        self.layout = PaneLayout::new(first.clone());

        let pane_area = self.screen.pane_area();
        for tid in terminal_ids {
            if !self.vterms.contains_key(tid) {
                if !matches!(
                    self.mux_tx
                        .send_control(&MuxRequest::AddTerminal { id: tid.clone() })
                        .await,
                    Ok(MuxResponse::Ok)
                ) {
                    continue;
                }
                self.vterms.insert(
                    tid.clone(),
                    VirtualTerminal::new(pane_area.height, pane_area.width),
                );
            }
            if tid != first
                && !self
                    .layout
                    .compute_rects(pane_area)
                    .iter()
                    .any(|(id, _)| id == tid)
            {
                self.layout.split(SplitDirection::Vertical, tid.clone());
            }
        }

        self.resize_all_vterms().await?;
        self.render_all()
    }

    async fn sync_layout(&self) {
        if let Some(vid) = &self.view_id
            && let Ok(mut c) = IpcClient::connect().await
        {
            let _ = c
                .send(Request::ViewUpdateLayout {
                    view_id: vid.clone(),
                    layout: self.layout.to_value(),
                    focused: self.layout.focused_terminal().to_string(),
                })
                .await;
        }
    }

    async fn detach_all(&mut self) {
        let _ = self
            .mux_tx
            .send_frame("\0\0\0\0\0\0\0\0", &BinaryFrame::Detach)
            .await;
    }
}

struct TuiInit<'a> {
    mux_tx: LocalMuxSender<tokio::net::unix::OwnedWriteHalf>,
    mux_rx: crate::ipc::mux::LocalMuxReceiver<tokio::net::unix::OwnedReadHalf>,
    terminal_name: &'a str,
    cols: u16,
    rows: u16,
    config: Config,
    extra_terminals: &'a [String],
    view_id: Option<&'a str>,
    saved_layout: Option<serde_json::Value>,
    saved_focused: Option<String>,
}

async fn run_tui(init: TuiInit<'_>) -> Result<()> {
    let has_saved_layout = matches!(&init.saved_layout, Some(v) if !v.is_null());
    let layout = match init.saved_layout {
        Some(v) if !v.is_null() => {
            PaneLayout::from_value(v, init.saved_focused.as_deref(), init.terminal_name)
        }
        _ => PaneLayout::new(init.terminal_name.to_string()),
    };

    let (tx, mut rx) = mpsc::channel::<(String, Vec<u8>)>(64);

    let pane_area = Screen::new(init.rows, init.cols).pane_area();
    let mut session = TuiSession {
        layout,
        vterms: HashMap::new(),
        mux_tx: init.mux_tx,
        screen: Screen::new(init.rows, init.cols),
        renderer: Renderer::new(io::stdout()),
        input: InputHandler::with_prefix(init.config.prefix.clone()),
        config: init.config,
        view_id: init.view_id.map(String::from),
        label: init.terminal_name.to_string(),
    };

    // Create vterms for all terminals (already attached via mux handshake)
    session.vterms.insert(
        init.terminal_name.to_string(),
        VirtualTerminal::new(pane_area.height, pane_area.width),
    );
    for tid in init.extra_terminals {
        session.vterms.insert(
            tid.clone(),
            VirtualTerminal::new(pane_area.height, pane_area.width),
        );
        if !has_saved_layout {
            session.layout.split(SplitDirection::Vertical, tid.clone());
        }
    }

    // Single recv task for all terminals
    let mut mux_rx = init.mux_rx;
    tokio::spawn(async move {
        while let Ok((tid, frame)) = mux_rx.recv_frame().await {
            if let BinaryFrame::Data(data) = frame
                && tx.send((tid, data)).await.is_err()
            {
                break;
            }
        }
    });

    session.resize_all_vterms().await?;
    session.render_all()?;

    let mut event_reader = EventStream::new();

    loop {
        tokio::select! {
            Some((tid, data)) = rx.recv() => {
                session.handle_pty_data(&tid, &data)?;
            }
            Some(Ok(event)) = event_reader.next() => {
                if let Event::Resize(new_cols, new_rows) = event {
                    session.handle_resize(new_rows, new_cols).await?;
                    continue;
                }
                match session.input.handle_event(&event) {
                    Action::SendToTerminal(bytes) => {
                        let focused = session.layout.focused_terminal().to_string();
                        if session.mux_tx.send_frame(&focused, &BinaryFrame::Data(bytes)).await.is_err()
                        {
                            break;
                        }
                    }
                    Action::ModeChanged(_) => {
                        session.render_status()?;
                    }
                    Action::PaneSplitHorizontal => {
                        session.handle_split(SplitDirection::Horizontal).await?;
                    }
                    Action::PaneSplitVertical => {
                        session.handle_split(SplitDirection::Vertical).await?;
                    }
                    Action::PaneNavigate(dir) => {
                        session.layout.navigate(dir, session.screen.pane_area());
                        session.render_all()?;
                    }
                    Action::PaneResize(dir) => {
                        session.handle_pane_resize(dir).await?;
                    }
                    Action::PaneClose => {
                        session.handle_close().await?;
                    }
                    Action::ViewList => {
                        if let Ok(text) = fetch_view_list_text().await {
                            let bar = session.screen.status_bar_area();
                            session.renderer.render_status_bar(&text, &bar)?;
                            session.renderer.flush()?;
                        }
                    }
                    Action::ViewSwitch(n) => {
                        if let Ok(info) = fetch_view_by_index(n).await {
                            session.switch_view(&info.terminal_ids).await?;
                        }
                    }
                    Action::Detach => break,
                    _ => {}
                }
            }
            else => break,
        }
    }

    session.detach_all().await;
    Ok(())
}

async fn fetch_view_list_text() -> Result<String> {
    let mut client = IpcClient::connect().await?;
    match client.send(Request::ViewList).await? {
        Response::ViewList { views } => {
            if views.is_empty() {
                return Ok(" [VIEWS] (none)".into());
            }
            let names: Vec<String> = views
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let label = v.name.as_deref().unwrap_or(&v.id);
                    format!("{}:{label}", i + 1)
                })
                .collect();
            Ok(format!(" [VIEWS] {}", names.join(" | ")))
        }
        _ => Ok(" [VIEWS] error".into()),
    }
}

async fn fetch_view_by_index(n: usize) -> Result<ViewInfo> {
    let mut client = IpcClient::connect().await?;
    match client.send(Request::ViewList).await? {
        Response::ViewList { views } => {
            if n == 0 || n > views.len() {
                return Err(crate::error::KexshError::Server(format!(
                    "view index {n} out of range"
                )));
            }
            Ok(views.into_iter().nth(n - 1).unwrap())
        }
        _ => Err(crate::error::KexshError::Server(
            "unexpected response".into(),
        )),
    }
}
