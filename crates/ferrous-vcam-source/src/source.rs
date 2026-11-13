#![allow(non_snake_case, clippy::missing_safety_doc)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

use ferrous_vcam::{Region, FPS, FRAME_BYTES, HEIGHT, REGION_BYTES, WIDTH};
use windows::core::{implement, Interface, Ref, Result, GUID, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Media::KernelStreaming::{IKsControl, IKsControl_Impl, KSIDENTIFIER, PINNAME_VIDEO_CAPTURE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows_core::{AsImpl, IUnknown, BOOL};

pub const CLSID_SOURCE: GUID = GUID::from_u128(0x93ac3189_7309_4939_a983_6883cee48652);

fn not_found() -> windows::core::Error {
    HRESULT::from_win32(ERROR_SET_NOT_FOUND.0).into()
}

// ------------------------------------------------------------- activator

#[implement(IMFActivate, IMFAttributes)]
struct Activator {
    attrs: IMFAttributes,
    source: Mutex<Option<IMFMediaSourceEx>>,
}

impl Activator {
    fn create() -> Result<IMFActivate> {
        let mut a = None;
        unsafe { MFCreateAttributes(&mut a, 1)? };
        Ok(Activator { attrs: a.ok_or(windows::core::Error::from(E_FAIL))?, source: Mutex::new(None) }.into())
    }
}

/// IMFAttributes, by delegation to an attribute store.
macro_rules! delegate_attributes {
    ($ty:ty, $field:ident) => {
        impl IMFAttributes_Impl for $ty {
            fn GetItem(&self, k: *const GUID, v: *mut PROPVARIANT) -> Result<()> { unsafe { (Interface::vtable(&self.$field).GetItem)(Interface::as_raw(&self.$field), k, v).ok() } }
            fn GetItemType(&self, k: *const GUID) -> Result<MF_ATTRIBUTE_TYPE> { unsafe { let mut t = MF_ATTRIBUTE_TYPE::default(); (Interface::vtable(&self.$field).GetItemType)(Interface::as_raw(&self.$field), k, &mut t).ok()?; Ok(t) } }
            fn CompareItem(&self, k: *const GUID, v: *const PROPVARIANT) -> Result<BOOL> { unsafe { let mut b = BOOL(0); (Interface::vtable(&self.$field).CompareItem)(Interface::as_raw(&self.$field), k, v, &mut b).ok()?; Ok(b) } }
            fn Compare(&self, theirs: Ref<IMFAttributes>, m: MF_ATTRIBUTES_MATCH_TYPE) -> Result<BOOL> { unsafe { let mut b = BOOL(0); (Interface::vtable(&self.$field).Compare)(Interface::as_raw(&self.$field), std::mem::transmute_copy(&theirs), m, &mut b).ok()?; Ok(b) } }
            fn GetUINT32(&self, k: *const GUID) -> Result<u32> { unsafe { let mut x = 0; (Interface::vtable(&self.$field).GetUINT32)(Interface::as_raw(&self.$field), k, &mut x).ok()?; Ok(x) } }
            fn GetUINT64(&self, k: *const GUID) -> Result<u64> { unsafe { let mut x = 0; (Interface::vtable(&self.$field).GetUINT64)(Interface::as_raw(&self.$field), k, &mut x).ok()?; Ok(x) } }
            fn GetDouble(&self, k: *const GUID) -> Result<f64> { unsafe { let mut x = 0.0; (Interface::vtable(&self.$field).GetDouble)(Interface::as_raw(&self.$field), k, &mut x).ok()?; Ok(x) } }
            fn GetGUID(&self, k: *const GUID) -> Result<GUID> { unsafe { let mut x = GUID::zeroed(); (Interface::vtable(&self.$field).GetGUID)(Interface::as_raw(&self.$field), k, &mut x).ok()?; Ok(x) } }
            fn GetStringLength(&self, k: *const GUID) -> Result<u32> { unsafe { let mut x = 0; (Interface::vtable(&self.$field).GetStringLength)(Interface::as_raw(&self.$field), k, &mut x).ok()?; Ok(x) } }
            fn GetString(&self, k: *const GUID, s: PWSTR, n: u32, len: *mut u32) -> Result<()> { unsafe { (Interface::vtable(&self.$field).GetString)(Interface::as_raw(&self.$field), k, s, n, len).ok() } }
            fn GetAllocatedString(&self, k: *const GUID, s: *mut PWSTR, len: *mut u32) -> Result<()> { unsafe { (Interface::vtable(&self.$field).GetAllocatedString)(Interface::as_raw(&self.$field), k, s, len).ok() } }
            fn GetBlobSize(&self, k: *const GUID) -> Result<u32> { unsafe { let mut x = 0; (Interface::vtable(&self.$field).GetBlobSize)(Interface::as_raw(&self.$field), k, &mut x).ok()?; Ok(x) } }
            fn GetBlob(&self, k: *const GUID, b: *mut u8, n: u32, sz: *mut u32) -> Result<()> { unsafe { (Interface::vtable(&self.$field).GetBlob)(Interface::as_raw(&self.$field), k, b, n, sz).ok() } }
            fn GetAllocatedBlob(&self, k: *const GUID, b: *mut *mut u8, sz: *mut u32) -> Result<()> { unsafe { (Interface::vtable(&self.$field).GetAllocatedBlob)(Interface::as_raw(&self.$field), k, b, sz).ok() } }
            fn GetUnknown(&self, k: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> { unsafe { (Interface::vtable(&self.$field).GetUnknown)(Interface::as_raw(&self.$field), k, riid, ppv).ok() } }
            fn SetItem(&self, k: *const GUID, v: *const PROPVARIANT) -> Result<()> { unsafe { (Interface::vtable(&self.$field).SetItem)(Interface::as_raw(&self.$field), k, v).ok() } }
            fn DeleteItem(&self, k: *const GUID) -> Result<()> { unsafe { (Interface::vtable(&self.$field).DeleteItem)(Interface::as_raw(&self.$field), k).ok() } }
            fn DeleteAllItems(&self) -> Result<()> { unsafe { (Interface::vtable(&self.$field).DeleteAllItems)(Interface::as_raw(&self.$field)).ok() } }
            fn SetUINT32(&self, k: *const GUID, v: u32) -> Result<()> { unsafe { (Interface::vtable(&self.$field).SetUINT32)(Interface::as_raw(&self.$field), k, v).ok() } }
            fn SetUINT64(&self, k: *const GUID, v: u64) -> Result<()> { unsafe { (Interface::vtable(&self.$field).SetUINT64)(Interface::as_raw(&self.$field), k, v).ok() } }
            fn SetDouble(&self, k: *const GUID, v: f64) -> Result<()> { unsafe { (Interface::vtable(&self.$field).SetDouble)(Interface::as_raw(&self.$field), k, v).ok() } }
            fn SetGUID(&self, k: *const GUID, v: *const GUID) -> Result<()> { unsafe { (Interface::vtable(&self.$field).SetGUID)(Interface::as_raw(&self.$field), k, v).ok() } }
            fn SetString(&self, k: *const GUID, v: &PCWSTR) -> Result<()> { unsafe { (Interface::vtable(&self.$field).SetString)(Interface::as_raw(&self.$field), k, *v).ok() } }
            fn SetBlob(&self, k: *const GUID, b: *const u8, n: u32) -> Result<()> { unsafe { (Interface::vtable(&self.$field).SetBlob)(Interface::as_raw(&self.$field), k, b, n).ok() } }
            fn SetUnknown(&self, k: *const GUID, u: Ref<IUnknown>) -> Result<()> { unsafe { (Interface::vtable(&self.$field).SetUnknown)(Interface::as_raw(&self.$field), k, std::mem::transmute_copy(&u)).ok() } }
            fn LockStore(&self) -> Result<()> { unsafe { (Interface::vtable(&self.$field).LockStore)(Interface::as_raw(&self.$field)).ok() } }
            fn UnlockStore(&self) -> Result<()> { unsafe { (Interface::vtable(&self.$field).UnlockStore)(Interface::as_raw(&self.$field)).ok() } }
            fn GetCount(&self) -> Result<u32> { unsafe { let mut x = 0; (Interface::vtable(&self.$field).GetCount)(Interface::as_raw(&self.$field), &mut x).ok()?; Ok(x) } }
            fn GetItemByIndex(&self, i: u32, k: *mut GUID, v: *mut PROPVARIANT) -> Result<()> { unsafe { (Interface::vtable(&self.$field).GetItemByIndex)(Interface::as_raw(&self.$field), i, k, v).ok() } }
            fn CopyAllItems(&self, d: Ref<IMFAttributes>) -> Result<()> { unsafe { (Interface::vtable(&self.$field).CopyAllItems)(Interface::as_raw(&self.$field), std::mem::transmute_copy(&d)).ok() } }
        }
    };
}

delegate_attributes!(Activator_Impl, attrs);

impl IMFActivate_Impl for Activator_Impl {
    fn ActivateObject(&self, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
        let mut s = self.source.lock().unwrap();
        if s.is_none() {
            *s = Some(Source::create()?);
        }
        unsafe { s.as_ref().unwrap().query(riid, ppv).ok() }
    }
    fn ShutdownObject(&self) -> Result<()> {
        if let Some(s) = self.source.lock().unwrap().take() {
            unsafe {
                let _ = s.Shutdown();
            }
        }
        Ok(())
    }
    fn DetachObject(&self) -> Result<()> {
        self.source.lock().unwrap().take();
        Ok(())
    }
}

// ---------------------------------------------------------------- source

#[implement(IMFMediaSourceEx, IMFMediaSource, IMFMediaEventGenerator, IMFGetService, IKsControl)]
struct Source {
    queue: IMFMediaEventQueue,
    pd: IMFPresentationDescriptor,
    attrs: IMFAttributes,
    stream: Mutex<Option<IMFMediaStream2>>,
    shut: Mutex<bool>,
}

fn media_type() -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, ((WIDTH as u64) << 32) | HEIGHT as u64)?;
        t.SetUINT64(&MF_MT_FRAME_RATE, ((FPS as u64) << 32) | 1)?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        t.SetUINT32(&MF_MT_DEFAULT_STRIDE, WIDTH)?;
        t.SetUINT32(&MF_MT_SAMPLE_SIZE, FRAME_BYTES as u32)?;
        t.SetUINT32(&MF_MT_AVG_BITRATE, WIDTH * HEIGHT * 12 * FPS)?;
        Ok(t)
    }
}

fn stream_attrs(a: &IMFAttributes) -> Result<()> {
    unsafe {
        a.SetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY, &PINNAME_VIDEO_CAPTURE)?;
        a.SetUINT32(&MF_DEVICESTREAM_STREAM_ID, 0)?;
        a.SetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED, 1)?;
        a.SetUINT32(&MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES, MFFrameSourceTypes_Color.0 as u32)?;
        Ok(())
    }
}

impl Source {
    fn create() -> Result<IMFMediaSourceEx> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
            let mt = media_type()?;
            let sd = MFCreateStreamDescriptor(0, &[Some(mt.clone())])?;
            sd.GetMediaTypeHandler()?.SetCurrentMediaType(&mt)?;
            stream_attrs(&sd.cast::<IMFAttributes>()?)?;
            let pd = MFCreatePresentationDescriptor(Some(&[Some(sd.clone())]))?;
            pd.SelectStream(0)?;
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 4)?;
            let attrs = attrs.ok_or(windows::core::Error::from(E_FAIL))?;
            let src: IMFMediaSourceEx = Source { queue: MFCreateEventQueue()?, pd, attrs, stream: Mutex::new(None), shut: Mutex::new(false) }.into();
            let stream = Stream::create(src.cast::<IMFMediaSource>()?, sd)?;
            *src.as_impl().stream.lock().unwrap() = Some(stream);
            Ok(src)
        }
    }

    fn check(&self) -> Result<()> {
        if *self.shut.lock().unwrap() { Err(MF_E_SHUTDOWN.into()) } else { Ok(()) }
    }
}

impl IMFMediaEventGenerator_Impl for Source_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> Result<IMFMediaEvent> {
        self.check()?;
        unsafe { self.queue.GetEvent(flags.0) }
    }
    fn BeginGetEvent(&self, cb: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> Result<()> {
        self.check()?;
        unsafe { self.queue.BeginGetEvent(cb.as_ref(), state.as_ref()) }
    }
    fn EndGetEvent(&self, r: Ref<IMFAsyncResult>) -> Result<IMFMediaEvent> {
        self.check()?;
        unsafe { self.queue.EndGetEvent(r.as_ref()) }
    }
    fn QueueEvent(&self, met: u32, ext: *const GUID, hr: HRESULT, v: *const PROPVARIANT) -> Result<()> {
        self.check()?;
        unsafe { self.queue.QueueEventParamVar(met, ext, hr, v) }
    }
}

impl IMFMediaSource_Impl for Source_Impl {
    fn GetCharacteristics(&self) -> Result<u32> {
        self.check()?;
        Ok(MFMEDIASOURCE_IS_LIVE.0 as u32)
    }
    fn CreatePresentationDescriptor(&self) -> Result<IMFPresentationDescriptor> {
        self.check()?;
        unsafe { self.pd.Clone() }
    }
    fn Start(&self, _pd: Ref<IMFPresentationDescriptor>, _fmt: *const GUID, start: *const PROPVARIANT) -> Result<()> {
        self.check()?;
        let stream = self.stream.lock().unwrap().clone().ok_or(windows::core::Error::from(E_UNEXPECTED))?;
        unsafe {
            stream.as_impl().set_state(MF_STREAM_STATE_RUNNING);
            self.queue.QueueEventParamUnk(MENewStream.0 as u32, &GUID::zeroed(), S_OK, &stream.cast::<IUnknown>()?)?;
            self.queue.QueueEventParamVar(MESourceStarted.0 as u32, &GUID::zeroed(), S_OK, start)?;
            stream.as_impl().queue.QueueEventParamVar(MEStreamStarted.0 as u32, &GUID::zeroed(), S_OK, start)?;
        }
        Ok(())
    }
    fn Stop(&self) -> Result<()> {
        self.check()?;
        if let Some(stream) = self.stream.lock().unwrap().clone() {
            unsafe {
                stream.as_impl().set_state(MF_STREAM_STATE_STOPPED);
                stream.as_impl().queue.QueueEventParamVar(MEStreamStopped.0 as u32, &GUID::zeroed(), S_OK, std::ptr::null())?;
            }
        }
        unsafe { self.queue.QueueEventParamVar(MESourceStopped.0 as u32, &GUID::zeroed(), S_OK, std::ptr::null()) }
    }
    fn Pause(&self) -> Result<()> {
        Err(MF_E_INVALID_STATE_TRANSITION.into())
    }
    fn Shutdown(&self) -> Result<()> {
        let mut shut = self.shut.lock().unwrap();
        if *shut {
            return Ok(());
        }
        *shut = true;
        if let Some(stream) = self.stream.lock().unwrap().take() {
            unsafe { stream.as_impl() }.shutdown();
        }
        unsafe {
            let _ = self.queue.Shutdown();
        }
        Ok(())
    }
}

impl IMFMediaSourceEx_Impl for Source_Impl {
    fn GetSourceAttributes(&self) -> Result<IMFAttributes> {
        self.check()?;
        Ok(self.attrs.clone())
    }
    fn GetStreamAttributes(&self, id: u32) -> Result<IMFAttributes> {
        self.check()?;
        if id != 0 {
            return Err(MF_E_INVALIDSTREAMNUMBER.into());
        }
        let s = self.stream.lock().unwrap().clone().ok_or(windows::core::Error::from(E_UNEXPECTED))?;
        Ok(unsafe { s.as_impl() }.attrs.clone())
    }
    fn SetD3DManager(&self, _m: Ref<IUnknown>) -> Result<()> {
        // Samples are delivered in system memory.
        Ok(())
    }
}

impl IMFGetService_Impl for Source_Impl {
    fn GetService(&self, _g: *const GUID, _r: *const GUID, _p: *mut *mut c_void) -> Result<()> {
        Err(MF_E_UNSUPPORTED_SERVICE.into())
    }
}

impl IKsControl_Impl for Source_Impl {
    fn KsProperty(&self, _p: *const KSIDENTIFIER, _pl: u32, _d: *mut c_void, _dl: u32, _r: *mut u32) -> Result<()> {
        Err(not_found())
    }
    fn KsMethod(&self, _p: *const KSIDENTIFIER, _pl: u32, _d: *mut c_void, _dl: u32, _r: *mut u32) -> Result<()> {
        Err(not_found())
    }
    fn KsEvent(&self, _p: *const KSIDENTIFIER, _pl: u32, _d: *mut c_void, _dl: u32, _r: *mut u32) -> Result<()> {
        Err(not_found())
    }
}

// ---------------------------------------------------------------- stream

/// The shared region the stream reads Ferrous's frames from.
struct Shared {
    _map: ferrous_vcam::win::Mapping,
    region: Region,
}

#[implement(IMFMediaStream2, IMFMediaStream, IMFMediaEventGenerator, IKsControl)]
struct Stream {
    source: Mutex<Option<IMFMediaSource>>,
    sd: IMFStreamDescriptor,
    queue: IMFMediaEventQueue,
    attrs: IMFAttributes,
    state: AtomicI32,
    shared: Mutex<Option<Shared>>,
    idle: Vec<u8>,
    frames: std::sync::atomic::AtomicU64,
}

impl Stream {
    fn create(source: IMFMediaSource, sd: IMFStreamDescriptor) -> Result<IMFMediaStream2> {
        unsafe {
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 4)?;
            let attrs = attrs.ok_or(windows::core::Error::from(E_FAIL))?;
            stream_attrs(&attrs)?;
            // The Frame Server can create Global\ objects (it's a service);
            // a camera opened in-process by an app falls back to Local\.
            let shared = ferrous_vcam::win::Mapping::create(ferrous_vcam::NAME_GLOBAL, REGION_BYTES)
                .or_else(|_| ferrous_vcam::win::Mapping::create(ferrous_vcam::NAME_LOCAL, REGION_BYTES))
                .ok()
                .map(|m| {
                    let region = Region::from_raw(m.ptr, REGION_BYTES);
                    if !region.is_valid() {
                        region.init();
                    }
                    Shared { _map: m, region }
                });
            Ok(Stream {
                source: Mutex::new(Some(source)),
                sd,
                queue: MFCreateEventQueue()?,
                attrs,
                state: AtomicI32::new(MF_STREAM_STATE_STOPPED.0),
                shared: Mutex::new(shared),
                idle: ferrous_vcam::idle_frame(),
                frames: Default::default(),
            }
            .into())
        }
    }

    fn set_state(&self, s: MF_STREAM_STATE) {
        self.state.store(s.0, Ordering::Relaxed);
    }

    fn shutdown(&self) {
        self.source.lock().unwrap().take();
        self.shared.lock().unwrap().take();
        unsafe {
            let _ = self.queue.Shutdown();
        }
    }

    /// Fills `dst` with Ferrous's newest frame, or the idle card if Ferrous
    /// hasn't written for two seconds.
    fn fill(&self, dst: &mut [u8]) {
        if let Some(s) = self.shared.lock().unwrap().as_ref() {
            let fresh = ferrous_vcam::now_ms().saturating_sub(s.region.header().heartbeat_ms.load(Ordering::Relaxed)) < 2000;
            if fresh && s.region.read(dst) {
                return;
            }
        }
        dst.copy_from_slice(&self.idle);
    }
}

impl IMFMediaEventGenerator_Impl for Stream_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> Result<IMFMediaEvent> {
        unsafe { self.queue.GetEvent(flags.0) }
    }
    fn BeginGetEvent(&self, cb: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> Result<()> {
        unsafe { self.queue.BeginGetEvent(cb.as_ref(), state.as_ref()) }
    }
    fn EndGetEvent(&self, r: Ref<IMFAsyncResult>) -> Result<IMFMediaEvent> {
        unsafe { self.queue.EndGetEvent(r.as_ref()) }
    }
    fn QueueEvent(&self, met: u32, ext: *const GUID, hr: HRESULT, v: *const PROPVARIANT) -> Result<()> {
        unsafe { self.queue.QueueEventParamVar(met, ext, hr, v) }
    }
}

impl IMFMediaStream_Impl for Stream_Impl {
    fn GetMediaSource(&self) -> Result<IMFMediaSource> {
        self.source.lock().unwrap().clone().ok_or(MF_E_SHUTDOWN.into())
    }
    fn GetStreamDescriptor(&self) -> Result<IMFStreamDescriptor> {
        Ok(self.sd.clone())
    }
    fn RequestSample(&self, token: Ref<IUnknown>) -> Result<()> {
        if self.state.load(Ordering::Relaxed) != MF_STREAM_STATE_RUNNING.0 {
            return Err(MF_E_INVALIDREQUEST.into());
        }
        unsafe {
            let buf = MFCreateMemoryBuffer(FRAME_BYTES as u32)?;
            let mut p = std::ptr::null_mut();
            let mut max = 0;
            buf.Lock(&mut p, Some(&mut max), None)?;
            self.fill(std::slice::from_raw_parts_mut(p, FRAME_BYTES));
            buf.Unlock()?;
            buf.SetCurrentLength(FRAME_BYTES as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime(MFGetSystemTime())?;
            sample.SetSampleDuration(10_000_000 / FPS as i64)?;
            if let Some(t) = token.as_ref() {
                sample.SetUnknown(&MFSampleExtension_Token, t)?;
            }
            self.frames.fetch_add(1, Ordering::Relaxed);
            self.queue.QueueEventParamUnk(MEMediaSample.0 as u32, &GUID::zeroed(), S_OK, &sample.cast::<IUnknown>()?)
        }
    }
}

impl IMFMediaStream2_Impl for Stream_Impl {
    fn SetStreamState(&self, value: MF_STREAM_STATE) -> Result<()> {
        self.set_state(value);
        Ok(())
    }
    fn GetStreamState(&self) -> Result<MF_STREAM_STATE> {
        Ok(MF_STREAM_STATE(self.state.load(Ordering::Relaxed)))
    }
}

impl IKsControl_Impl for Stream_Impl {
    fn KsProperty(&self, _p: *const KSIDENTIFIER, _pl: u32, _d: *mut c_void, _dl: u32, _r: *mut u32) -> Result<()> {
        Err(not_found())
    }
    fn KsMethod(&self, _p: *const KSIDENTIFIER, _pl: u32, _d: *mut c_void, _dl: u32, _r: *mut u32) -> Result<()> {
        Err(not_found())
    }
    fn KsEvent(&self, _p: *const KSIDENTIFIER, _pl: u32, _d: *mut c_void, _dl: u32, _r: *mut u32) -> Result<()> {
        Err(not_found())
    }
}

// ------------------------------------------------------------ DLL exports

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(&self, outer: Ref<IUnknown>, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let a = Activator::create()?;
        unsafe { a.query(riid, ppv).ok() }
    }
    fn LockServer(&self, _lock: BOOL) -> Result<()> {
        Ok(())
    }
}

#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(clsid: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = std::ptr::null_mut();
    if clsid.is_null() || *clsid != CLSID_SOURCE {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    let f: IClassFactory = Factory.into();
    f.query(riid, ppv)
}

#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    // The Frame Server keeps the DLL loaded while the camera exists.
    S_FALSE
}

fn module_path() -> Option<Vec<u16>> {
    use windows::Win32::System::LibraryLoader::*;
    unsafe {
        let mut h = HMODULE::default();
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(DllGetClassObject as *const () as *const u16),
            &mut h,
        )
        .ok()?;
        let mut buf = vec![0u16; 1024];
        let n = GetModuleFileNameW(Some(h), &mut buf) as usize;
        (n > 0).then(|| {
            buf.truncate(n);
            buf.push(0);
            buf
        })
    }
}

fn clsid_key() -> Vec<u16> {
    format!("Software\\Classes\\CLSID\\{}", ferrous_vcam::CLSID).encode_utf16().chain(Some(0)).collect()
}

/// `regsvr32 ferrous_vcam.dll` (as administrator): registers the source in
/// HKLM so the Frame Server service can load it.
#[no_mangle]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    use windows::Win32::System::Registry::*;
    let Some(path) = module_path() else { return E_FAIL };
    unsafe {
        let key = clsid_key();
        let mut h = HKEY::default();
        if RegCreateKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(key.as_ptr()), None, None, REG_OPTION_NON_VOLATILE, KEY_WRITE, None, &mut h, None).is_err() {
            return HRESULT(0x8004_0201_u32 as i32); // SELFREG_E_CLASS
        }
        let name: Vec<u16> = ferrous_vcam::FRIENDLY_NAME.encode_utf16().chain(Some(0)).collect();
        let _ = RegSetValueExW(h, PCWSTR::null(), None, REG_SZ, Some(std::slice::from_raw_parts(name.as_ptr() as *const u8, name.len() * 2)));
        let mut inproc = HKEY::default();
        let ok = RegCreateKeyExW(h, windows::core::w!("InprocServer32"), None, None, REG_OPTION_NON_VOLATILE, KEY_WRITE, None, &mut inproc, None).is_ok();
        let _ = RegCloseKey(h);
        if !ok {
            return HRESULT(0x8004_0201_u32 as i32); // SELFREG_E_CLASS
        }
        let model: Vec<u16> = "Both".encode_utf16().chain(Some(0)).collect();
        let _ = RegSetValueExW(inproc, PCWSTR::null(), None, REG_SZ, Some(std::slice::from_raw_parts(path.as_ptr() as *const u8, path.len() * 2)));
        let _ = RegSetValueExW(inproc, windows::core::w!("ThreadingModel"), None, REG_SZ, Some(std::slice::from_raw_parts(model.as_ptr() as *const u8, model.len() * 2)));
        let _ = RegCloseKey(inproc);
    }
    S_OK
}

#[no_mangle]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    use windows::Win32::System::Registry::*;
    let key = clsid_key();
    unsafe {
        let _ = RegDeleteTreeW(HKEY_LOCAL_MACHINE, PCWSTR(key.as_ptr()));
    }
    S_OK
}
