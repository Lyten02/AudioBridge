//! Minimal AAudio wrapper (raw `ndk-sys` bindings, the layer under `ndk::audio`).
//!
//! Streams are 48 kHz float, shared mode (AAudio converts rate/format to the device), low-latency,
//! driven by a data callback that hands buffers straight to a core audio handle. The handle lives in a
//! heap context passed as AAudio user data and is returned to the caller when the stream is closed, so
//! it survives device reopenings.
//!
//! Usage, content type and input preset setters only exist from API 28; they are looked up with `dlsym`
//! so the library still loads on API 26/27 (where AAudio already defaults input to VOICE_RECOGNITION).

use std::cell::UnsafeCell;
use std::ffi::{c_void, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use audiobridge_core::audio::{CaptureHandle, PlayoutHandle};
use audiobridge_core::proto::SAMPLE_RATE;
use ndk_sys as ffi;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Output,
    Input,
}

/// Receives `(direction, stream generation, AAudio error code)` from a stream's error callback.
/// Called on an AAudio-internal thread; must only hand the event off.
pub type ErrorSink = Arc<dyn Fn(Dir, u64, i32) + Send + Sync>;

/// A core audio handle that can sit behind an AAudio data callback.
pub trait Endpoint: Send + 'static {
    const DIR: Dir;
    fn channels(&self) -> usize;
    /// # Safety
    /// `data` points to `frames * channels()` f32 samples owned by AAudio for the duration of the call.
    unsafe fn process(&mut self, data: *mut c_void, frames: usize);
}

impl Endpoint for PlayoutHandle {
    const DIR: Dir = Dir::Output;
    fn channels(&self) -> usize {
        PlayoutHandle::channels(self)
    }
    unsafe fn process(&mut self, data: *mut c_void, frames: usize) {
        let out = std::slice::from_raw_parts_mut(data.cast::<f32>(), frames * PlayoutHandle::channels(self));
        self.fill(out);
    }
}

impl Endpoint for CaptureHandle {
    const DIR: Dir = Dir::Input;
    fn channels(&self) -> usize {
        CaptureHandle::channels(self)
    }
    unsafe fn process(&mut self, data: *mut c_void, frames: usize) {
        let input = std::slice::from_raw_parts(data.cast::<f32>().cast_const(), frames * CaptureHandle::channels(self));
        self.push(input);
    }
}

pub fn result_text(code: i32) -> String {
    // SAFETY: AAudio returns a pointer to a static NUL-terminated string for every code.
    let p = unsafe { ffi::AAudio_convertResultToText(code) };
    if p.is_null() {
        return format!("AAudio error {code}");
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

fn check(code: i32, what: &str) -> Result<(), String> {
    if code == ffi::AAUDIO_OK {
        Ok(())
    } else {
        Err(format!("{what}: {}", result_text(code)))
    }
}

type BuilderSetter = unsafe extern "C" fn(*mut ffi::AAudioStreamBuilder, i32);

struct Api28 {
    set_usage: Option<BuilderSetter>,
    set_content_type: Option<BuilderSetter>,
    set_input_preset: Option<BuilderSetter>,
}

static API28: LazyLock<Api28> = LazyLock::new(|| {
    // SAFETY: libaaudio.so is already mapped (we link it); dlopen only bumps its refcount, which is
    // never released, so the symbols stay valid for the process lifetime.
    unsafe {
        let lib = libc::dlopen(c"libaaudio.so".as_ptr(), libc::RTLD_NOW);
        let sym = |name: &CStr| -> Option<BuilderSetter> {
            if lib.is_null() {
                return None;
            }
            let p = libc::dlsym(lib, name.as_ptr());
            (!p.is_null()).then(|| std::mem::transmute::<*mut c_void, BuilderSetter>(p))
        };
        Api28 {
            set_usage: sym(c"AAudioStreamBuilder_setUsage"),
            set_content_type: sym(c"AAudioStreamBuilder_setContentType"),
            set_input_preset: sym(c"AAudioStreamBuilder_setInputPreset"),
        }
    }
});

/// State shared with the AAudio callbacks. The handle is only touched by the data callback
/// (one thread at a time); the remaining fields are read-only or atomic.
struct Ctx<H> {
    handle: UnsafeCell<H>,
    gen: u64,
    sink: ErrorSink,
    closing: AtomicBool,
    error_in_callback: AtomicBool,
}

unsafe extern "C" fn data_callback<H: Endpoint>(
    _stream: *mut ffi::AAudioStream,
    user: *mut c_void,
    data: *mut c_void,
    frames: i32,
) -> ffi::aaudio_data_callback_result_t {
    let ctx = &*user.cast::<Ctx<H>>();
    let frames = frames.max(0) as usize;
    let handle = ctx.handle.get();
    let ok = catch_unwind(AssertUnwindSafe(|| (*handle).process(data, frames))).is_ok();
    if ok {
        ffi::AAUDIO_CALLBACK_RESULT_CONTINUE as i32
    } else {
        if H::DIR == Dir::Output {
            ptr::write_bytes(data.cast::<f32>(), 0, frames * (*handle).channels());
        }
        // The panic hook already logged it. Stopping turns into a stream error → controlled reopen.
        ffi::AAUDIO_CALLBACK_RESULT_STOP as i32
    }
}

unsafe extern "C" fn error_callback<H: Endpoint>(
    _stream: *mut ffi::AAudioStream,
    user: *mut c_void,
    error: ffi::aaudio_result_t,
) {
    let ctx = &*user.cast::<Ctx<H>>();
    ctx.error_in_callback.store(true, Ordering::SeqCst);
    if !ctx.closing.load(Ordering::SeqCst) {
        let _ = catch_unwind(AssertUnwindSafe(|| (ctx.sink)(H::DIR, ctx.gen, error)));
    }
    ctx.error_in_callback.store(false, Ordering::SeqCst);
}

/// Deletes the builder on every exit path.
struct Builder(NonNull<ffi::AAudioStreamBuilder>);

impl Drop for Builder {
    fn drop(&mut self) {
        // SAFETY: created by AAudio_createStreamBuilder and deleted exactly once.
        unsafe { ffi::AAudioStreamBuilder_delete(self.0.as_ptr()) };
    }
}

/// An open, started AAudio stream that owns the core handle `H` while open.
pub struct Stream<H: Endpoint> {
    raw: NonNull<ffi::AAudioStream>,
    ctx: NonNull<Ctx<H>>,
    gen: u64,
    opened_at: Instant,
}

// SAFETY: AAudio streams may be closed from any thread; the handle is Send.
unsafe impl<H: Endpoint> Send for Stream<H> {}

impl<H: Endpoint> Stream<H> {
    /// Opens and starts a stream for `handle`. On failure the handle is returned with the reason.
    pub fn open(handle: H, gen: u64, sink: ErrorSink) -> Result<Self, (H, String)> {
        let channels = handle.channels() as i32;
        let ctx = Box::into_raw(Box::new(Ctx {
            handle: UnsafeCell::new(handle),
            gen,
            sink,
            closing: AtomicBool::new(false),
            error_in_callback: AtomicBool::new(false),
        }));
        // SAFETY: ctx is a fresh Box pointer; it is reclaimed on every failure path below, or by `close`.
        let reclaim = |ctx: *mut Ctx<H>| unsafe { Box::from_raw(ctx).handle.into_inner() };
        match unsafe { Self::open_raw(channels, ctx) } {
            Ok(raw) => {
                let stream = Stream { raw, ctx: NonNull::new(ctx).expect("box pointer"), gen, opened_at: Instant::now() };
                // SAFETY: valid open stream.
                let start = unsafe { ffi::AAudioStream_requestStart(raw.as_ptr()) };
                if let Err(e) = check(start, "requestStart") {
                    return Err((stream.close(), e));
                }
                Ok(stream)
            }
            Err(e) => Err((reclaim(ctx), e)),
        }
    }

    unsafe fn open_raw(channels: i32, ctx: *mut Ctx<H>) -> Result<NonNull<ffi::AAudioStream>, String> {
        let mut b = ptr::null_mut();
        check(ffi::AAudio_createStreamBuilder(&mut b), "createStreamBuilder")?;
        let builder = Builder(NonNull::new(b).ok_or("createStreamBuilder returned null")?);
        let b = builder.0.as_ptr();

        let output = H::DIR == Dir::Output;
        ffi::AAudioStreamBuilder_setDirection(
            b,
            if output { ffi::AAUDIO_DIRECTION_OUTPUT } else { ffi::AAUDIO_DIRECTION_INPUT } as i32,
        );
        ffi::AAudioStreamBuilder_setSharingMode(b, ffi::AAUDIO_SHARING_MODE_SHARED as i32);
        ffi::AAudioStreamBuilder_setPerformanceMode(b, ffi::AAUDIO_PERFORMANCE_MODE_LOW_LATENCY as i32);
        ffi::AAudioStreamBuilder_setSampleRate(b, SAMPLE_RATE as i32);
        ffi::AAudioStreamBuilder_setChannelCount(b, channels);
        ffi::AAudioStreamBuilder_setFormat(b, ffi::AAUDIO_FORMAT_PCM_FLOAT);
        let api = &*API28;
        if output {
            if let Some(f) = api.set_usage {
                f(b, ffi::AAUDIO_USAGE_MEDIA as i32);
            }
            if let Some(f) = api.set_content_type {
                f(b, ffi::AAUDIO_CONTENT_TYPE_MUSIC as i32);
            }
        } else if let Some(f) = api.set_input_preset {
            // Not VOICE_COMMUNICATION: that preset can pull Bluetooth headsets into SCO/HFP and wreck A2DP.
            f(b, ffi::AAUDIO_INPUT_PRESET_VOICE_RECOGNITION as i32);
        }
        ffi::AAudioStreamBuilder_setDataCallback(b, Some(data_callback::<H>), ctx.cast());
        ffi::AAudioStreamBuilder_setErrorCallback(b, Some(error_callback::<H>), ctx.cast());

        let mut s = ptr::null_mut();
        check(ffi::AAudioStreamBuilder_openStream(b, &mut s), "openStream")?;
        drop(builder);
        let raw = NonNull::new(s).ok_or("openStream returned null")?;

        let rate = ffi::AAudioStream_getSampleRate(s);
        let got_channels = ffi::AAudioStream_getChannelCount(s);
        let format = ffi::AAudioStream_getFormat(s);
        if rate != SAMPLE_RATE as i32 || got_channels != channels || format != ffi::AAUDIO_FORMAT_PCM_FLOAT {
            ffi::AAudioStream_close(s);
            return Err(format!(
                "unsupported stream config: {rate} Hz, {got_channels} ch, format {format} (wanted {SAMPLE_RATE} Hz, {channels} ch, float)"
            ));
        }
        let burst = ffi::AAudioStream_getFramesPerBurst(s);
        if output && burst > 0 {
            // Two bursts: lowest glitch-free double buffering. The returned value is the actual size.
            ffi::AAudioStream_setBufferSizeInFrames(s, burst * 2);
        }
        log::info!(
            "{} stream opened: {} Hz, {} ch, burst {} frames, buffer {} / {} frames, device {}, perf mode {}",
            if output { "output" } else { "input" },
            rate,
            got_channels,
            burst,
            ffi::AAudioStream_getBufferSizeInFrames(s),
            ffi::AAudioStream_getBufferCapacityInFrames(s),
            ffi::AAudioStream_getDeviceId(s),
            ffi::AAudioStream_getPerformanceMode(s),
        );
        Ok(raw)
    }

    pub fn gen(&self) -> u64 {
        self.gen
    }

    pub fn age(&self) -> Duration {
        self.opened_at.elapsed()
    }

    /// Stops and closes the stream, returning the handle. No callback runs after this returns.
    pub fn close(self) -> H {
        let this = std::mem::ManuallyDrop::new(self);
        // SAFETY: `this` is never dropped, so the stream/ctx are released exactly once here.
        unsafe { this.release() }
    }

    unsafe fn release(&self) -> H {
        let ctx = self.ctx.as_ptr();
        (*ctx).closing.store(true, Ordering::SeqCst);
        // Let an in-flight error callback finish handing off its event before the context goes away.
        let deadline = Instant::now() + Duration::from_millis(200);
        while (*ctx).error_in_callback.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        // AAudioStream_close stops the stream and joins the callback thread.
        let r = ffi::AAudioStream_close(self.raw.as_ptr());
        if r != ffi::AAUDIO_OK {
            log::warn!("{:?} stream close: {}", H::DIR, result_text(r));
        }
        Box::from_raw(ctx).handle.into_inner()
    }
}

impl<H: Endpoint> Drop for Stream<H> {
    fn drop(&mut self) {
        // SAFETY: `close` bypasses Drop, so this is the only release on this path.
        drop(unsafe { self.release() });
    }
}
