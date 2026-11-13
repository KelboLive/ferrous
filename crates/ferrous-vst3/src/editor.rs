//! The plugin's own editor in a top-level Win32 window (Windows only).
//!
//! VST3 editors must be created, attached, resized and removed on the thread
//! that owns their parent window, so each editor gets a thread with its own
//! window and message loop. Edits made in the editor reach the audio thread
//! through the component handler set up in `Plugin::load`.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Arc;

use anyhow::anyhow;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComWrapper};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

struct SendPtr<T>(T);
unsafe impl<T> Send for SendPtr<T> {}

pub(crate) struct EditorHandle {
    hwnd: Arc<AtomicIsize>,
    open: Arc<AtomicBool>,
}

impl EditorHandle {
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }
    pub fn focus(&self) {
        let h = HWND(self.hwnd.load(Ordering::Relaxed) as *mut _);
        unsafe {
            let _ = ShowWindow(h, SW_SHOWNORMAL);
            let _ = SetForegroundWindow(h);
        }
    }
    pub fn close(&self) {
        let h = HWND(self.hwnd.load(Ordering::Relaxed) as *mut _);
        if !h.0.is_null() {
            unsafe {
                let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    }
}

/// Lets the editor ask for a new size.
struct Frame {
    hwnd: Arc<AtomicIsize>,
}

impl Class for Frame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for Frame {
    unsafe fn resizeView(&self, view: *mut IPlugView, new_size: *mut ViewRect) -> tresult {
        let h = HWND(self.hwnd.load(Ordering::Relaxed) as *mut _);
        if h.0.is_null() || new_size.is_null() {
            return kInvalidArgument;
        }
        let r = &*new_size;
        let (w, hgt) = outer_size(r.right - r.left, r.bottom - r.top);
        let _ = SetWindowPos(h, None, 0, 0, w, hgt, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
        if let Some(v) = vst3::ComRef::from_raw(view) {
            let mut rr = *r;
            v.onSize(&mut rr);
        }
        kResultOk
    }
}

fn style() -> WINDOW_STYLE {
    WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX
}

fn outer_size(w: i32, h: i32) -> (i32, i32) {
    let mut r = RECT { left: 0, top: 0, right: w.max(64), bottom: h.max(48) };
    unsafe {
        let _ = AdjustWindowRect(&mut r, style(), false);
    }
    (r.right - r.left, r.bottom - r.top)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

pub(crate) fn open(controller: ComPtr<IEditController>, title: &str) -> anyhow::Result<EditorHandle> {
    let hwnd = Arc::new(AtomicIsize::new(0));
    let open = Arc::new(AtomicBool::new(true));
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let ctl = SendPtr(controller);
    let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let (hwnd2, open2) = (hwnd.clone(), open.clone());
    std::thread::Builder::new().name("vst3-editor".into()).spawn(move || unsafe {
        let ctl = ctl;
        let result = (|| -> Result<(ComPtr<IPlugView>, HWND), String> {
            let view_raw = ctl.0.createView(ViewType::kEditor);
            let view = ComPtr::<IPlugView>::from_raw(view_raw).ok_or("the plugin has no editor")?;
            if view.isPlatformTypeSupported(kPlatformTypeHWND) != kResultTrue {
                return Err("the editor doesn't support Windows".into());
            }
            let hinst = GetModuleHandleW(None).map_err(|e| e.to_string())?;
            let class = w!("FerrousVst3Editor");
            let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst.into(), lpszClassName: class, hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(), ..Default::default() };
            RegisterClassW(&wc); // fails harmlessly if already registered
            let mut rect: ViewRect = std::mem::zeroed();
            view.getSize(&mut rect);
            let (w, h) = outer_size(rect.right - rect.left, rect.bottom - rect.top);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                PCWSTR(title.as_ptr()),
                style(),
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                w,
                h,
                None,
                None,
                Some(hinst.into()),
                None,
            )
            .map_err(|e| e.to_string())?;
            hwnd2.store(hwnd.0 as isize, Ordering::Relaxed);
            if let Some(frame) = ComWrapper::new(Frame { hwnd: hwnd2.clone() }).to_com_ptr::<IPlugFrame>() {
                view.setFrame(frame.as_ptr());
                std::mem::forget(frame); // the view holds it until removed()
            }
            if view.attached(hwnd.0, kPlatformTypeHWND) != kResultOk {
                let _ = DestroyWindow(hwnd);
                return Err("the editor failed to open".into());
            }
            let _ = ShowWindow(hwnd, SW_SHOW);
            Ok((view, hwnd))
        })();
        match result {
            Ok((view, _hwnd)) => {
                let _ = tx.send(Ok(()));
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                view.removed();
                view.setFrame(std::ptr::null_mut());
            }
            Err(e) => {
                let _ = tx.send(Err(e));
            }
        }
        hwnd2.store(0, Ordering::Relaxed);
        open2.store(false, Ordering::Relaxed);
    })?;
    rx.recv().map_err(|e| anyhow!("{e}"))?.map_err(|e| anyhow!(e))?;
    Ok(EditorHandle { hwnd, open })
}
