// Windows 로그오프/종료/재시작 때 패닉 없이 끝내기.
//
// tao 0.35.3은 WM_ENDSESSION(wParam=TRUE)을 받으면 이벤트 루프를 Destroyed로 옮기고
// (event_loop.rs:2384-2392) 그대로 메시지 루프로 돌아간다. 창이 닫히기 전까지 다음
// 메시지(PTY 출력 이벤트, WM_PAINT 등)가 들어오면 runner.rs:371
// "cannot move state from Destroyed"로 패닉한다. crash.log의 네 건이 모두 이것이다
// (시각이 전부 로그오프/종료 이벤트와 겹친다). 패닉 훅의 오류 창이 종료 도중에 떠서
// 종료를 붙잡을 수도 있다.
//
// tao 0.37.0(#1157)은 loop_destroyed 뒤 바로 process::exit(0)으로 고쳤지만
// tauri-runtime-wry 2.11.x가 tao ^0.35에 묶여 있어 cargo update로는 못 받는다.
// 그래서 같은 동작을 앱에서 한다: tao의 숨은 "Tao Thread Event Target" 창(이 창이
// WM_ENDSESSION을 처리한다)에 서브클래스를 하나 더 건다. 나중에 건 서브클래스가 먼저
// 불리므로, tao의 처리를 먼저 돌려(RunEvent::Exit → 창 상태 저장) 놓고 PTY를 정리한 뒤
// 루프로 돌아가지 않고 바로 끝낸다. tao를 0.37 이상으로 올리면 이 파일은 지워도 된다.

use super::*;
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumThreadWindows, GetClassNameW, WM_ENDSESSION, WM_NCDESTROY,
};

const TAO_TARGET_CLASS: &str = "Tao Thread Event Target";
const SUBCLASS_ID: usize = 0x434C_4944; // "CLID"

static APP: OnceLock<AppHandle> = OnceLock::new();

/// 세션이 끝나는 중이면 패닉 훅이 오류 창을 띄우지 않게 한다 (창이 종료를 붙잡는다).
pub(crate) static SESSION_ENDING: AtomicBool = AtomicBool::new(false);

/// 메인 스레드(setup)에서 불러야 한다 — SetWindowSubclass는 창을 만든 스레드에서만 된다.
pub(crate) fn install(app: &AppHandle) {
    let _ = APP.set(app.clone());
    let mut hooked: u32 = 0;
    unsafe {
        EnumThreadWindows(
            GetCurrentThreadId(),
            Some(enum_proc),
            &mut hooked as *mut u32 as LPARAM,
        );
    }
    trace_always("app", "", "endsession-hook", &hooked.to_string());
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let mut buf = [0u16; 64];
    let n = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if n > 0 && String::from_utf16_lossy(&buf[..n as usize]) == TAO_TARGET_CLASS {
        if SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0) != 0 {
            *(lparam as *mut u32) += 1;
        }
    }
    1 // 계속 열거
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_ENDSESSION if wparam != 0 => {
            SESSION_ENDING.store(true, Ordering::SeqCst);
            // tao가 loop_destroyed → RunEvent::Exit을 돌린다 (window-state 플러그인이 여기서 저장).
            let _ = DefSubclassProc(hwnd, msg, wparam, lparam);
            if let Some(app) = APP.get() {
                kill_all_ptys(app);
            }
            // 루프로 돌아가면 다음 메시지에서 패닉한다. tao 0.37과 같이 여기서 끝낸다.
            std::process::exit(0);
        }
        WM_NCDESTROY => {
            RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID);
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        _ => DefSubclassProc(hwnd, msg, wparam, lparam),
    }
}
