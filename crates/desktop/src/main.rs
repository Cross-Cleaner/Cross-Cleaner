#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use clap::{ArgAction, Parser};
use database::cleaner_database::CleanerDatabase;
#[cfg(windows)]
use database::registry_database::RegistryDatabase;
use database::structures::CustomCleaner;
#[cfg(feature = "self-update")]
use database::version::check_new_version;
use eframe::UserEvent;
use eframe::egui;
use gui::app::MyApp;
use gui::icons;
use gui::sounds;
use gui::title_bar::TITLE_BAR_HEIGHT;
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::error::EventLoopError;
use winit::event::{DeviceEvent, DeviceId, MouseScrollDelta, StartCause, TouchPhase, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

#[cfg(target_os = "linux")]
use gui::display;

/// Minimum interval between repaints caused purely by pointer movement or by
/// scroll-wheel input.
///
/// Software rendering (e.g. Windows Sandbox without a GPU) is CPU-bound, and
/// `egui-winit` requests an immediate repaint for every `CursorMoved` and every
/// `MouseWheel` event. High-resolution wheels and touchpads emit those at a
/// very high rate, so they are coalesced here. Hover/hit-testing uses
/// `CursorMoved`, so only that event is throttled by dropping; wheel deltas are
/// accumulated and forwarded together so scrolling is not lost. Raw
/// `DeviceEvent::MouseMotion` is dropped entirely (see `device_event`) because
/// it arrives at the mouse polling rate and adds no useful information here.
const POINTER_REPAINT_INTERVAL: Duration = Duration::from_millis(50);

/// Interval between flushed scroll-wheel deltas. Higher than the pointer
/// interval because each flush translates into a full (software-rendered)
/// redraw of the scroll area.
const WHEEL_REPAINT_INTERVAL: Duration = Duration::from_millis(100);

/// Global cap for repaints initiated by egui itself (animations, smooth
/// scrolling, timers, ...). egui requests an immediate repaint on every frame
/// while its internal scroll smoothing is active, which keeps the software
/// renderer at full frame rate for as long as a scroll is in flight. Input
/// repaints do not go through this path, so the UI still reacts immediately.
const MIN_REPAINT_INTERVAL: Duration = Duration::from_millis(33);

/// A scroll-wheel event waiting to be flushed.
struct PendingWheel {
    window_id: WindowId,
    device_id: DeviceId,
    delta: MouseScrollDelta,
    phase: TouchPhase,
}

/// Adds `delta` to `acc` when both use the same unit. Returns `false` if the
/// variants differ (in which case the caller forwards them separately).
fn merge_scroll(acc: &mut MouseScrollDelta, delta: &MouseScrollDelta) -> bool {
    match (acc, delta) {
        (MouseScrollDelta::LineDelta(ax, ay), MouseScrollDelta::LineDelta(bx, by)) => {
            *ax += *bx;
            *ay += *by;
            true
        }
        (MouseScrollDelta::PixelDelta(a), MouseScrollDelta::PixelDelta(b)) => {
            a.x += b.x;
            a.y += b.y;
            true
        }
        _ => false,
    }
}

/// Wraps eframe's winit application and coalesces pointer-movement and
/// scroll-wheel events so they cannot force a repaint on every OS event.
struct PointerThrottle<'a> {
    inner: eframe::EframeWinitApplication<'a>,
    last_pointer_repaint: Option<Instant>,
    pending_wheel: Option<PendingWheel>,
    last_wheel_flush: Option<Instant>,
}

impl<'a> PointerThrottle<'a> {
    fn new(inner: eframe::EframeWinitApplication<'a>) -> Self {
        Self {
            inner,
            last_pointer_repaint: None,
            pending_wheel: None,
            last_wheel_flush: None,
        }
    }

    fn allow_pointer_move(&mut self) -> bool {
        let now = Instant::now();
        match self.last_pointer_repaint {
            Some(last) if now.duration_since(last) < POINTER_REPAINT_INTERVAL => false,
            _ => {
                self.last_pointer_repaint = Some(now);
                true
            }
        }
    }

    fn forward_wheel(&mut self, event_loop: &ActiveEventLoop, wheel: PendingWheel) {
        self.last_wheel_flush = Some(Instant::now());
        self.inner.window_event(
            event_loop,
            wheel.window_id,
            WindowEvent::MouseWheel {
                device_id: wheel.device_id,
                delta: wheel.delta,
                phase: wheel.phase,
            },
        );
    }
}

impl ApplicationHandler<UserEvent> for PointerThrottle<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.resumed(event_loop);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        match &event {
            WindowEvent::CursorMoved { .. } => {
                if !self.allow_pointer_move() {
                    return;
                }
            }
            WindowEvent::MouseWheel {
                device_id,
                delta,
                phase,
            } => {
                let (device_id, delta, phase) = (*device_id, *delta, *phase);
                let now = Instant::now();
                let interval_elapsed = self
                    .last_wheel_flush
                    .is_none_or(|last| now.duration_since(last) >= WHEEL_REPAINT_INTERVAL);

                if interval_elapsed && self.pending_wheel.is_none() {
                    self.last_wheel_flush = Some(now);
                    self.inner.window_event(
                        event_loop,
                        window_id,
                        WindowEvent::MouseWheel {
                            device_id,
                            delta,
                            phase,
                        },
                    );
                    return;
                }

                let merged = match self.pending_wheel.as_mut() {
                    Some(pending) if pending.window_id == window_id => {
                        merge_scroll(&mut pending.delta, &delta)
                    }
                    _ => false,
                };
                if merged {
                    let pending = self.pending_wheel.as_mut().expect("pending exists");
                    pending.device_id = device_id;
                    pending.phase = phase;
                } else if interval_elapsed {
                    // Different unit: cannot merge, so forward this one now and
                    // let the pending accumulation flush on the next tick.
                    self.inner.window_event(
                        event_loop,
                        window_id,
                        WindowEvent::MouseWheel {
                            device_id,
                            delta,
                            phase,
                        },
                    );
                } else {
                    self.pending_wheel = Some(PendingWheel {
                        window_id,
                        device_id,
                        delta,
                        phase,
                    });
                }
                return;
            }
            _ => {}
        }
        self.inner.window_event(event_loop, window_id, event);
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        // A scheduled wheel flush woke us up: deliver it before eframe computes
        // its next repaint time, so the redraw is requested in this iteration.
        if matches!(cause, StartCause::ResumeTimeReached { .. })
            && let Some(wheel) = self.pending_wheel.take()
        {
            self.forward_wheel(event_loop, wheel);
        }
        self.inner.new_events(event_loop, cause);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        // Clamp egui's own repaint requests to a minimum interval. This is what
        // stops "smooth scroll" (and other continuous animations) from
        // repainting at the display refresh rate.
        let event = match event {
            UserEvent::RequestRepaint {
                viewport_id,
                when,
                cumulative_pass_nr,
            } => {
                let floor = Instant::now() + MIN_REPAINT_INTERVAL;
                UserEvent::RequestRepaint {
                    viewport_id,
                    when: when.max(floor),
                    cumulative_pass_nr,
                }
            }
            // The `accesskit` feature adds more variants.
            #[allow(unreachable_patterns)]
            other => other,
        };
        self.inner.user_event(event_loop, event);
    }

    fn device_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        device_id: DeviceId,
        event: DeviceEvent,
    ) {
        // Raw mouse motion is delivered at the mouse polling rate (up to
        // 1000 Hz) and also forces a repaint, but carries no information the UI
        // needs: pointer position and hover come from `CursorMoved`. Dropping
        // it prevents the cursor-position event from being starved.
        if matches!(event, DeviceEvent::MouseMotion { .. }) {
            return;
        }
        self.inner.device_event(event_loop, device_id, event);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.about_to_wait(event_loop);

        if self.pending_wheel.is_none() {
            return;
        }
        let deadline = self
            .last_wheel_flush
            .map_or_else(Instant::now, |last| last + WHEEL_REPAINT_INTERVAL);
        if Instant::now() >= deadline {
            let wheel = self.pending_wheel.take().expect("pending exists");
            self.forward_wheel(event_loop, wheel);
            // Make sure eframe processes the freshly scheduled repaint.
            event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now()));
        } else {
            let should_set = match event_loop.control_flow() {
                ControlFlow::Wait => true,
                ControlFlow::WaitUntil(existing) => deadline < existing,
                ControlFlow::Poll => false,
            };
            if should_set {
                event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
            }
        }
    }

    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.suspended(event_loop);
    }

    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.exiting(event_loop);
    }

    fn memory_warning(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.memory_warning(event_loop);
    }
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Disable all built-in custom cleanings.
    /// Example: --disable-custom=true
    #[arg(long, value_name = "bool", default_value_t = false, action = ArgAction::Set)]
    disable_custom: bool,

    /// Specify a custom database file path.
    /// Example: --database-path=custom_database.json
    #[arg(long, value_name = "path")]
    database_path: Option<String>,

    /// Specify a custom registry database file path.
    /// Example: --registry-database-path=custom_database.json
    #[cfg(windows)]
    #[arg(long, value_name = "registry_path")]
    registry_database_path: Option<String>,
}

/// Shown when no display server can be reached at all, i.e. neither a
/// compositor socket nor a usable X server was found.
const DISPLAY_HINT: &str = "\
Cross Cleaner could not reach a display server. Start it from your desktop \
session; when elevating, keep the session environment \
(`sudo -E Cross_Cleaner_GUI`).";

/// Backend [`display::detect`] picked before the event loop was created.
#[cfg(target_os = "linux")]
type BackendChoice = Option<display::Backend>;
/// No backend choice on platforms where winit has a single display server.
#[cfg(not(target_os = "linux"))]
type BackendChoice = ();

/// Creates the event loop, forcing the backend chosen by [`prepare_display`].
#[allow(unused_variables)]
fn build_event_loop(backend: BackendChoice) -> Result<EventLoop<UserEvent>, EventLoopError> {
    #[allow(unused_mut)]
    let mut builder = EventLoop::<UserEvent>::with_user_event();
    #[cfg(target_os = "linux")]
    match backend {
        Some(display::Backend::Wayland) => {
            use winit::platform::wayland::EventLoopBuilderExtWayland;
            builder.with_wayland();
        }
        Some(display::Backend::X11) => {
            use winit::platform::x11::EventLoopBuilderExtX11;
            builder.with_x11();
        }
        None => {}
    }
    builder.build()
}

fn main() -> eframe::Result {
    // INFO: `std::env::set_var` is `unsafe` in edition 2024 and must not race
    // with other threads reading the environment, so the display has to be
    // prepared before the tokio runtime spawns its workers. That is why `main`
    // is not `#[tokio::main]`.
    let backend = prepare_display();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| eframe::Error::AppCreation(Box::new(e)))?
        .block_on(run(backend))
}

/// Picks the window backend and restores the session variables it needs.
#[cfg(target_os = "linux")]
fn prepare_display() -> BackendChoice {
    let display = display::detect();
    display.apply();
    display.backend
}

#[cfg(not(target_os = "linux"))]
fn prepare_display() -> BackendChoice {}

async fn run(backend: BackendChoice) -> eframe::Result {
    let icon = icons::load_icon_from_ico_bytes(database::ICON_BYTES).expect("Failed to load icon");

    let args = Args::parse();

    // INFO: Initialize UI sounds (no-op without an audio device)
    sounds::init();

    // INFO: Register all built-in custom cleanings (functions in cleaner::custom_cleaners)
    cleaner::custom_cleaners::register_all();
    // INFO: Scripts register theirs into the same registry, before it is read.
    appcore::scripts::register_custom_cleaners();

    let custom_database: Arc<[CustomCleaner]> = if args.disable_custom {
        Arc::from(Vec::new())
    } else {
        Arc::from(database::custom_cleaners::get_custom_cleaners())
    };

    // INFO: Validate a user-supplied database early; the entries themselves are
    // streamed on demand (see CleanerDatabase::for_each).
    let database = if let Some(db_path) = &args.database_path {
        let database = CleanerDatabase::from_file(db_path);
        if let Err(e) = database.for_each(|_| {}) {
            eprintln!("Failed to load database from file: {}", e);
            std::process::exit(1);
        }
        database
    } else {
        CleanerDatabase::default_source()
    };
    let database = appcore::scripts::with_rules(database);

    #[cfg(windows)]
    let registry_database = if let Some(db_path) = &args.registry_database_path {
        let database = RegistryDatabase::from_file(db_path);
        if let Err(e) = database.for_each(|_| {}) {
            eprintln!("Failed to load database from file: {}", e);
            std::process::exit(1);
        }
        database
    } else {
        RegistryDatabase::default_source()
    };
    #[cfg(windows)]
    let registry_database = appcore::scripts::with_registry_rules(registry_database);
    // Keep original databases for the fallback loop (they are Clone and cheap).
    // `app` is only needed to compute window height.
    #[cfg(windows)]
    let app_for_size = MyApp::from_database(
        database.clone(),
        registry_database.clone(),
        custom_database.clone(),
    );
    #[cfg(not(windows))]
    let app_for_size = MyApp::from_database(database.clone(), custom_database.clone());
    let checkbox_count = app_for_size.state.categories.len();
    let rows = checkbox_count.div_ceil(3);
    // INFO: 20px for 1 checkbox, 45px for button, 32px for custom title bar
    let height = (rows * 20) + 445 + TITLE_BAR_HEIGHT as usize;

    let size = egui::vec2(570.0, height as f32);

    // --- Renderer fallback chain: glow (OpenGL) -> vulkan -> DirectX 12 ---
    // Default: glow (smaller binary, good for older GPUs).
    // If glow is not available, fall back to wgpu with Vulkan, then DX12.
    // Probe wgpu backends without creating a window so we can pick the best
    // available before building NativeOptions. Runtime fallback (window
    // creation failure) is also handled by retrying the next candidate.
    #[cfg(target_os = "windows")]
    fn is_wgpu_backend_available(backend: eframe::wgpu::Backends) -> bool {
        let mut setup = eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle();
        setup.instance_descriptor.backends = backend;
        let desc = eframe::wgpu::InstanceDescriptor {
            backends: setup.instance_descriptor.backends,
            flags: setup.instance_descriptor.flags,
            backend_options: setup.instance_descriptor.backend_options.clone(),
            memory_budget_thresholds: setup.instance_descriptor.memory_budget_thresholds,
            display: None,
        };
        let instance = eframe::wgpu::Instance::new(desc);
        let adapters = futures::executor::block_on(instance.enumerate_adapters(backend));
        !adapters.is_empty()
    }

    #[cfg(target_os = "windows")]
    fn configure_wgpu_backend(
        options: &mut eframe::NativeOptions,
        backend: eframe::wgpu::Backends,
    ) {
        let mut wgpu_cfg = eframe::egui_wgpu::WgpuConfiguration::default();
        let mut setup = eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle();
        setup.instance_descriptor.backends = backend;
        wgpu_cfg.wgpu_setup = eframe::egui_wgpu::WgpuSetup::CreateNew(setup);
        options.wgpu_options = wgpu_cfg;
    }

    // Build ordered candidates. Glow first, then Vulkan, then DX12.
    let mut candidates: Vec<(eframe::Renderer, Option<eframe::wgpu::Backends>, &str)> = Vec::new();
    #[cfg(target_os = "windows")]
    {
        // Windows: eframe built with both glow and wgpu
        candidates.push((eframe::Renderer::Glow, None, "glow"));
        candidates.push((
            eframe::Renderer::Wgpu,
            Some(eframe::wgpu::Backends::VULKAN),
            "vulkan",
        ));
        candidates.push((
            eframe::Renderer::Wgpu,
            Some(eframe::wgpu::Backends::DX12),
            "dx12",
        ));
        candidates.push((eframe::Renderer::Wgpu, None, "wgpu-auto"));
    }
    #[cfg(target_os = "linux")]
    {
        // Linux: only glow is enabled (per Cargo.toml)
        candidates.push((eframe::Renderer::Glow, None, "glow"));
    }
    #[cfg(target_os = "macos")]
    {
        candidates.push((eframe::Renderer::Glow, None, "glow"));
        candidates.push((eframe::Renderer::Wgpu, None, "wgpu-auto"));
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        candidates.push((eframe::Renderer::default(), None, "default"));
    }
    // Fallback if no renderer feature is detected (should not happen)
    if candidates.is_empty() {
        candidates.push((eframe::Renderer::default(), None, "default"));
    }

    // Filter by probe: keep glow always (cheap, assume available),
    // for wgpu candidates skip if that exact backend has no adapter.
    #[cfg(target_os = "windows")]
    let candidates: Vec<_> = candidates
        .into_iter()
        .filter(|(_, backend, _)| {
            if let Some(b) = backend {
                if *b == eframe::wgpu::Backends::VULKAN || *b == eframe::wgpu::Backends::DX12 {
                    is_wgpu_backend_available(*b)
                } else {
                    true
                }
            } else {
                true // glow / auto
            }
        })
        .collect();

    // Ensure at least glow remains even if probes filtered everything
    let candidates = if candidates.is_empty() {
        vec![(eframe::Renderer::Glow, None, "glow")]
    } else {
        candidates
    };

    let app_title = format!("Cross Cleaner GUI");

    // INFO: winit allows exactly one event loop per process, and it is
    // consumed by `run_app`, so a renderer that fails at runtime (glow
    // NoGlutinConfigs, wgpu RequestAdapterError) cannot be retried with the
    // next candidate: every later `build` returns `RecreationAttempt`. The
    // probes above are therefore what the fallback chain is made of, and only
    // the first surviving candidate is used.
    let (renderer, wgpu_backend, name) = candidates
        .into_iter()
        .next()
        .expect("candidates is never empty");

    #[allow(unused_mut)]
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(size)
            .with_min_inner_size(size)
            .with_max_inner_size(size)
            .with_resizable(false)
            .with_maximize_button(false)
            .with_decorations(false)
            .with_icon(icon.clone()),
        renderer,
        ..Default::default()
    };
    if let Some(backend) = wgpu_backend {
        #[cfg(target_os = "windows")]
        configure_wgpu_backend(&mut options, backend);
        #[cfg(not(target_os = "windows"))]
        {
            let _ = backend;
        }
    }
    // Also apply vsync etc from default wgpu config when using wgpu
    eprintln!("Using renderer: {name} ({renderer})");

    // We use our own EventLoop + PointerThrottle so pointer events are
    // coalesced. `create_native` itself is infallible; a failure to reach a
    // display server surfaces here as `WinitEventLoop`.
    let event_loop = match build_event_loop(backend) {
        Ok(el) => el,
        Err(e) => {
            eprintln!("Failed to create the {name} event loop: {e}");
            eprintln!("{DISPLAY_HINT}");
            return Err(eframe::Error::WinitEventLoop(e));
        }
    };

    // Move app into the closure; update_receiver is filled in the background
    let app_for_closure = {
        // Every mutation below sits behind the `self-update` feature, so without
        // it the binding is never written to.
        #[allow(unused_mut)]
        #[cfg(windows)]
        let mut a = MyApp::from_database(database, registry_database, custom_database);
        #[allow(unused_mut)]
        #[cfg(not(windows))]
        let mut a = MyApp::from_database(database, custom_database);
        // Version check and the notification built from its answer. A build
        // without the `self-update` feature skips both: the check never runs, so
        // no release is ever found and no update notification is raised. That is
        // the point of the flag for a distribution package — the repository owns
        // the version, so the app must not advertise a newer one.
        #[cfg(feature = "self-update")]
        {
            let (tx, rx) = std::sync::mpsc::channel();
            a.update_receiver = Some(rx);
            std::thread::spawn(move || {
                let _ = tx.send(check_new_version());
            });
        }
        // Self-update worker: downloads the new release and replaces the
        // running executable with it (see the `selfupdate` crate). Registering
        // it is what turns the update notification into an actual in-app update;
        // without it the GUI only offers the release page.
        //
        // The `self-update` feature is the switch for the whole update path, check
        // and worker together. A build without it compiles no worker and leaves
        // `updater_tx` empty, and nothing can rewrite the executable on disk.
        #[cfg(feature = "self-update")]
        {
            let (updater_tx, updater_rx) = std::sync::mpsc::channel();
            let updater_state = a.updater_state.clone();
            a.updater_tx = Some(updater_tx);
            std::thread::spawn(move || selfupdate::run(updater_rx, updater_state));
        }
        a
    };

    let mut native_app = PointerThrottle::new(eframe::create_native(
        &app_title,
        options,
        Box::new(move |_cc| {
            _cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(app_for_closure))
        }),
        &event_loop,
    ));

    event_loop
        .run_app(&mut native_app)
        .map_err(eframe::Error::WinitEventLoop)
}

// INFO: every test below builds a `RegistryDatabase`, which only exists on
// Windows, so the module is compiled there only.
#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use database::structures::{CleanerData, CleanerDataRegistry, CleanerFlags};
    use gui::app::Page;

    #[test]
    fn test_load_icon_from_bytes() {
        let icon_data = database::ICON_BYTES;
        let result = icons::load_icon_from_bytes(&icon_data[..]);

        assert!(result.is_ok(), "Icon should load successfully");
        let icon = result.unwrap();
        assert!(!icon.rgba.is_empty(), "Icon RGBA data should not be empty");
    }

    #[test]
    fn test_myapp_from_database() {
        let database: Vec<CleanerData> = vec![
            CleanerData {
                path: String::from("test/path1").into(),
                category: std::sync::Arc::from("Cache"),
                program: std::sync::Arc::from("TestApp1"),
                class: std::sync::Arc::from("Application"),
                sub_category: std::sync::Arc::from("Browser"),
                files_to_remove: vec![],
                directories_to_remove: vec![],
                flags: CleanerFlags::empty(),
            },
            CleanerData {
                path: String::from("test/path2").into(),
                category: std::sync::Arc::from("Logs"),
                program: std::sync::Arc::from("TestApp2"),
                class: std::sync::Arc::from("Application"),
                sub_category: std::sync::Arc::from("System"),
                files_to_remove: vec![],
                directories_to_remove: vec![],
                flags: CleanerFlags::empty(),
            },
        ];

        let registry_database: Vec<CleanerDataRegistry> = vec![CleanerDataRegistry {
            category: std::sync::Arc::from(""),
            program: std::sync::Arc::from(""),
            class: std::sync::Arc::from(""),
            sub_category: std::sync::Arc::from(""),
            remove_all_in_tree: false,
            remove_all_in_registry: false,
            path: String::new().into(),
            values_to_remove: vec![],
            keys_to_remove: vec![],
            remove_values: String::new(),
            remove_trees: String::new(),
        }];

        let app = MyApp::from_database(
            CleanerDatabase::from_vec(database),
            RegistryDatabase::from_vec(registry_database),
            Arc::from(Vec::new()),
        );

        // The category index itself is covered by `appcore`; here we only check
        // that `MyApp` wires the shared state up correctly.
        let state = &app.state;
        assert_eq!(state.categories.len(), 2, "Should have 2 categories");
        assert!(
            state.task_handle.is_none(),
            "Task handle should be None initially"
        );
        assert_eq!(state.current_page, Page::Main, "Should start on Main page");
        assert_eq!(state.current_task, 0, "Current task should be 0");
        assert_eq!(state.total_tasks, 0, "Total tasks should be 0");
        assert!(
            state.program_checkboxes.is_empty(),
            "Program checkboxes should be empty"
        );
        assert!(
            state.excluded_programs.is_empty(),
            "Excluded programs should be empty"
        );
    }

    #[test]
    fn test_myapp_initial_state() {
        let app = MyApp::from_database(
            CleanerDatabase::from_vec(vec![]),
            RegistryDatabase::from_vec(vec![]),
            Arc::from(Vec::new()),
        );

        let state = &app.state;
        assert!(
            state.progress_message.is_empty(),
            "Progress message should be empty"
        );
        assert!(
            state.search_query.is_empty(),
            "Search query should be empty"
        );
        // No run in flight, so no result channel is waiting for a result.
        assert!(state.result_receiver.is_none());
        // The window-only plumbing starts empty too.
        assert!(app.changelog.is_none());
        assert!(!app.show_changelog);
        assert!(app.notifications.is_empty());
    }

    #[test]
    fn test_args_parsing() {
        // Test that Args structure can be created
        let args = Args {
            disable_custom: false,
            database_path: Some(String::from("test.json")),
            registry_database_path: Some(String::from("registry_test.json")),
        };

        assert_eq!(args.database_path, Some(String::from("test.json")));
        assert!(!args.disable_custom);
        assert_eq!(
            args.registry_database_path,
            Some(String::from("registry_test.json"))
        );
    }
}
