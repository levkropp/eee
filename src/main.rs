#![windows_subsystem = "windows"]

//! eee — Hold Ctrl+Alt+Shift+E for 10 seconds to restart explorer.exe.
//!
//! Usage:
//!   eee              Run in foreground (normal operation)
//!   eee install      Copy to %LOCALAPPDATA%\eee, create startup scheduled task, start
//!   eee uninstall    Stop, remove scheduled task, delete installed files

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_MENU, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

const TIMER_ID: usize = 1;
const POLL_MS: u32 = 50;
const HOLD_SECONDS: f64 = 10.0;

const WND_WIDTH: i32 = 380;
const WND_HEIGHT: i32 = 140;

const VK_E: i32 = 0x45;

// Windows 11 Fluent-style colors (BGR format)
const BG_COLOR: COLORREF = COLORREF(0x00FFFFFF);        // white
const BORDER_COLOR: COLORREF = COLORREF(0x00E0E0E0);    // light gray border
const BAR_BG_COLOR: COLORREF = COLORREF(0x00EDEDED);    // light gray track
const FILL_COLOR: COLORREF = COLORREF(0x00D77800);      // Windows accent blue (#0078D7 in BGR)
const TEXT_COL: COLORREF = COLORREF(0x00403A36);         // near-black text (#363A40 in BGR)
const SUBTEXT_COL: COLORREF = COLORREF(0x00767676);      // gray subtitle

const MUTEX_NAME: &str = "Global\\eee_explorer_restart_singleton";
const TASK_NAME: &str = "eee";

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------------------
// Install / Uninstall
// ---------------------------------------------------------------------------

fn install_dir() -> std::path::PathBuf {
    let local_app_data =
        std::env::var("LOCALAPPDATA").unwrap_or_else(|_| r"C:\Users\Med\AppData\Local".into());
    std::path::PathBuf::from(local_app_data).join("eee")
}

fn is_running_from_install_dir() -> bool {
    let src_exe = std::env::current_exe().unwrap_or_default();
    let dest_dir = install_dir();
    let src_canonical = std::fs::canonicalize(&src_exe).unwrap_or(src_exe);
    let dest_canonical = std::fs::canonicalize(&dest_dir).unwrap_or(dest_dir);
    src_canonical.starts_with(&dest_canonical)
}

fn register_uninstall_entry(exe_path: &str) {
    let key_path = to_wide(r"Software\Microsoft\Windows\CurrentVersion\Uninstall\eee");
    let mut hkey = HKEY::default();

    let result = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_path.as_ptr()),
            Some(0),
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
    };

    if result.is_err() {
        return;
    }

    let set_str = |name: &str, value: &str| {
        let wide_name = to_wide(name);
        let wide_value = to_wide(value);
        let byte_len = (wide_value.len() * 2) as u32;
        unsafe {
            let _ = RegSetValueExW(
                hkey,
                PCWSTR(wide_name.as_ptr()),
                Some(0),
                REG_SZ,
                Some(std::slice::from_raw_parts(
                    wide_value.as_ptr() as *const u8,
                    byte_len as usize,
                )),
            );
        }
    };

    let set_dword = |name: &str, value: u32| {
        let wide_name = to_wide(name);
        unsafe {
            let _ = RegSetValueExW(
                hkey,
                PCWSTR(wide_name.as_ptr()),
                Some(0),
                REG_DWORD,
                Some(&value.to_le_bytes()),
            );
        }
    };

    set_str("DisplayName", "eee");
    set_str("Publisher", "Lev Kropp");
    set_str("DisplayVersion", "0.1.0");
    set_str("UninstallString", &format!("\"{}\" uninstall", exe_path));
    set_str("DisplayIcon", exe_path);
    set_str("InstallLocation", &install_dir().to_string_lossy());
    set_str("URLInfoAbout", "https://github.com/levkropp/eee");
    set_dword("EstimatedSize", 300); // ~300 KB
    set_dword("NoModify", 1);
    set_dword("NoRepair", 1);

    unsafe {
        let _ = RegCloseKey(hkey);
    }
}

fn remove_uninstall_entry() {
    let key_path = to_wide(r"Software\Microsoft\Windows\CurrentVersion\Uninstall\eee");
    unsafe {
        let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(key_path.as_ptr()));
    }
}

fn install() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let dest_dir = install_dir();
    let dest_exe = dest_dir.join("eee.exe");
    let src_exe = std::env::current_exe().expect("cannot determine own path");

    // Create directory
    let _ = std::fs::create_dir_all(&dest_dir);

    // Copy exe (skip if running from install location already)
    let src_canonical = std::fs::canonicalize(&src_exe).unwrap_or(src_exe.clone());
    let dest_canonical = std::fs::canonicalize(&dest_exe).unwrap_or(dest_exe.clone());
    if src_canonical != dest_canonical {
        if let Err(e) = std::fs::copy(&src_exe, &dest_exe) {
            show_message(&format!("Failed to copy exe: {}", e), "eee - Error");
            return;
        }
    }

    // Remove existing task (ignore errors)
    let _ = std::process::Command::new("schtasks")
        .args(["/delete", "/tn", TASK_NAME, "/f"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    // Create scheduled task: run at logon, normal priority, no time limit
    let exe_str = dest_exe.to_string_lossy().to_string();
    let result = std::process::Command::new("schtasks")
        .args([
            "/create",
            "/tn", TASK_NAME,
            "/tr", &format!("\"{}\"", exe_str),
            "/sc", "onlogon",
            "/rl", "highest",
            "/f",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    match result {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            show_message(
                &format!("Failed to create scheduled task:\n{}", stderr),
                "eee - Error",
            );
            return;
        }
        Err(e) => {
            show_message(&format!("Failed to run schtasks: {}", e), "eee - Error");
            return;
        }
    }

    // Register in Add/Remove Programs
    register_uninstall_entry(&exe_str);

    // Start it now
    let _ = std::process::Command::new("schtasks")
        .args(["/run", "/tn", TASK_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    show_message(
        &format!(
            "eee installed successfully.\n\n\
             Location: {}\n\
             Startup: Scheduled Task \"{}\"\n\n\
             Hold Ctrl+Alt+Shift+E for 10s to restart Explorer.\n\
             Uninstall from Settings > Apps or run \"eee uninstall\".",
            exe_str, TASK_NAME
        ),
        "eee - Installed",
    );
}

fn uninstall() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    // Delete scheduled task
    let _ = std::process::Command::new("schtasks")
        .args(["/delete", "/tn", TASK_NAME, "/f"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    // Remove Add/Remove Programs entry
    remove_uninstall_entry();

    // Stop other running instances (exclude our own PID)
    let our_pid = std::process::id();
    let _ = std::process::Command::new("cmd")
        .args([
            "/c",
            &format!(
                "wmic process where \"name='eee.exe' and processid!={}\" call terminate >nul 2>&1",
                our_pid
            ),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    show_message("eee uninstalled successfully.", "eee - Uninstalled");

    // Schedule self-deletion after we exit
    let dest_dir = install_dir();
    let dest_exe = dest_dir.join("eee.exe");
    let _ = std::process::Command::new("cmd")
        .args([
            "/c",
            &format!(
                "ping -n 2 127.0.0.1 >nul & del /q \"{}\" & rmdir \"{}\"",
                dest_exe.to_string_lossy(),
                dest_dir.to_string_lossy()
            ),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

fn show_message(text: &str, title: &str) {
    let wide_text = to_wide(text);
    let wide_title = to_wide(title);
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(wide_text.as_ptr()),
            PCWSTR(wide_title.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

// ---------------------------------------------------------------------------
// Single-instance mutex
// ---------------------------------------------------------------------------

/// Returns true if we are the only instance. The handle is intentionally
/// leaked so the mutex lives for the process lifetime.
fn acquire_singleton() -> bool {
    let name = to_wide(MUTEX_NAME);
    let result = unsafe { CreateMutexW(None, true, PCWSTR(name.as_ptr())) };
    match result {
        Ok(_handle) => {
            // If GetLastError would be ERROR_ALREADY_EXISTS the call still
            // succeeds but we didn't create it. Check with GetLastError.
            let err = unsafe { windows::Win32::Foundation::GetLastError() };
            if err.0 == 183 {
                // ERROR_ALREADY_EXISTS
                false
            } else {
                // We own it — leak the handle so it stays alive.
                let _ = _handle;
                true
            }
        }
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Hotkey detection & overlay (unchanged logic)
// ---------------------------------------------------------------------------

fn hotkey_held() -> bool {
    unsafe {
        GetAsyncKeyState(VK_E) < 0
            && GetAsyncKeyState(VK_CONTROL.0 as i32) < 0
            && GetAsyncKeyState(VK_MENU.0 as i32) < 0
            && GetAsyncKeyState(VK_SHIFT.0 as i32) < 0
    }
}

struct OverlayState {
    hold_start: Option<Instant>,
    progress: f64,
    triggered: bool,
}

fn get_state(hwnd: HWND) -> *mut OverlayState {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OverlayState }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let state = Box::into_raw(Box::new(OverlayState {
                hold_start: None,
                progress: 0.0,
                triggered: false,
            }));
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize) };
            unsafe { SetTimer(Some(hwnd), TIMER_ID, POLL_MS, None) };
            let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
            LRESULT(0)
        }

        WM_TIMER => {
            let state = get_state(hwnd);
            if state.is_null() {
                return LRESULT(0);
            }
            let state = unsafe { &mut *state };

            if state.triggered {
                return LRESULT(0);
            }

            if hotkey_held() {
                if state.hold_start.is_none() {
                    state.hold_start = Some(Instant::now());
                    let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
                    let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
                    let x = (screen_w - WND_WIDTH) / 2;
                    let y = (screen_h - WND_HEIGHT) / 2;
                    unsafe {
                        let _ = SetWindowPos(
                            hwnd,
                            Some(HWND_TOPMOST),
                            x,
                            y,
                            WND_WIDTH,
                            WND_HEIGHT,
                            SWP_SHOWWINDOW,
                        );
                    }
                }

                let elapsed = state.hold_start.unwrap().elapsed().as_secs_f64();
                state.progress = (elapsed / HOLD_SECONDS).min(1.0);

                if elapsed >= HOLD_SECONDS {
                    state.triggered = true;
                    let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
                    restart_explorer();
                    state.hold_start = None;
                    state.progress = 0.0;
                    state.triggered = false;
                }
            } else if state.hold_start.is_some() {
                state.hold_start = None;
                state.progress = 0.0;
                let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
            }

            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
            LRESULT(0)
        }

        WM_PAINT => {
            let state = get_state(hwnd);
            let mut ps = PAINTSTRUCT::default();
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };

            let mut rc = RECT::default();
            unsafe {
                let _ = GetClientRect(hwnd, &mut rc);
            }

            let margin = 24;

            // White background
            let bg = unsafe { CreateSolidBrush(BG_COLOR) };
            unsafe { FillRect(hdc, &rc, bg) };
            unsafe { let _ = DeleteObject(bg.into()); }

            // 1px border
            let border_brush = unsafe { CreateSolidBrush(BORDER_COLOR) };
            unsafe { FrameRect(hdc, &rc, border_brush) };
            unsafe { let _ = DeleteObject(border_brush.into()); }

            // Blue accent strip at top (4px)
            let accent_rect = RECT { left: 1, top: 1, right: rc.right - 1, bottom: 5 };
            let accent = unsafe { CreateSolidBrush(FILL_COLOR) };
            unsafe { FillRect(hdc, &accent_rect, accent) };
            unsafe { let _ = DeleteObject(accent.into()); }

            unsafe { SetBkMode(hdc, TRANSPARENT) };

            // Title font (Segoe UI Semibold, 20px)
            let font_name = to_wide("Segoe UI");
            let title_font = unsafe {
                CreateFontW(
                    -20, 0, 0, 0,
                    FW_SEMIBOLD.0 as i32,
                    0, 0, 0,
                    DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
                    CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY,
                    0, PCWSTR(font_name.as_ptr()),
                )
            };

            // Subtitle font (Segoe UI Regular, 14px)
            let sub_font = unsafe {
                CreateFontW(
                    -14, 0, 0, 0,
                    FW_NORMAL.0 as i32,
                    0, 0, 0,
                    DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
                    CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY,
                    0, PCWSTR(font_name.as_ptr()),
                )
            };

            let progress = if !state.is_null() {
                unsafe { (*state).progress }
            } else {
                0.0
            };

            let remaining = ((1.0 - progress) * HOLD_SECONDS).max(0.0);

            // Title: "Restarting Explorer..."
            let title = "Restarting Explorer...";
            unsafe { SetTextColor(hdc, TEXT_COL) };
            let old = unsafe { SelectObject(hdc, title_font.into()) };
            let mut title_rect = RECT {
                left: margin, top: 16, right: rc.right - margin, bottom: 48,
            };
            let mut wide_title = to_wide(title);
            unsafe {
                DrawTextW(hdc, &mut wide_title, &mut title_rect,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER);
            }

            // Subtitle: countdown + hint
            unsafe { SelectObject(hdc, sub_font.into()) };
            unsafe { SetTextColor(hdc, SUBTEXT_COL) };

            let subtitle = if remaining > 0.1 {
                format!("{:.0} seconds remaining \u{2014} release keys to cancel", remaining)
            } else {
                "Restarting now...".to_string()
            };
            let mut sub_rect = RECT {
                left: margin, top: 48, right: rc.right - margin, bottom: 70,
            };
            let mut wide_sub = to_wide(&subtitle);
            unsafe {
                DrawTextW(hdc, &mut wide_sub, &mut sub_rect,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER);
            }

            unsafe { SelectObject(hdc, old) };
            unsafe { let _ = DeleteObject(title_font.into()); }
            unsafe { let _ = DeleteObject(sub_font.into()); }

            // Progress bar (rounded feel: 6px tall)
            let bar_height = 6;
            let bar_top = rc.bottom - 32;
            let bar_rect = RECT {
                left: margin, top: bar_top,
                right: rc.right - margin, bottom: bar_top + bar_height,
            };

            let bar_bg = unsafe { CreateSolidBrush(BAR_BG_COLOR) };
            unsafe { FillRect(hdc, &bar_rect, bar_bg) };
            unsafe { let _ = DeleteObject(bar_bg.into()); }

            let fill_width = ((bar_rect.right - bar_rect.left) as f64 * progress) as i32;
            if fill_width > 0 {
                let fill_rect = RECT {
                    left: bar_rect.left, top: bar_rect.top,
                    right: bar_rect.left + fill_width, bottom: bar_rect.bottom,
                };
                let fill_br = unsafe { CreateSolidBrush(FILL_COLOR) };
                unsafe { FillRect(hdc, &fill_rect, fill_br) };
                unsafe { let _ = DeleteObject(fill_br.into()); }
            }

            // Percentage right-aligned under bar
            let pct_text = format!("{}%", (progress * 100.0) as u32);
            let pct_font = unsafe {
                CreateFontW(
                    -12, 0, 0, 0,
                    FW_NORMAL.0 as i32,
                    0, 0, 0,
                    DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
                    CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY,
                    0, PCWSTR(font_name.as_ptr()),
                )
            };
            let old2 = unsafe { SelectObject(hdc, pct_font.into()) };
            unsafe { SetTextColor(hdc, SUBTEXT_COL) };
            let mut pct_rect = RECT {
                left: margin, top: bar_top + bar_height + 2,
                right: rc.right - margin, bottom: rc.bottom - 4,
            };
            let mut wide_pct = to_wide(&pct_text);
            unsafe {
                DrawTextW(hdc, &mut wide_pct, &mut pct_rect,
                    DT_RIGHT | DT_SINGLELINE);
            }
            unsafe { SelectObject(hdc, old2) };
            unsafe { let _ = DeleteObject(pct_font.into()); }

            let _ = unsafe { EndPaint(hwnd, &ps) };
            LRESULT(0)
        }

        WM_DESTROY => {
            let state = get_state(hwnd);
            if !state.is_null() {
                unsafe { drop(Box::from_raw(state)) };
            }
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }

        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn restart_explorer() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/IM", "explorer.exe"])
        .creation_flags(CREATE_NO_WINDOW)
        .status();

    std::thread::sleep(Duration::from_millis(500));

    let _ = std::process::Command::new("explorer.exe").spawn();
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() > 1 {
        match args[1].to_lowercase().as_str() {
            "install" => {
                install();
                return;
            }
            "uninstall" => {
                uninstall();
                return;
            }
            _ => {}
        }
    }

    // If launched with no args and NOT from the install directory,
    // treat it as a double-click install.
    if args.len() == 1 && !is_running_from_install_dir() {
        install();
        return;
    }

    // Single-instance check — silently exit if already running
    if !acquire_singleton() {
        return;
    }

    // Run the overlay message loop
    let class_name = to_wide("eee_overlay");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wnd_proc),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW).unwrap_or_default() },
        ..Default::default()
    };

    unsafe { RegisterClassW(&wc) };

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(to_wide("eee").as_ptr()),
            WS_POPUP,
            0,
            0,
            WND_WIDTH,
            WND_HEIGHT,
            None,
            None,
            None,
            None,
        )
    }
    .unwrap();

    unsafe {
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 245, LWA_ALPHA);
    }

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
