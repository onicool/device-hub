//! 常駐 (タスクトレイ) と Windows ログイン時の自動起動 — G HUB の「常に裏で動く」役の置き換え。
//!
//! - 窓の × は終了ではなく**隠すだけ** (`Prefs::resident` が ON のとき)。トレイアイコンの
//!   クリックで再表示、右クリックメニューの「終了」で本当に終わる。
//!   プロセスが生きている間は HID バックエンド・プロセス連動 (procmon)・電池監視が動き続ける。
//! - 自動起動は `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\DeviceHub` に
//!   `"<exe>" --background` を書く (管理者権限不要)。`--background` で起動すると
//!   窓を出さずにトレイだけで常駐する。
//!
//! マウスの設定自体は本体のオンボードメモリに保存されるので、常駐していなくても
//! DPI・ボタン等は効く。常駐は電池通知・アプリ連動プロファイルのため。

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

/// × で隠すか (Prefs::resident の写し。設定画面のトグルで更新)。
pub static RESIDENT: AtomicBool = AtomicBool::new(true);
/// トレイの「終了」が押された — 次の close 要求は止めない。
static QUITTING: AtomicBool = AtomicBool::new(false);
/// メイン窓の HWND。
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

/// `--background` で起動されたか (自動起動時)。
pub fn background_launch() -> bool {
    std::env::args().any(|a| a == "--background")
}

/// close 要求を受けたとき: 閉じてよければ true。常駐中は窓を隠して false。
pub fn should_close() -> bool {
    if QUITTING.load(Ordering::SeqCst) || !RESIDENT.load(Ordering::SeqCst) {
        return true;
    }
    if MAIN_HWND.load(Ordering::SeqCst) == 0 {
        return true; // トレイが無い (非 Windows) なら隠すと戻せない
    }
    hide_main();
    false
}

/// メイン窓を登録してトレイアイコンを出す。
pub fn install(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    MAIN_HWND.store(hwnd, Ordering::SeqCst);
    imp::start_tray();
}

pub fn hide_main() {
    imp::show(MAIN_HWND.load(Ordering::SeqCst), false);
}

pub fn show_main() {
    imp::show(MAIN_HWND.load(Ordering::SeqCst), true);
}

/// 自動起動が登録されているか。
pub fn autostart_enabled() -> bool {
    imp::autostart_get()
}

/// 自動起動の登録/解除。
pub fn set_autostart(on: bool) -> Result<(), String> {
    imp::autostart_set(on)
}

#[cfg(windows)]
mod imp {
    use super::{MAIN_HWND, QUITTING};
    use std::sync::atomic::Ordering;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Registry::{
        RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
    };
    use windows_sys::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    const WM_TRAY: u32 = WM_APP + 1;
    const ID_OPEN: usize = 1;
    const ID_QUIT: usize = 2;
    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const RUN_NAME: &str = "DeviceHub";

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn show(hwnd: isize, visible: bool) {
        if hwnd == 0 {
            return;
        }
        unsafe {
            let h = hwnd as HWND;
            if visible {
                ShowWindow(h, SW_SHOW);
                ShowWindow(h, SW_RESTORE);
                SetForegroundWindow(h);
            } else {
                ShowWindow(h, SW_HIDE);
            }
        }
    }

    unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == WM_TRAY {
            let ev = (lp & 0xFFFF) as u32;
            if ev == WM_LBUTTONUP || ev == WM_LBUTTONDBLCLK {
                show(MAIN_HWND.load(Ordering::SeqCst), true);
            } else if ev == WM_RBUTTONUP {
                let menu = CreatePopupMenu();
                let open = w("Device HUB を開く");
                let quit = w("終了");
                AppendMenuW(menu, MF_STRING, ID_OPEN, open.as_ptr());
                AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
                AppendMenuW(menu, MF_STRING, ID_QUIT, quit.as_ptr());
                let mut pt = POINT { x: 0, y: 0 };
                GetCursorPos(&mut pt);
                // メニュー外クリックで閉じるには前面化が必要 (Win32 の作法)。
                SetForegroundWindow(h);
                let cmd = TrackPopupMenu(
                    menu,
                    TPM_RETURNCMD | TPM_RIGHTBUTTON,
                    pt.x,
                    pt.y,
                    0,
                    h,
                    std::ptr::null(),
                ) as usize;
                DestroyMenu(menu);
                match cmd {
                    ID_OPEN => show(MAIN_HWND.load(Ordering::SeqCst), true),
                    ID_QUIT => {
                        QUITTING.store(true, Ordering::SeqCst);
                        let main = MAIN_HWND.load(Ordering::SeqCst);
                        if main != 0 {
                            PostMessageW(main as HWND, WM_CLOSE, 0, 0);
                        }
                    }
                    _ => {}
                }
            }
            return 0;
        }
        DefWindowProcW(h, msg, wp, lp)
    }

    pub fn start_tray() {
        std::thread::Builder::new()
            .name("devhub-tray".into())
            .spawn(|| unsafe {
                let inst = GetModuleHandleW(std::ptr::null());
                let class = w("DeviceHubTray");
                let wc = WNDCLASSW {
                    style: 0,
                    lpfnWndProc: Some(wndproc),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: inst,
                    hIcon: std::ptr::null_mut(),
                    hCursor: std::ptr::null_mut(),
                    hbrBackground: std::ptr::null_mut(),
                    lpszMenuName: std::ptr::null(),
                    lpszClassName: class.as_ptr(),
                };
                RegisterClassW(&wc);
                let title = w("Device HUB Tray");
                let h = CreateWindowExW(
                    0,
                    class.as_ptr(),
                    title.as_ptr(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    std::ptr::null_mut(),
                    inst,
                    std::ptr::null(),
                );
                if h.is_null() {
                    return;
                }
                // build.rs が埋め込んだアプリアイコン (リソース ID 1)。無ければ既定。
                let mut icon = LoadIconW(inst, 1 as _);
                if icon.is_null() {
                    icon = LoadIconW(std::ptr::null_mut(), IDI_APPLICATION);
                }
                let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
                nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
                nid.hWnd = h;
                nid.uID = 1;
                nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
                nid.uCallbackMessage = WM_TRAY;
                nid.hIcon = icon;
                for (i, c) in "Device HUB (常駐中)".encode_utf16().take(127).enumerate() {
                    nid.szTip[i] = c;
                }
                Shell_NotifyIconW(NIM_ADD, &nid);
                let mut msg: MSG = std::mem::zeroed();
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                Shell_NotifyIconW(NIM_DELETE, &nid);
            })
            .ok();
    }

    pub fn autostart_get() -> bool {
        let key = w(RUN_KEY);
        let name = w(RUN_NAME);
        let mut size: u32 = 0;
        unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            ) == 0
        }
    }

    pub fn autostart_set(on: bool) -> Result<(), String> {
        let key = w(RUN_KEY);
        let name = w(RUN_NAME);
        let rc = unsafe {
            if on {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                let cmd = w(&format!("\"{}\" --background", exe.display()));
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    name.as_ptr(),
                    REG_SZ,
                    cmd.as_ptr() as *const _,
                    (cmd.len() * 2) as u32,
                )
            } else {
                let r = RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr());
                if r == 2 {
                    0 // ERROR_FILE_NOT_FOUND = もともと無い
                } else {
                    r
                }
            }
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(format!("レジストリ書き込みに失敗しました (code {rc})"))
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn show(_hwnd: isize, _visible: bool) {}
    pub fn start_tray() {}
    pub fn autostart_get() -> bool {
        false
    }
    pub fn autostart_set(_on: bool) -> Result<(), String> {
        Err("自動起動は Windows のみ対応です".into())
    }
}
