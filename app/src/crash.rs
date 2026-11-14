//! Crash reporting. Release builds have no console, so without this an
//! internal error (a Rust panic) closes the window and leaves no trace.

use std::io::Write;
use std::path::PathBuf;

/// Keeps the previous session's log (`ferrous.previous.log`) so a crash
/// isn't overwritten by the next launch, and returns the new log's path.
pub fn rotate_log() -> PathBuf {
    let dir = ferrous_core::paths::config_dir();
    let _ = std::fs::create_dir_all(&dir);
    let log = dir.join("ferrous.log");
    if log.exists() {
        let _ = std::fs::rename(&log, dir.join("ferrous.previous.log"));
    }
    log
}

/// Logs every panic with its thread, location and backtrace, and writes a
/// `crash-<time>.txt` report next to the settings. When the UI thread
/// panics (the app is about to close), the user is told where the report is.
/// Panics on other threads (a source, an encoder) are reported but don't end
/// the app.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed").to_string();
        let backtrace = std::backtrace::Backtrace::force_capture();
        let report = format!(
            "Ferrous {} crashed.\nThread: {name}\n{info}\n\nBacktrace:\n{backtrace}\n",
            env!("CARGO_PKG_VERSION")
        );
        log::error!("{report}");
        log::logger().flush();
        let stamp = chrono_like_stamp();
        let path = ferrous_core::paths::config_dir().join(format!("crash-{stamp}.txt"));
        if let Ok(mut f) = std::fs::File::create(&path) {
            let _ = f.write_all(report.as_bytes());
        }
        default(info);
        if name == "main" {
            let msg = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown error".into());
            let at = info.location().map(|l| format!(" ({}:{})", l.file(), l.line())).unwrap_or_default();
            let _ = rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title("Ferrous crashed")
                .set_description(format!(
                    "Ferrous hit an internal error and has to close.\n\n{msg}{at}\n\nA report was saved to:\n{}",
                    path.display()
                ))
                .set_buttons(rfd::MessageButtons::Ok)
                .show();
        }
    }));
}

/// On Windows, records a hard crash (a driver or access violation, which Rust never sees as a panic)
/// in `native-crash.txt` next to the settings: the exception code and which DLL it happened in.
#[cfg(windows)]
pub fn install_native_crash_log() {
    use windows::Win32::System::Diagnostics::Debug::{SetUnhandledExceptionFilter, EXCEPTION_POINTERS};
    use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT};

    unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
        // SAFETY: Windows passes a valid pointer; everything below only reads from it.
        unsafe {
            let (mut code, mut addr) = (0u32, std::ptr::null_mut());
            if !info.is_null() && !(*info).ExceptionRecord.is_null() {
                code = (*(*info).ExceptionRecord).ExceptionCode.0 as u32;
                addr = (*(*info).ExceptionRecord).ExceptionAddress;
            }
            let mut module = String::from("unknown");
            let mut handle = Default::default();
            if GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, windows::core::PCWSTR(addr as *const u16), &mut handle).is_ok() {
                let mut buf = [0u16; 520];
                let n = GetModuleFileNameW(Some(handle), &mut buf) as usize;
                module = String::from_utf16_lossy(&buf[..n.min(buf.len())]);
            }
            let text = format!("Ferrous {} native crash: exception 0x{code:08X} at {addr:p} in {module}\n", env!("CARGO_PKG_VERSION"));
            log::error!("{text}");
            log::logger().flush();
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(ferrous_core::paths::config_dir().join("native-crash.txt")) {
                let _ = f.write_all(text.as_bytes());
            }
        }
        0 // EXCEPTION_CONTINUE_SEARCH: let Windows handle it as usual
    }
    // SAFETY: installs a process-wide handler that only writes a file.
    unsafe {
        SetUnhandledExceptionFilter(Some(filter));
    }
}

/// `YYYYMMDD-HHMMSS` in UTC, without pulling in a date crate.
fn chrono_like_stamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    #[test]
    fn stamp_is_well_formed() {
        let s = super::chrono_like_stamp();
        assert_eq!(s.len(), 15);
        assert!(s.starts_with("20"));
        assert_eq!(&s[8..9], "-");
    }
}
