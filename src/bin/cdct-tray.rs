//! `cdct-tray` — the tray/menu-bar client.
//!
//! It shows who holds the card and offers the other kernels. The menu is built
//! from [`cdct::tray`], a pure model; this binary only renders it with
//! `tray-icon` (StatusNotifierItem on Linux, the menu bar on macOS, the tray on
//! Windows) and forwards clicks as switch requests.
//!
//! Updates arrive from the daemon's subscription, never from a timer: the
//! reader thread blocks on the pushed replies and wakes the event loop with
//! each new status.
//!
//! This binary is built only with the `tray` feature, so the daemon and the CLI
//! carry no GUI dependency.

use std::thread;
use std::time::Duration;

use cdct::client::{Client, Endpoint};
use cdct::config::Config;
use cdct::describe;
use cdct::protocol::{Reply, Request, Status};
use cdrv::KernelOwner;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::WindowId;

/// What wakes the event loop.
enum UserEvent {
    /// A fresh status snapshot from the daemon.
    Status(Status),
    /// A menu entry was chosen.
    Menu(MenuEvent),
    /// The subscription is down; the tray says so until it returns.
    Disconnected(String),
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = load_config()?;
    let endpoint = Endpoint::detect(&config.socket);

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = menu_proxy.send_event(UserEvent::Menu(event));
    }));

    start_subscription(endpoint.clone(), proxy.clone());

    let mut app = TrayApp {
        endpoint,
        tray: None,
        entries: Vec::new(),
        status: None,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// `--config <path>` or `--config=<path>`; the only option the tray takes.
fn config_argument() -> Option<std::path::PathBuf> {
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--config" {
            return args.next().map(Into::into);
        }
        if let Some(value) = arg.to_str().and_then(|a| a.strip_prefix("--config=")) {
            return Some(value.into());
        }
    }
    None
}

fn load_config() -> Result<Config, Box<dyn std::error::Error>> {
    if let Some(path) = config_argument() {
        return Ok(Config::load(&path)?);
    }
    match Config::default_path() {
        Some(path) if path.exists() => Ok(Config::load(&path)?),
        _ => Ok(Config::default()),
    }
}

struct TrayApp {
    endpoint: Endpoint,
    tray: Option<TrayIcon>,
    /// The menu entries, by id, so a click resolves to a kernel.
    entries: Vec<(MenuId, KernelOwner)>,
    status: Option<Status>,
}

impl TrayApp {
    fn render(&mut self, status: &Status) {
        let model = cdct::tray::menu(status);
        let menu = Menu::new();
        let header = MenuItem::new(&model.title, false, None);
        let _ = menu.append(&header);
        let _ = menu.append(&PredefinedMenuItem::separator());
        self.entries.clear();
        for (index, entry) in model.entries.iter().enumerate() {
            let text = match &entry.reason {
                Some(reason) => format!("{} — {reason}", entry.label),
                None => entry.label.clone(),
            };
            let id = MenuId(format!("target-{index}"));
            let item = MenuItem::with_id(id.clone(), text, entry.enabled, None);
            let _ = menu.append(&item);
            self.entries.push((id, entry.kernel.clone()));
        }
        if let Some(tray) = &self.tray {
            tray.set_menu(Some(Box::new(menu)));
            let _ = tray.set_tooltip(Some(&model.title));
            tray.set_title(Some(&model.title));
        }
    }

    fn request_switch(&self, kernel: KernelOwner) {
        let endpoint = self.endpoint.clone();
        thread::spawn(move || match Client::connect(&endpoint) {
            Ok(mut client) => match client.switch(&kernel) {
                Ok(Reply::Switch { outcome }) => {
                    if !matches!(
                        outcome,
                        cdct::SwitchOutcome::Plan(_) | cdct::SwitchOutcome::AlreadyHeld { .. }
                    ) {
                        eprintln!("cdct-tray: {}", describe::outcome(&outcome).join(" "));
                    }
                }
                Ok(other) => eprintln!("cdct-tray: unexpected reply: {other:?}"),
                Err(error) => eprintln!("cdct-tray: switch failed: {error}"),
            },
            Err(error) => eprintln!("cdct-tray: {error}"),
        });
    }
}

impl ApplicationHandler<UserEvent> for TrayApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        if self.tray.is_some() {
            return;
        }
        match TrayIconBuilder::new()
            .with_menu(Box::new(Menu::new()))
            .with_icon(app_icon())
            .with_tooltip("cdct — who holds the GPU")
            .build()
        {
            Ok(tray) => {
                self.tray = Some(tray);
                if let Some(status) = self.status.clone() {
                    self.render(&status);
                }
            }
            Err(error) => eprintln!("cdct-tray: cannot create the tray icon: {error}"),
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        _event: WindowEvent,
    ) {
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Status(status) => {
                self.render(&status);
                self.status = Some(status);
            }
            UserEvent::Menu(event) => {
                let kernel = self
                    .entries
                    .iter()
                    .find(|(id, _)| *id == event.id)
                    .map(|(_, kernel)| kernel.clone());
                if let Some(kernel) = kernel {
                    self.request_switch(kernel);
                }
            }
            UserEvent::Disconnected(reason) => {
                eprintln!("cdct-tray: subscription lost: {reason}");
                if let Some(tray) = &self.tray {
                    tray.set_title(Some("GPU: unavailable"));
                    let _ = tray.set_tooltip(Some("cdct: the conductor is unreachable"));
                }
            }
        }
    }
}

/// Subscribe on a blocking reader thread and wake the event loop on each push.
fn start_subscription(endpoint: Endpoint, proxy: EventLoopProxy<UserEvent>) {
    thread::spawn(move || {
        loop {
            match Client::connect(&endpoint) {
                Ok(mut client) => {
                    if client.send(&Request::Subscribe).is_err() {
                        let _ = proxy.send_event(UserEvent::Disconnected(
                            "cannot send the subscription".to_owned(),
                        ));
                    } else {
                        loop {
                            match client.receive() {
                                Ok(Some(Reply::Subscribed { status, .. }))
                                | Ok(Some(Reply::Status { status })) => {
                                    if proxy.send_event(UserEvent::Status(status)).is_err() {
                                        return;
                                    }
                                }
                                Ok(Some(Reply::Event { .. })) => {}
                                Ok(Some(_)) => {}
                                Ok(None) => break,
                                Err(error) => {
                                    let _ = proxy
                                        .send_event(UserEvent::Disconnected(error.to_string()));
                                    break;
                                }
                            }
                        }
                    }
                }
                Err(error) => {
                    let _ = proxy.send_event(UserEvent::Disconnected(error.to_string()));
                }
            }
            // A reconnect delay, not state polling: the stream itself is pushed.
            thread::sleep(Duration::from_secs(2));
        }
    });
}

/// A small opaque white circle; macOS draws it as a template image.
fn app_icon() -> tray_icon::Icon {
    const SIZE: u32 = 16;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - 7.5;
            let dy = y as f32 - 7.5;
            let alpha = if dx * dx + dy * dy <= 49.0 { 255 } else { 0 };
            rgba.extend_from_slice(&[255, 255, 255, alpha]);
        }
    }
    tray_icon::Icon::from_rgba(rgba, SIZE, SIZE).expect("the generated icon is valid")
}
