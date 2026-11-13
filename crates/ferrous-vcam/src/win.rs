//! Windows: shared memory, and turning the camera on and off.

use anyhow::{anyhow, Context};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree, INVALID_HANDLE_VALUE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Memory::*;

/// A mapped view of a named file mapping.
pub struct Mapping {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    pub ptr: *mut u8,
}

unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Opens an existing mapping for writing (Ferrous's side).
    pub fn open(name: &str) -> anyhow::Result<Mapping> {
        unsafe {
            let handle = OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, &HSTRING::from(name)).with_context(|| format!("opening {name}"))?;
            Self::map(handle)
        }
    }

    /// Creates the mapping (the camera source's side), readable and writable
    /// by any user's processes so Ferrous (running as the user) can feed a
    /// source running inside the Frame Server service.
    pub fn create(name: &str, size: usize) -> anyhow::Result<Mapping> {
        unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            // Generic all to Everyone, and a low mandatory label so
            // processes of any integrity level can open it.
            ConvertStringSecurityDescriptorToSecurityDescriptorW(&HSTRING::from("D:(A;;GA;;;WD)S:(ML;;NW;;;LW)"), SDDL_REVISION_1, &mut sd, None)
                .context("security descriptor")?;
            let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0, bInheritHandle: false.into() };
            let handle = CreateFileMappingW(INVALID_HANDLE_VALUE, Some(&sa), PAGE_READWRITE, (size as u64 >> 32) as u32, size as u32, &HSTRING::from(name));
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            Self::map(handle.with_context(|| format!("creating {name}"))?)
        }
    }

    unsafe fn map(handle: HANDLE) -> anyhow::Result<Mapping> {
        let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, super::REGION_BYTES);
        if view.Value.is_null() {
            let _ = CloseHandle(handle);
            return Err(anyhow!("mapping the camera's shared memory failed"));
        }
        Ok(Mapping { handle, ptr: view.Value as *mut u8, view })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.handle);
        }
    }
}

/// The camera as Windows sees it, while it's on.
pub struct Camera {
    cam: IMFVirtualCamera,
}

unsafe impl Send for Camera {}

impl Camera {
    /// Registers "Ferrous Virtual Camera" for this session and starts
    /// it (Windows 11). Needs the camera source installed (see [`install`]).
    pub fn start() -> anyhow::Result<Camera> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_FULL).context("Media Foundation")?;
            let cam = MFCreateVirtualCamera(
                MFVirtualCameraType_SoftwareCameraSource,
                MFVirtualCameraLifetime_Session,
                MFVirtualCameraAccess_CurrentUser,
                &HSTRING::from(super::FRIENDLY_NAME),
                &HSTRING::from(super::CLSID),
                None,
            )
            .map_err(|e| anyhow!("Windows refused to create the virtual camera (it needs Windows 11 and the camera installed): {e}"))?;
            cam.Start(None).map_err(|e| anyhow!("starting the virtual camera: {e}"))?;
            Ok(Camera { cam })
        }
    }

    pub fn stop(self) {
        unsafe {
            let _ = self.cam.Stop();
            let _ = self.cam.Remove();
            let _ = self.cam.Shutdown();
        }
    }
}

/// Whether the camera source DLL is registered.
pub fn installed() -> bool {
    use windows::Win32::System::Registry::*;
    let key = HSTRING::from(format!("CLSID\\{}\\InprocServer32", super::CLSID));
    unsafe {
        let mut h = HKEY::default();
        let ok = RegOpenKeyExW(HKEY_CLASSES_ROOT, &key, None, KEY_READ, &mut h).is_ok();
        if ok {
            let _ = RegCloseKey(h);
        }
        ok
    }
}

/// Registers the camera source DLL (asks for administrator rights, like
/// any camera driver install).
pub fn install(dll: &std::path::Path) -> anyhow::Result<()> {
    run_regsvr(dll, false)
}

pub fn uninstall(dll: &std::path::Path) -> anyhow::Result<()> {
    run_regsvr(dll, true)
}

fn run_regsvr(dll: &std::path::Path, remove: bool) -> anyhow::Result<()> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let params = format!("/s {}\"{}\"", if remove { "/u " } else { "" }, dll.display());
    let r = unsafe { ShellExecuteW(None, &HSTRING::from("runas"), &HSTRING::from("regsvr32.exe"), &HSTRING::from(params), PCWSTR::null(), SW_HIDE) };
    if r.0 as usize <= 32 {
        return Err(anyhow!("couldn't run regsvr32 (code {})", r.0 as usize));
    }
    Ok(())
}
