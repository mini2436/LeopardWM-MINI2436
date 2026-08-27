//! System tray icon management for LeopardWM daemon.
//!
//! Provides a system tray icon with a context menu for common operations:
//! - Pause/resume tiling
//! - Quick toggles for common settings
//! - Configuration access (Settings GUI, Edit Config, Reload)
//! - Troubleshooting (Refresh, View Logs, Release All Windows)
//! - Exit
//!
//! The tray icon and its hidden notification window live on a dedicated thread
//! that runs a Win32 message pump. This is required for the right-click context
//! menu to appear — the `tray-icon` crate needs `WM_RBUTTONUP` and related
//! shell notification messages to be dispatched on the owning thread.

use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    mpsc, Arc, Mutex,
};
use thiserror::Error;
use tracing::{debug, info};
use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
    TrayIconBuilder,
};

/// Minimal Win32 FFI for the tray icon message pump.
///
/// Only the functions needed to run a message loop and signal the thread.
mod win32_msg {
    use std::ffi::c_void;

    #[repr(C)]
    #[allow(clippy::upper_case_acronyms)]
    pub struct POINT {
        pub x: i32,
        pub y: i32,
    }

    #[repr(C)]
    #[allow(clippy::upper_case_acronyms)]
    pub struct MSG {
        pub hwnd: *mut c_void,
        pub message: u32,
        pub wparam: usize,
        pub lparam: isize,
        pub time: u32,
        pub pt: POINT,
    }

    pub const WM_QUIT: u32 = 0x0012;
    pub const PM_NOREMOVE: u32 = 0x0000;
    /// Application-private message: signal the thread to apply a tooltip update.
    pub const WM_APP_UPDATE_TOOLTIP: u32 = 0x8000; // WM_APP
    /// Application-private message: signal the thread to update pause text.
    pub const WM_APP_UPDATE_PAUSE: u32 = 0x8001; // WM_APP + 1
    /// Application-private message: signal the thread to sync quick-toggle check marks.
    pub const WM_APP_UPDATE_TOGGLES: u32 = 0x8002; // WM_APP + 2
    /// Application-private message: signal the thread to refresh the update-status item.
    pub const WM_APP_UPDATE_RELEASE_INFO: u32 = 0x8003; // WM_APP + 3

    extern "system" {
        pub fn GetCurrentThreadId() -> u32;
        pub fn GetMessageW(msg: *mut MSG, hwnd: *mut c_void, min: u32, max: u32) -> i32;
        pub fn TranslateMessage(msg: *const MSG) -> i32;
        pub fn DispatchMessageW(msg: *const MSG) -> isize;
        pub fn PostThreadMessageW(id: u32, msg: u32, wp: usize, lp: isize) -> i32;
        pub fn PeekMessageW(
            msg: *mut MSG,
            hwnd: *mut c_void,
            min: u32,
            max: u32,
            remove: u32,
        ) -> i32;
    }
}

/// Menu item IDs for tray context menu.
mod menu_ids {
    pub const REFRESH: &str = "refresh";
    pub const RELOAD: &str = "reload";
    pub const EXIT: &str = "exit";
    pub const TOGGLE_PAUSE: &str = "toggle_pause";
    pub const OPEN_CONFIG: &str = "open_config";
    pub const OPEN_ABOUT: &str = "open_about";
    pub const EDIT_CONFIG: &str = "edit_config";
    pub const VIEW_LOGS: &str = "view_logs";
    pub const RELEASE_ALL_WINDOWS: &str = "release_all_windows";
    pub const TOGGLE_ACTIVE_BORDER: &str = "toggle_active_border";
    pub const TOGGLE_FOCUS_NEW_WINDOWS: &str = "toggle_focus_new_windows";
    pub const TOGGLE_FOCUS_FOLLOWS_MOUSE: &str = "toggle_focus_follows_mouse";
    pub const TOGGLE_HIDE_OFFSCREEN_TASKBAR: &str = "toggle_hide_offscreen_taskbar";
    pub const TOGGLE_AUTO_START: &str = "toggle_auto_start";
    pub const CENTERING_CENTER: &str = "centering_center";
    pub const CENTERING_JUST_IN_VIEW: &str = "centering_just_in_view";
    pub const CENTERING_ON_OVERFLOW: &str = "centering_on_overflow";
    pub const PLACEMENT_NEW_COLUMN: &str = "placement_new_column";
    pub const PLACEMENT_IN_COLUMN: &str = "placement_in_column";
    pub const CHECK_UPDATES: &str = "check_updates";
}

/// Events emitted by the tray icon.
#[derive(Debug, Clone)]
pub enum TrayEvent {
    /// User clicked "Refresh Windows" menu item.
    Refresh,
    /// User clicked "Reload Config" menu item.
    Reload,
    /// User clicked "Exit" menu item.
    Exit,
    /// User clicked "Pause/Resume Tiling" menu item.
    TogglePause,
    /// User clicked "Settings" menu item.
    OpenConfig,
    /// User clicked the title / "About" menu item.
    OpenAbout,
    /// User clicked "Edit Config" menu item.
    EditConfig,
    /// User clicked "View Logs" menu item.
    ViewLogs,
    /// User clicked "Release All Windows" menu item.
    ReleaseAllWindows,
    /// User toggled "Active Border" check item.
    ToggleActiveBorder,
    /// User toggled "Focus New Windows" check item.
    ToggleFocusNewWindows,
    /// User toggled "Focus Follows Mouse" check item.
    ToggleFocusFollowsMouse,
    /// User toggled "Hide off-screen taskbar buttons" check item.
    ToggleHideOffscreenTaskbar,
    /// User toggled "Start with Windows" check item.
    ToggleAutoStart,
    /// User selected "Center" centering mode.
    SetCenteringCenter,
    /// User selected "Just in View" centering mode.
    SetCenteringJustInView,
    /// User selected "On Overflow" centering mode.
    SetCenteringOnOverflow,
    /// User selected "New Column" new-window placement.
    SetPlacementNewColumn,
    /// User selected "In Focused Column" new-window placement.
    SetPlacementInColumn,
    /// User clicked "Check for Updates" / "Update available" menu item.
    OpenReleasesPage,
}

/// Centering mode values for atomic storage.
pub const CENTERING_CENTER: u8 = 0;
pub const CENTERING_JUST_IN_VIEW: u8 = 1;
pub const CENTERING_ON_OVERFLOW: u8 = 2;
pub const PLACEMENT_NEW_COLUMN: u8 = 0;
pub const PLACEMENT_IN_COLUMN: u8 = 1;

/// Shared state between the caller and the message-loop thread.
///
/// `MenuItem` and `TrayIcon` are `!Send`, so they must stay on the
/// message-loop thread. Updates are communicated via these shared atomics
/// and mutexes, with `PostThreadMessageW` to wake the thread.
struct SharedState {
    use_chinese: AtomicBool,
    active_workspace: AtomicU8,
    paused: AtomicBool,
    tooltip_text: Mutex<String>,
    active_border: AtomicBool,
    focus_new_windows: AtomicBool,
    focus_follows_mouse: AtomicBool,
    hide_offscreen_taskbar: AtomicBool,
    auto_start: AtomicBool,
    centering_mode: AtomicU8,
    placement_mode: AtomicU8,
    /// `Some(tag)` when a newer release has been observed; `None` otherwise.
    available_update: Mutex<Option<String>>,
}

/// Items returned by `build_tray` that the message-loop thread needs to update.
struct TrayItems {
    pause_item: MenuItem,
    active_border_item: CheckMenuItem,
    focus_new_windows_item: CheckMenuItem,
    focus_follows_mouse_item: CheckMenuItem,
    hide_offscreen_taskbar_item: CheckMenuItem,
    auto_start_item: CheckMenuItem,
    centering_center_item: CheckMenuItem,
    centering_just_in_view_item: CheckMenuItem,
    centering_on_overflow_item: CheckMenuItem,
    placement_new_column_item: CheckMenuItem,
    placement_in_column_item: CheckMenuItem,
    update_item: MenuItem,
}

/// Initial state for quick-toggle menu items.
pub struct QuickToggleState {
    pub language: crate::config::Language,
    pub active_border: bool,
    pub focus_new_windows: bool,
    pub focus_follows_mouse: bool,
    pub hide_offscreen_taskbar: bool,
    pub auto_start: bool,
    /// 0 = Center, 1 = JustInView, 2 = OnOverflow
    pub centering_mode: u8,
    /// 0 = NewColumn, 1 = InColumn
    pub placement_mode: u8,
}

fn system_uses_chinese() -> bool {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    // PRIMARYLANGID(langid) == LANG_CHINESE (0x04).
    unsafe { GetUserDefaultUILanguage() & 0x03ff == 0x04 }
}

fn use_chinese(language: crate::config::Language) -> bool {
    match language {
        crate::config::Language::System => system_uses_chinese(),
        crate::config::Language::English => false,
        crate::config::Language::SimplifiedChinese => true,
    }
}

/// Manages the system tray icon and context menu.
///
/// The tray icon and its hidden window live on a dedicated thread that runs a
/// Win32 message pump, which is required for the context menu to appear on
/// right-click.
pub struct TrayManager {
    shared: Arc<SharedState>,
    /// Win32 thread ID of the message-loop thread (for `PostThreadMessageW`).
    msg_thread_id: u32,
    /// Join handle for the message-loop thread.
    msg_thread: Option<std::thread::JoinHandle<()>>,
}

/// Init handshake sent from the message-loop thread back to the caller.
type InitResult = Result<u32, TrayError>;

impl TrayManager {
    /// Create a new tray manager with icon and context menu.
    ///
    /// The provided sender will receive tray events when menu items are clicked.
    /// `initial` sets the starting check state for quick-toggle items.
    pub fn new(
        event_sender: mpsc::Sender<TrayEvent>,
        initial: QuickToggleState,
        initial_workspace: u8,
    ) -> Result<Self, TrayError> {
        let shared = Arc::new(SharedState {
            use_chinese: AtomicBool::new(use_chinese(initial.language)),
            active_workspace: AtomicU8::new(initial_workspace.clamp(1, 9)),
            paused: AtomicBool::new(false),
            tooltip_text: Mutex::new(String::from(if use_chinese(initial.language) {
                "LeopardWM - 平铺窗口管理器"
            } else {
                "LeopardWM - Tiling Window Manager"
            })),
            active_border: AtomicBool::new(initial.active_border),
            focus_new_windows: AtomicBool::new(initial.focus_new_windows),
            focus_follows_mouse: AtomicBool::new(initial.focus_follows_mouse),
            hide_offscreen_taskbar: AtomicBool::new(initial.hide_offscreen_taskbar),
            auto_start: AtomicBool::new(initial.auto_start),
            centering_mode: AtomicU8::new(initial.centering_mode),
            placement_mode: AtomicU8::new(initial.placement_mode),
            available_update: Mutex::new(None),
        });
        let shared_for_thread = shared.clone();
        let (init_tx, init_rx) = mpsc::channel::<InitResult>();

        let thread = std::thread::Builder::new()
            .name("tray-msg-loop".into())
            .spawn(move || {
                run_tray_thread(init_tx, shared_for_thread, initial);
            })
            .map_err(|e| TrayError::Build(format!("Failed to spawn tray thread: {e}")))?;

        // Wait for the message-loop thread to finish building the tray icon.
        let thread_id = init_rx
            .recv()
            .map_err(|_| TrayError::Build("Tray thread exited during init".into()))??;

        // Spawn thread to listen for menu events and forward them.
        let menu_sender = event_sender.clone();
        std::thread::Builder::new()
            .name("tray-menu-events".into())
            .spawn(move || {
                let rx = MenuEvent::receiver();
                while let Ok(event) = rx.recv() {
                    let Some(tray_event) = map_menu_id_to_event(event.id.0.as_str()) else {
                        debug!("Unknown menu item clicked: {}", event.id.0);
                        continue;
                    };
                    if menu_sender.send(tray_event).is_err() {
                        break;
                    }
                }
            })
            .ok();

        // Spawn thread for tray-icon clicks: double-click opens Settings
        // (single/right click keep showing the context menu).
        // TrayIconEvent::receiver() is one process-global stream; this must
        // stay the only consumer or readers would compete for events.
        std::thread::Builder::new()
            .name("tray-icon-events".into())
            .spawn(move || {
                let rx = tray_icon::TrayIconEvent::receiver();
                while let Ok(event) = rx.recv() {
                    if matches!(event, tray_icon::TrayIconEvent::DoubleClick { .. })
                        && event_sender.send(TrayEvent::OpenConfig).is_err()
                    {
                        break;
                    }
                }
            })
            .ok();

        info!("System tray icon created");

        Ok(Self {
            shared,
            msg_thread_id: thread_id,
            msg_thread: Some(thread),
        })
    }

    /// Update the pause menu item text based on the current paused state.
    pub fn update_pause_text(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
        unsafe {
            win32_msg::PostThreadMessageW(self.msg_thread_id, win32_msg::WM_APP_UPDATE_PAUSE, 0, 0);
        }
    }

    /// Sync quick-toggle check marks with the current config state.
    pub fn update_quick_toggles(&self, toggles: &QuickToggleState) {
        self.shared
            .use_chinese
            .store(use_chinese(toggles.language), Ordering::Relaxed);
        self.shared
            .active_border
            .store(toggles.active_border, Ordering::Relaxed);
        self.shared
            .focus_new_windows
            .store(toggles.focus_new_windows, Ordering::Relaxed);
        self.shared
            .focus_follows_mouse
            .store(toggles.focus_follows_mouse, Ordering::Relaxed);
        self.shared
            .hide_offscreen_taskbar
            .store(toggles.hide_offscreen_taskbar, Ordering::Relaxed);
        self.shared
            .auto_start
            .store(toggles.auto_start, Ordering::Relaxed);
        self.shared
            .centering_mode
            .store(toggles.centering_mode, Ordering::Relaxed);
        self.shared
            .placement_mode
            .store(toggles.placement_mode, Ordering::Relaxed);
        unsafe {
            win32_msg::PostThreadMessageW(
                self.msg_thread_id,
                win32_msg::WM_APP_UPDATE_TOGGLES,
                0,
                0,
            );
        }
    }

    /// Update the tray tooltip to reflect current state.
    ///
    /// If `hotkey_mismatch` is provided as `Some((registered, requested))` and
    /// registered < requested, a warning is appended to the tooltip.
    pub fn update_tooltip(
        &self,
        window_count: usize,
        monitor_count: usize,
        paused: bool,
        hotkey_mismatch: Option<(usize, usize)>,
        active_workspace: u8,
    ) {
        let tooltip = format_tooltip_text(
            window_count,
            monitor_count,
            paused,
            hotkey_mismatch,
            active_workspace,
        );
        let tooltip = if self.shared.use_chinese.load(Ordering::Relaxed) {
            format_tooltip_text_zh(
                window_count,
                monitor_count,
                paused,
                hotkey_mismatch,
                active_workspace,
            )
        } else {
            tooltip
        };
        if let Ok(mut text) = self.shared.tooltip_text.lock() {
            *text = tooltip;
        }
        self.shared
            .active_workspace
            .store(active_workspace.clamp(1, 9), Ordering::Relaxed);
        // Wake the message-loop thread to apply the new tooltip.
        unsafe {
            win32_msg::PostThreadMessageW(
                self.msg_thread_id,
                win32_msg::WM_APP_UPDATE_TOOLTIP,
                0,
                0,
            );
        }
    }

    /// Record the latest available release tag (e.g. `v0.1.11`) and refresh
    /// the tray's update menu item label. Pass `None` to reset to the default
    /// "Check for Updates" label.
    pub fn set_available_update(&self, tag: Option<String>) {
        if let Ok(mut g) = self.shared.available_update.lock() {
            *g = tag;
        }
        unsafe {
            win32_msg::PostThreadMessageW(
                self.msg_thread_id,
                win32_msg::WM_APP_UPDATE_RELEASE_INFO,
                0,
                0,
            );
        }
    }
}

impl Drop for TrayManager {
    fn drop(&mut self) {
        // Signal the message loop to exit. WM_QUIT causes GetMessageW to return 0,
        // which breaks the loop and lets TrayIcon drop on its creating thread.
        unsafe {
            win32_msg::PostThreadMessageW(self.msg_thread_id, win32_msg::WM_QUIT, 0, 0);
        }
        if let Some(handle) = self.msg_thread.take() {
            let _ = handle.join();
        }
    }
}

/// Enable dark mode for native Win32 context menus.
///
/// Calls undocumented but stable `uxtheme.dll` ordinals (135, 136) used by
/// Windows Terminal, VS Code, Notepad, etc. Requires Windows 10 1903+.
/// Silently no-ops on older Windows versions.
fn enable_dark_mode_menus() {
    extern "system" {
        fn LoadLibraryW(name: *const u16) -> isize;
        fn GetProcAddress(
            module: isize,
            name: *const u8,
        ) -> Option<unsafe extern "system" fn() -> isize>;
        fn FreeLibrary(module: isize) -> i32;
    }

    const ALLOW_DARK: i32 = 1; // PreferredAppMode::AllowDark — follows system theme

    unsafe {
        let lib: Vec<u16> = "uxtheme.dll\0".encode_utf16().collect();
        let hmodule = LoadLibraryW(lib.as_ptr());
        if hmodule == 0 {
            return;
        }

        // Ordinal 135: SetPreferredAppMode(AllowDark)
        // Tells Windows to use dark theme for native controls when the system is in dark mode.
        if let Some(f) = GetProcAddress(hmodule, 135usize as *const u8) {
            let set_preferred_app_mode: unsafe extern "system" fn(i32) -> i32 =
                std::mem::transmute(f);
            set_preferred_app_mode(ALLOW_DARK);
        }

        // Ordinal 136: FlushMenuThemes()
        // Discards cached menu theme so the new preference takes effect immediately.
        if let Some(f) = GetProcAddress(hmodule, 136usize as *const u8) {
            let flush_menu_themes: unsafe extern "system" fn() = std::mem::transmute(f);
            flush_menu_themes();
        }

        FreeLibrary(hmodule);
    }
}

/// Runs on the dedicated tray thread: builds the tray icon and pumps messages.
fn run_tray_thread(
    init_tx: mpsc::Sender<InitResult>,
    shared: Arc<SharedState>,
    initial: QuickToggleState,
) {
    let zh = use_chinese(initial.language);
    let thread_id = unsafe { win32_msg::GetCurrentThreadId() };

    // Enable dark mode for native context menus before any menu is created.
    enable_dark_mode_menus();

    let initial_workspace = shared.active_workspace.load(Ordering::Relaxed);
    let (tray, items) = match build_tray(&initial, initial_workspace) {
        Ok(v) => v,
        Err(e) => {
            let _ = init_tx.send(Err(e));
            return;
        }
    };

    // Ensure the thread has a message queue before signaling init complete.
    // PeekMessageW creates the queue as a side effect, so subsequent
    // PostThreadMessageW calls from the caller won't be lost.
    let mut rendered_workspace = initial_workspace;
    unsafe {
        let mut msg = std::mem::zeroed::<win32_msg::MSG>();
        win32_msg::PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, win32_msg::PM_NOREMOVE);
    }

    if init_tx.send(Ok(thread_id)).is_err() {
        return; // Caller dropped the receiver.
    }

    // Win32 message loop — pumps messages for the hidden tray-icon window.
    unsafe {
        let mut msg = std::mem::zeroed::<win32_msg::MSG>();
        loop {
            let ret = win32_msg::GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
            if ret <= 0 {
                break; // WM_QUIT (0) or error (-1).
            }

            // Thread messages (hwnd == NULL) carry our custom update signals.
            if msg.hwnd.is_null() {
                match msg.message {
                    win32_msg::WM_APP_UPDATE_TOOLTIP => {
                        if let Ok(text) = shared.tooltip_text.lock() {
                            let _ = tray.set_tooltip(Some(text.as_str()));
                        }
                        let workspace = shared.active_workspace.load(Ordering::Relaxed);
                        if workspace != rendered_workspace {
                            match create_workspace_icon(workspace) {
                                Ok(icon) => {
                                    if tray.set_icon(Some(icon)).is_ok() {
                                        rendered_workspace = workspace;
                                    }
                                }
                                Err(error) => {
                                    tracing::warn!(
                                        workspace,
                                        %error,
                                        "Failed to update dynamic workspace tray icon"
                                    );
                                }
                            }
                        }
                        continue;
                    }
                    win32_msg::WM_APP_UPDATE_PAUSE => {
                        let paused = shared.paused.load(Ordering::Relaxed);
                        let label = if paused {
                            if zh {
                                "恢复平铺\tCtrl+Alt+P"
                            } else {
                                "Resume Tiling\tCtrl+Alt+P"
                            }
                        } else {
                            if zh {
                                "暂停平铺\tCtrl+Alt+P"
                            } else {
                                "Pause Tiling\tCtrl+Alt+P"
                            }
                        };
                        items.pause_item.set_text(label);
                        continue;
                    }
                    win32_msg::WM_APP_UPDATE_TOGGLES => {
                        items
                            .active_border_item
                            .set_checked(shared.active_border.load(Ordering::Relaxed));
                        items
                            .focus_new_windows_item
                            .set_checked(shared.focus_new_windows.load(Ordering::Relaxed));
                        items
                            .focus_follows_mouse_item
                            .set_checked(shared.focus_follows_mouse.load(Ordering::Relaxed));
                        items
                            .hide_offscreen_taskbar_item
                            .set_checked(shared.hide_offscreen_taskbar.load(Ordering::Relaxed));
                        items
                            .auto_start_item
                            .set_checked(shared.auto_start.load(Ordering::Relaxed));
                        let cm = shared.centering_mode.load(Ordering::Relaxed);
                        items
                            .centering_center_item
                            .set_checked(cm == CENTERING_CENTER);
                        items
                            .centering_just_in_view_item
                            .set_checked(cm == CENTERING_JUST_IN_VIEW);
                        items
                            .centering_on_overflow_item
                            .set_checked(cm == CENTERING_ON_OVERFLOW);
                        let pm = shared.placement_mode.load(Ordering::Relaxed);
                        items
                            .placement_new_column_item
                            .set_checked(pm == PLACEMENT_NEW_COLUMN);
                        items
                            .placement_in_column_item
                            .set_checked(pm == PLACEMENT_IN_COLUMN);
                        continue;
                    }
                    win32_msg::WM_APP_UPDATE_RELEASE_INFO => {
                        let label = match shared.available_update.lock() {
                            Ok(g) => match g.as_ref() {
                                Some(tag) if zh => format!("有可用更新：{tag}"),
                                Some(tag) => format!("Update available: {tag}"),
                                None if zh => "检查更新".to_string(),
                                None => "Check for Updates".to_string(),
                            },
                            Err(_) if zh => "检查更新".to_string(),
                            Err(_) => "Check for Updates".to_string(),
                        };
                        items.update_item.set_text(label);
                        continue;
                    }
                    _ => {}
                }
            }

            win32_msg::TranslateMessage(&msg);
            win32_msg::DispatchMessageW(&msg);
        }
    }
    // `tray` and items are dropped here — on the same thread that created them.
}

/// Build the tray icon with its context menu. Called on the message-loop
/// thread so the hidden notification window belongs to that thread.
fn build_tray(
    initial: &QuickToggleState,
    initial_workspace: u8,
) -> Result<(tray_icon::TrayIcon, TrayItems), TrayError> {
    let zh = use_chinese(initial.language);
    let tr = |en: &'static str, cn: &'static str| if zh { cn } else { en };
    let menu = Menu::new();
    let append = |item: &dyn tray_icon::menu::IsMenuItem| -> Result<(), TrayError> {
        menu.append(item)
            .map_err(|e| TrayError::Menu(e.to_string()))
    };

    // Title item (clickable — opens About section in Settings)
    let version = env!("CARGO_PKG_VERSION");
    append(&MenuItem::with_id(
        menu_ids::OPEN_ABOUT,
        format!("LeopardWM v{version}"),
        true,
        None,
    ))?;
    append(&PredefinedMenuItem::separator())?;

    // Toggle Pause (first — most time-sensitive action)
    let toggle_pause = MenuItem::with_id(
        menu_ids::TOGGLE_PAUSE,
        tr("Pause Tiling\tCtrl+Alt+P", "暂停平铺\tCtrl+Alt+P"),
        true,
        None,
    );
    append(&toggle_pause)?;
    append(&PredefinedMenuItem::separator())?;

    // Quick toggles
    let active_border_item = CheckMenuItem::with_id(
        menu_ids::TOGGLE_ACTIVE_BORDER,
        tr("Active Border", "活动窗口边框"),
        true,
        initial.active_border,
        None,
    );
    append(&active_border_item)?;

    let focus_new_windows_item = CheckMenuItem::with_id(
        menu_ids::TOGGLE_FOCUS_NEW_WINDOWS,
        tr("Focus New Windows", "聚焦新窗口"),
        true,
        initial.focus_new_windows,
        None,
    );
    append(&focus_new_windows_item)?;

    let focus_follows_mouse_item = CheckMenuItem::with_id(
        menu_ids::TOGGLE_FOCUS_FOLLOWS_MOUSE,
        tr("Focus Follows Mouse", "焦点跟随鼠标"),
        true,
        initial.focus_follows_mouse,
        None,
    );
    append(&focus_follows_mouse_item)?;

    let hide_offscreen_taskbar_item = CheckMenuItem::with_id(
        menu_ids::TOGGLE_HIDE_OFFSCREEN_TASKBAR,
        tr(
            "Hide Off-Screen Taskbar Buttons",
            "隐藏屏幕外窗口的任务栏按钮",
        ),
        true,
        initial.hide_offscreen_taskbar,
        None,
    );
    append(&hide_offscreen_taskbar_item)?;

    let auto_start_item = CheckMenuItem::with_id(
        menu_ids::TOGGLE_AUTO_START,
        tr("Start with Windows", "随 Windows 启动"),
        true,
        initial.auto_start,
        None,
    );
    append(&auto_start_item)?;

    // Centering Mode submenu
    let centering_sub = Submenu::new(tr("Centering Mode", "居中模式"), true);
    let centering_center_item = CheckMenuItem::with_id(
        menu_ids::CENTERING_CENTER,
        tr("Center", "居中"),
        true,
        initial.centering_mode == CENTERING_CENTER,
        None,
    );
    let centering_just_in_view_item = CheckMenuItem::with_id(
        menu_ids::CENTERING_JUST_IN_VIEW,
        tr("Just in View", "仅保持可见"),
        true,
        initial.centering_mode == CENTERING_JUST_IN_VIEW,
        None,
    );
    let centering_on_overflow_item = CheckMenuItem::with_id(
        menu_ids::CENTERING_ON_OVERFLOW,
        tr("On Overflow", "溢出时居中"),
        true,
        initial.centering_mode == CENTERING_ON_OVERFLOW,
        None,
    );
    centering_sub
        .append(&centering_center_item)
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    centering_sub
        .append(&centering_just_in_view_item)
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    centering_sub
        .append(&centering_on_overflow_item)
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    append(&centering_sub)?;

    // New Window Placement submenu
    let placement_sub = Submenu::new(tr("New Window Placement", "新窗口位置"), true);
    let placement_new_column_item = CheckMenuItem::with_id(
        menu_ids::PLACEMENT_NEW_COLUMN,
        tr("New Column", "新列"),
        true,
        initial.placement_mode == PLACEMENT_NEW_COLUMN,
        None,
    );
    let placement_in_column_item = CheckMenuItem::with_id(
        menu_ids::PLACEMENT_IN_COLUMN,
        tr("In Focused Column", "放入焦点列"),
        true,
        initial.placement_mode == PLACEMENT_IN_COLUMN,
        None,
    );
    placement_sub
        .append(&placement_new_column_item)
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    placement_sub
        .append(&placement_in_column_item)
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    append(&placement_sub)?;
    append(&PredefinedMenuItem::separator())?;

    // Update checker — relabels itself when a newer release is detected.
    let update_item = MenuItem::with_id(
        menu_ids::CHECK_UPDATES,
        tr("Check for Updates", "检查更新"),
        true,
        None,
    );
    append(&update_item)?;
    append(&PredefinedMenuItem::separator())?;

    // Configuration group
    append(&MenuItem::with_id(
        menu_ids::OPEN_CONFIG,
        tr("Settings...", "设置..."),
        true,
        None,
    ))?;
    append(&MenuItem::with_id(
        menu_ids::EDIT_CONFIG,
        tr("Edit Config", "编辑配置"),
        true,
        None,
    ))?;
    append(&MenuItem::with_id(
        menu_ids::RELOAD,
        tr(
            "Reload Config\tCtrl+Alt+Shift+R",
            "重新加载配置\tCtrl+Alt+Shift+R",
        ),
        true,
        None,
    ))?;
    append(&PredefinedMenuItem::separator())?;

    // Troubleshooting submenu
    let troubleshoot = Submenu::new(tr("Troubleshooting", "故障排除"), true);
    troubleshoot
        .append(&MenuItem::with_id(
            menu_ids::REFRESH,
            tr("Refresh Windows\tCtrl+Alt+R", "刷新窗口\tCtrl+Alt+R"),
            true,
            None,
        ))
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    troubleshoot
        .append(&MenuItem::with_id(
            menu_ids::VIEW_LOGS,
            tr("View Logs", "查看日志"),
            true,
            None,
        ))
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    troubleshoot
        .append(&MenuItem::with_id(
            menu_ids::RELEASE_ALL_WINDOWS,
            tr("Release All Windows", "释放所有窗口"),
            true,
            None,
        ))
        .map_err(|e| TrayError::Menu(e.to_string()))?;
    append(&troubleshoot)?;
    append(&PredefinedMenuItem::separator())?;

    // Exit
    append(&MenuItem::with_id(
        menu_ids::EXIT,
        tr("Exit", "退出"),
        true,
        None,
    ))?;

    // Create the tray icon with a simple embedded icon
    let icon = create_workspace_icon(initial_workspace).or_else(|error| {
        tracing::warn!(%error, "Falling back to the static LeopardWM tray icon");
        create_default_icon()
    })?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(tr(
            "LeopardWM - Tiling Window Manager",
            "LeopardWM - 平铺窗口管理器",
        ))
        .with_icon(icon)
        .build()
        .map_err(|e| TrayError::Build(e.to_string()))?;

    let items = TrayItems {
        pause_item: toggle_pause,
        active_border_item,
        focus_new_windows_item,
        focus_follows_mouse_item,
        hide_offscreen_taskbar_item,
        auto_start_item,
        centering_center_item,
        centering_just_in_view_item,
        centering_on_overflow_item,
        placement_new_column_item,
        placement_in_column_item,
        update_item,
    };

    Ok((tray, items))
}

fn map_menu_id_to_event(menu_id: &str) -> Option<TrayEvent> {
    match menu_id {
        menu_ids::REFRESH => Some(TrayEvent::Refresh),
        menu_ids::RELOAD => Some(TrayEvent::Reload),
        menu_ids::EXIT => Some(TrayEvent::Exit),
        menu_ids::TOGGLE_PAUSE => Some(TrayEvent::TogglePause),
        menu_ids::OPEN_CONFIG => Some(TrayEvent::OpenConfig),
        menu_ids::OPEN_ABOUT => Some(TrayEvent::OpenAbout),
        menu_ids::EDIT_CONFIG => Some(TrayEvent::EditConfig),
        menu_ids::VIEW_LOGS => Some(TrayEvent::ViewLogs),
        menu_ids::RELEASE_ALL_WINDOWS => Some(TrayEvent::ReleaseAllWindows),
        menu_ids::TOGGLE_ACTIVE_BORDER => Some(TrayEvent::ToggleActiveBorder),
        menu_ids::TOGGLE_FOCUS_NEW_WINDOWS => Some(TrayEvent::ToggleFocusNewWindows),
        menu_ids::TOGGLE_FOCUS_FOLLOWS_MOUSE => Some(TrayEvent::ToggleFocusFollowsMouse),
        menu_ids::TOGGLE_HIDE_OFFSCREEN_TASKBAR => Some(TrayEvent::ToggleHideOffscreenTaskbar),
        menu_ids::TOGGLE_AUTO_START => Some(TrayEvent::ToggleAutoStart),
        menu_ids::CENTERING_CENTER => Some(TrayEvent::SetCenteringCenter),
        menu_ids::CENTERING_JUST_IN_VIEW => Some(TrayEvent::SetCenteringJustInView),
        menu_ids::CENTERING_ON_OVERFLOW => Some(TrayEvent::SetCenteringOnOverflow),
        menu_ids::PLACEMENT_NEW_COLUMN => Some(TrayEvent::SetPlacementNewColumn),
        menu_ids::PLACEMENT_IN_COLUMN => Some(TrayEvent::SetPlacementInColumn),
        menu_ids::CHECK_UPDATES => Some(TrayEvent::OpenReleasesPage),
        _ => None,
    }
}

/// Format the tray tooltip text (testable without requiring a real tray icon).
pub fn format_tooltip_text(
    window_count: usize,
    monitor_count: usize,
    paused: bool,
    hotkey_mismatch: Option<(usize, usize)>,
    active_workspace: u8,
) -> String {
    let status = if paused { "Paused" } else { "Active" };
    let mut tooltip = format!(
        "LeopardWM - {} (WS {}, {} windows, {} monitors)",
        status, active_workspace, window_count, monitor_count
    );
    if let Some((registered, requested)) = hotkey_mismatch {
        if registered < requested {
            tooltip.push_str(&format!(
                "\nHotkeys: {}/{} ({} failed)",
                registered,
                requested,
                requested - registered
            ));
        }
    }
    tooltip
}

fn format_tooltip_text_zh(
    window_count: usize,
    monitor_count: usize,
    paused: bool,
    hotkey_mismatch: Option<(usize, usize)>,
    active_workspace: u8,
) -> String {
    let status = if paused { "已暂停" } else { "运行中" };
    let mut tooltip = format!(
        "LeopardWM - {}（工作区 {}，{} 个窗口，{} 台显示器）",
        status, active_workspace, window_count, monitor_count
    );
    if let Some((registered, requested)) = hotkey_mismatch {
        if registered < requested {
            tooltip.push_str(&format!(
                "\n快捷键：{}/{}（{} 个注册失败）",
                registered,
                requested,
                requested - registered
            ));
        }
    }
    tooltip
}

/// Create the tray icon from the embedded 32x32 PNG.
fn create_default_icon() -> Result<tray_icon::Icon, TrayError> {
    let png_bytes = include_bytes!("../../../assets/icon-32.png");
    let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    let mut reader = decoder
        .read_info()
        .map_err(|e| TrayError::Icon(format!("PNG decode error: {e}")))?;
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| TrayError::Icon(format!("PNG frame error: {e}")))?;
    buf.truncate(info.buffer_size());

    // Convert RGB to RGBA if needed
    let rgba = if info.color_type == png::ColorType::Rgb {
        let mut out = Vec::with_capacity((info.width * info.height * 4) as usize);
        for chunk in buf.chunks(3) {
            out.extend_from_slice(chunk);
            out.push(255);
        }
        out
    } else {
        buf
    };

    tray_icon::Icon::from_rgba(rgba, info.width, info.height)
        .map_err(|e| TrayError::Icon(e.to_string()))
}

const WORKSPACE_ICON_SIZE: usize = 32;

/// Five-by-seven bitmap glyphs for 1-9. Rendering these ourselves keeps the
/// dynamic icon deterministic and avoids depending on a particular Windows
/// font having the Unicode circled-number glyphs installed.
const WORKSPACE_DIGITS: [[u8; 7]; 9] = [
    [
        0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
    ],
    [
        0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
    ],
    [
        0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
    ],
    [
        0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
    ],
    [
        0b11111, 0b10000, 0b10000, 0b11110, 0b00001, 0b00001, 0b11110,
    ],
    [
        0b01110, 0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
    ],
    [
        0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
    ],
    [
        0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
    ],
    [
        0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110,
    ],
];

fn workspace_icon_rgba(workspace: u8) -> Result<Vec<u8>, TrayError> {
    if !(1..=9).contains(&workspace) {
        return Err(TrayError::Icon(format!(
            "workspace icon index must be 1-9, got {workspace}"
        )));
    }

    let mut rgba = vec![0u8; WORKSPACE_ICON_SIZE * WORKSPACE_ICON_SIZE * 4];
    let center = (WORKSPACE_ICON_SIZE as f32 - 1.0) / 2.0;
    let outer_radius = 14.5f32;
    let inner_radius = 12.3f32;

    for y in 0..WORKSPACE_ICON_SIZE {
        for x in 0..WORKSPACE_ICON_SIZE {
            // Four-by-four supersampling gives the circular edge a stable,
            // antialiased outline at the small sizes used by the Windows tray.
            let mut disk_samples = 0u16;
            let mut ring_samples = 0u16;
            for sy in 0..4 {
                for sx in 0..4 {
                    let px = x as f32 + (sx as f32 + 0.5) / 4.0;
                    let py = y as f32 + (sy as f32 + 0.5) / 4.0;
                    let dx = px - (center + 0.5);
                    let dy = py - (center + 0.5);
                    let distance = (dx * dx + dy * dy).sqrt();
                    if distance <= outer_radius {
                        disk_samples += 1;
                    }
                    if distance > inner_radius && distance <= outer_radius {
                        ring_samples += 1;
                    }
                }
            }
            if disk_samples == 0 {
                continue;
            }
            let offset = (y * WORKSPACE_ICON_SIZE + x) * 4;
            let ring_mix = ring_samples as f32 / disk_samples as f32;
            // Windows accent-blue disc with a bright outer ring. It remains
            // legible on both light and dark taskbars.
            rgba[offset] = (0.0 * (1.0 - ring_mix) + 225.0 * ring_mix) as u8;
            rgba[offset + 1] = (120.0 * (1.0 - ring_mix) + 245.0 * ring_mix) as u8;
            rgba[offset + 2] = (212.0 * (1.0 - ring_mix) + 255.0 * ring_mix) as u8;
            rgba[offset + 3] = ((disk_samples as f32 / 16.0) * 255.0).round() as u8;
        }
    }

    let glyph = WORKSPACE_DIGITS[(workspace - 1) as usize];
    let scale = 3usize;
    let glyph_width = 5 * scale;
    let glyph_height = 7 * scale;
    let origin_x = (WORKSPACE_ICON_SIZE - glyph_width) / 2;
    let origin_y = (WORKSPACE_ICON_SIZE - glyph_height) / 2;
    for (row, bits) in glyph.iter().enumerate() {
        for column in 0..5 {
            if bits & (1 << (4 - column)) == 0 {
                continue;
            }
            for dy in 0..scale {
                for dx in 0..scale {
                    let x = origin_x + column * scale + dx;
                    let y = origin_y + row * scale + dy;
                    let offset = (y * WORKSPACE_ICON_SIZE + x) * 4;
                    rgba[offset..offset + 4].copy_from_slice(&[255, 255, 255, 255]);
                }
            }
        }
    }

    Ok(rgba)
}

fn create_workspace_icon(workspace: u8) -> Result<tray_icon::Icon, TrayError> {
    tray_icon::Icon::from_rgba(
        workspace_icon_rgba(workspace)?,
        WORKSPACE_ICON_SIZE as u32,
        WORKSPACE_ICON_SIZE as u32,
    )
    .map_err(|error| TrayError::Icon(error.to_string()))
}

/// Errors that can occur during tray operations.
#[derive(Debug, Error)]
pub enum TrayError {
    #[error("Failed to create menu: {0}")]
    Menu(String),

    #[error("Failed to build tray icon: {0}")]
    Build(String),

    #[error("Failed to create icon: {0}")]
    Icon(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_icons_are_valid_distinct_rgba_images() {
        let icons: Vec<Vec<u8>> = (1..=9)
            .map(|workspace| workspace_icon_rgba(workspace).expect("valid workspace icon"))
            .collect();

        for icon in &icons {
            assert_eq!(icon.len(), WORKSPACE_ICON_SIZE * WORKSPACE_ICON_SIZE * 4);
            assert_eq!(icon[3], 0, "top-left corner must be transparent");
            let top_right_alpha = (WORKSPACE_ICON_SIZE - 1) * 4 + 3;
            assert_eq!(icon[top_right_alpha], 0, "top-right must be transparent");
            let center =
                (WORKSPACE_ICON_SIZE / 2 * WORKSPACE_ICON_SIZE + WORKSPACE_ICON_SIZE / 2) * 4;
            assert_eq!(icon[center + 3], 255, "icon center must be opaque");
        }

        for (index, icon) in icons.iter().enumerate() {
            assert!(
                icons.iter().skip(index + 1).all(|other| other != icon),
                "workspace digit images must be distinct"
            );
        }
        assert!(workspace_icon_rgba(0).is_err());
        assert!(workspace_icon_rgba(10).is_err());
    }

    #[test]
    fn test_create_default_icon() {
        let icon = create_default_icon();
        assert!(icon.is_ok(), "Should create default icon successfully");
    }

    #[test]
    fn test_tray_event_toggle_pause_variant() {
        let event = TrayEvent::TogglePause;
        assert!(matches!(event, TrayEvent::TogglePause));
    }

    #[test]
    fn test_tray_event_release_all_windows_variant() {
        let event = TrayEvent::ReleaseAllWindows;
        assert!(matches!(event, TrayEvent::ReleaseAllWindows));
    }

    #[test]
    fn test_tray_event_quick_toggle_variants() {
        assert!(matches!(
            TrayEvent::ToggleActiveBorder,
            TrayEvent::ToggleActiveBorder
        ));
        assert!(matches!(
            TrayEvent::ToggleFocusNewWindows,
            TrayEvent::ToggleFocusNewWindows
        ));
        assert!(matches!(
            TrayEvent::ToggleFocusFollowsMouse,
            TrayEvent::ToggleFocusFollowsMouse
        ));
        assert!(matches!(
            TrayEvent::SetCenteringCenter,
            TrayEvent::SetCenteringCenter
        ));
        assert!(matches!(
            TrayEvent::SetCenteringJustInView,
            TrayEvent::SetCenteringJustInView
        ));
    }

    #[test]
    fn test_map_menu_id_to_event() {
        assert!(matches!(
            map_menu_id_to_event(menu_ids::REFRESH),
            Some(TrayEvent::Refresh)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::RELOAD),
            Some(TrayEvent::Reload)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::EXIT),
            Some(TrayEvent::Exit)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::TOGGLE_PAUSE),
            Some(TrayEvent::TogglePause)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::OPEN_CONFIG),
            Some(TrayEvent::OpenConfig)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::OPEN_ABOUT),
            Some(TrayEvent::OpenAbout)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::EDIT_CONFIG),
            Some(TrayEvent::EditConfig)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::VIEW_LOGS),
            Some(TrayEvent::ViewLogs)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::RELEASE_ALL_WINDOWS),
            Some(TrayEvent::ReleaseAllWindows)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::TOGGLE_ACTIVE_BORDER),
            Some(TrayEvent::ToggleActiveBorder)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::TOGGLE_FOCUS_NEW_WINDOWS),
            Some(TrayEvent::ToggleFocusNewWindows)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::TOGGLE_FOCUS_FOLLOWS_MOUSE),
            Some(TrayEvent::ToggleFocusFollowsMouse)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::CENTERING_CENTER),
            Some(TrayEvent::SetCenteringCenter)
        ));
        assert!(matches!(
            map_menu_id_to_event(menu_ids::CENTERING_JUST_IN_VIEW),
            Some(TrayEvent::SetCenteringJustInView)
        ));
        assert!(map_menu_id_to_event("unknown").is_none());
    }

    #[test]
    fn test_tooltip_format() {
        let active = format_tooltip_text(14, 2, false, None, 1);
        assert_eq!(active, "LeopardWM - Active (WS 1, 14 windows, 2 monitors)");

        let paused = format_tooltip_text(3, 1, true, None, 1);
        assert_eq!(paused, "LeopardWM - Paused (WS 1, 3 windows, 1 monitors)");
    }

    #[test]
    fn test_tooltip_format_with_hotkey_mismatch() {
        let tooltip = format_tooltip_text(10, 2, false, Some((7, 10)), 1);
        assert_eq!(
            tooltip,
            "LeopardWM - Active (WS 1, 10 windows, 2 monitors)\nHotkeys: 7/10 (3 failed)"
        );
    }

    #[test]
    fn test_tooltip_format_no_hotkey_mismatch() {
        // When registered == requested, no mismatch line
        let tooltip = format_tooltip_text(10, 2, false, Some((10, 10)), 1);
        assert_eq!(tooltip, "LeopardWM - Active (WS 1, 10 windows, 2 monitors)");
    }

    #[test]
    fn test_tooltip_format_paused_with_mismatch() {
        let tooltip = format_tooltip_text(5, 1, true, Some((3, 8)), 1);
        assert!(tooltip.contains("Paused"));
        assert!(tooltip.contains("3/8 (5 failed)"));
    }

    #[test]
    fn test_menu_ids_constants() {
        // Ensure menu IDs are distinct
        let ids = [
            menu_ids::REFRESH,
            menu_ids::RELOAD,
            menu_ids::EXIT,
            menu_ids::TOGGLE_PAUSE,
            menu_ids::OPEN_CONFIG,
            menu_ids::OPEN_ABOUT,
            menu_ids::EDIT_CONFIG,
            menu_ids::VIEW_LOGS,
            menu_ids::RELEASE_ALL_WINDOWS,
            menu_ids::TOGGLE_ACTIVE_BORDER,
            menu_ids::TOGGLE_FOCUS_NEW_WINDOWS,
            menu_ids::TOGGLE_FOCUS_FOLLOWS_MOUSE,
            menu_ids::CENTERING_CENTER,
            menu_ids::CENTERING_JUST_IN_VIEW,
        ];
        for (i, a) in ids.iter().enumerate() {
            for (j, b) in ids.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "Menu IDs must be distinct");
                }
            }
        }
    }
}
