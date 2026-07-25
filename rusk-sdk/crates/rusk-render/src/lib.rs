//! rusk-render: turns a bare `ANativeActivity_onCreate` entry point into
//! an actual render loop that puts pixels on screen.
//!
//! # Why the screen is black without this
//!
//! `android.app.NativeActivity` (what every `rusk new` project's
//! manifest declares as its launcher activity) creates a native window
//! and hands your code a `*mut ANativeWindow` through the
//! `onNativeWindowCreated` callback — and then does *nothing else*. No
//! swapchain, no draw loop, no clear color. A minimal `onCreate` that
//! only implements the required callback contract genuinely produces a
//! window with undefined/black contents, on any language — this isn't a
//! Rust-specific gap, and there is no way to "translate Rust into Java"
//! that would change it, since Java's own `NativeActivity` equivalent
//! has exactly the same empty-by-default behavior. What's missing is a
//! render loop, not a different source language.
//!
//! # What this crate does
//!
//! [`AppShell`] is a safe wrapper around the raw `rusk-ndk-sys` FFI that:
//! - Implements the full `ANativeActivityCallbacks` table so window
//!   create/resize/destroy and input-queue create/destroy are tracked
//!   correctly — the #1 cause of a black (or, worse, *intermittently*
//!   black after a screen rotation) screen in hand-rolled NativeActivity
//!   code is missing the resize/destroy half of this contract, not just
//!   the creation half.
//! - Drives an `ALooper`-based event loop, polling both input events and
//!   the window lifecycle without spinning the CPU when idle.
//! - On every frame where a window exists, locks it via
//!   `ANativeWindow_lock`, hands the caller a safe [`FrameBuffer`] view
//!   over the pixel memory, and unlocks/posts it — a complete, correct
//!   present cycle with no OpenGL/Vulkan setup required. This is
//!   deliberately software rendering: it has no GPU-driver dependency,
//!   works identically on every device/emulator, and is enough to prove
//!   "something is drawing" (a solid color, a gradient, a CPU-rasterized
//!   sprite) before reaching for a GPU API.
//!
//! For GPU-accelerated rendering, [`AppShell::native_window`] exposes
//! the raw `*mut ANativeWindow` pointer at the same lifecycle points, so
//! a project can hand it to `wgpu`'s
//! `raw_window_handle::RawWindowHandle::AndroidNdk` (or `glutin`'s
//! EGL/Android surface constructor) instead of using [`FrameBuffer`] —
//! the window lifecycle tracking is the part that's easy to get subtly
//! wrong and is shared by both paths; which renderer sits on top is not
//! this crate's decision to make.

use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, Ordering};

use rusk_ndk_sys::input::AInputQueue;
use rusk_ndk_sys::looper::{self, ALooper};
use rusk_ndk_sys::native_activity::{ANativeActivity, ANativeActivityCallbacks, ARect};
use rusk_ndk_sys::native_window::{self, ANativeWindow, ANativeWindow_Buffer, WindowFormat};

pub mod input;

/// A locked, writable view over one frame's pixel buffer. Pixel format
/// is fixed at RGBA8888 (see the render loop's `setBuffersGeometry`
/// call) so `put_pixel`'s channel order is always predictable regardless
/// of the device's native framebuffer format.
pub struct FrameBuffer<'a> {
    width: i32,
    height: i32,
    stride_pixels: i32,
    bits: &'a mut [u8],
}

impl<'a> FrameBuffer<'a> {
    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn height(&self) -> i32 {
        self.height
    }

    /// Sets one pixel to an `0xRRGGBBAA` color. Silently ignores
    /// out-of-bounds coordinates rather than panicking — a render loop
    /// computing coordinates from time-varying state (animation,
    /// physics) shouldn't crash the whole app over one frame's rounding
    /// error landing a pixel just outside the window.
    pub fn put_pixel(&mut self, x: i32, y: i32, rgba: u32) {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return;
        }
        let offset = ((y * self.stride_pixels + x) * 4) as usize;
        if offset + 4 > self.bits.len() {
            return;
        }
        let [r, g, b, a] = rgba.to_be_bytes();
        self.bits[offset] = r;
        self.bits[offset + 1] = g;
        self.bits[offset + 2] = b;
        self.bits[offset + 3] = a;
    }

    /// Fills the entire buffer with one color — the one-line fix for a
    /// black screen: call this with any non-black color at the top of
    /// `draw` and you have visible proof the render loop is running
    /// before writing any real drawing logic.
    pub fn clear(&mut self, rgba: u32) {
        let [r, g, b, a] = rgba.to_be_bytes();
        for row in 0..self.height {
            let row_start = (row * self.stride_pixels * 4) as usize;
            for col in 0..self.width {
                let offset = row_start + (col * 4) as usize;
                if offset + 4 <= self.bits.len() {
                    self.bits[offset] = r;
                    self.bits[offset + 1] = g;
                    self.bits[offset + 2] = b;
                    self.bits[offset + 3] = a;
                }
            }
        }
    }

    /// Direct access to the raw RGBA8888 bytes, row-major with
    /// `stride_pixels()` possibly wider than `width()` (the OS may pad
    /// each row for alignment) — for callers doing bulk copies (e.g.
    /// blitting a CPU-rasterized image) who want to skip the per-pixel
    /// bounds-checked path.
    pub fn raw_bytes_mut(&mut self) -> &mut [u8] {
        self.bits
    }

    pub fn stride_pixels(&self) -> i32 {
        self.stride_pixels
    }
}

/// Why [`AppShell::run`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// `onDestroy` was called — the activity is being torn down.
    Destroyed,
    /// The `draw` callback returned `false`, requesting a clean exit.
    DrawRequestedExit,
}

/// Shared state written from the `ANativeActivityCallbacks` (called on
/// Android's own UI/binder threads) and read from the render loop
/// running on whichever thread called [`AppShell::run`]. Every field is
/// atomic specifically because these two sides run concurrently — this
/// is the actual hard part of hand-rolled NativeActivity code, and the
/// part most tutorials get subtly wrong by using plain (non-atomic,
/// non-synchronized) globals.
struct SharedState {
    window: AtomicPtr<ANativeWindow>,
    input_queue: AtomicPtr<AInputQueue>,
    window_generation: AtomicI32,
    should_destroy: AtomicBool,
    has_focus: AtomicBool,
}

impl SharedState {
    fn new() -> Self {
        Self {
            window: AtomicPtr::new(std::ptr::null_mut()),
            input_queue: AtomicPtr::new(std::ptr::null_mut()),
            window_generation: AtomicI32::new(0),
            should_destroy: AtomicBool::new(false),
            has_focus: AtomicBool::new(true),
        }
    }
}

// SAFETY: every field is a lock-free atomic; SharedState has no
// interior mutability outside of them, so sharing `&SharedState` across
// the callback threads and the render-loop thread is sound.
unsafe impl Sync for SharedState {}

/// The entry point a project's `ANativeActivity_onCreate` calls into.
/// Sets up the callback table, then blocks running the render loop on
/// the calling thread until the activity is destroyed or `draw` asks to
/// exit.
pub struct AppShell {
    state: &'static SharedState,
    #[allow(dead_code)]
    activity: *mut ANativeActivity,
}

impl AppShell {
    /// # Safety
    /// `activity` must be the pointer Android passed to
    /// `ANativeActivity_onCreate` for the lifetime of this call — this
    /// is always true when called directly from that function, which is
    /// the only supported call site.
    pub unsafe fn run(
        activity: *mut ANativeActivity,
        mut draw: impl FnMut(&mut FrameBuffer, f32) -> bool,
    ) -> ExitReason {
        // Leaked deliberately: the callbacks table Android holds a
        // pointer to must outlive this function call (Android may still
        // invoke a queued callback after onCreate returns, before the
        // activity is actually destroyed), and this function does not
        // return until the activity's lifetime is genuinely over, at
        // which point the process is torn down anyway.
        let state: &'static SharedState = Box::leak(Box::new(SharedState::new()));

        let callbacks = Box::leak(Box::new(ANativeActivityCallbacks {
            on_start,
            on_resume,
            on_save_instance_state,
            on_pause,
            on_stop,
            on_destroy,
            on_window_focus_changed,
            on_native_window_created,
            on_native_window_resized,
            on_native_window_redraw_needed,
            on_native_window_destroyed,
            on_input_queue_created,
            on_input_queue_destroyed,
            on_content_rect_changed,
            on_configuration_changed,
            on_low_memory,
        }));

        // `instance` is how the callbacks (which only receive the
        // ANativeActivity pointer, not any context we control) find
        // their way back to `state` — Android's contract explicitly
        // reserves this field for exactly this purpose.
        (*activity).instance = state as *const SharedState as *mut c_void;
        (*activity).callbacks = callbacks as *mut ANativeActivityCallbacks;

        let _ = looper::ALooper_prepare(looper::ALOOPER_PREPARE_ALLOW_NON_CALLBACKS);

        let shell = AppShell { state, activity };
        shell.render_loop(&mut draw)
    }

    /// The current native window, or `None` between `onNativeWindowDestroyed`
    /// and the next `onNativeWindowCreated` (e.g. briefly during a screen
    /// rotation, or while the app is backgrounded on some OEM skins).
    /// Exposed for GPU-renderer integration (see module docs); the
    /// built-in [`FrameBuffer`] path in [`run`](Self::run) already
    /// handles this correctly and most projects won't call this
    /// directly.
    pub fn native_window(&self) -> Option<*mut ANativeWindow> {
        let ptr = self.state.window.load(Ordering::Acquire);
        if ptr.is_null() {
            None
        } else {
            Some(ptr)
        }
    }

    /// The current input queue, or `None` between
    /// `onInputQueueDestroyed` and the next `onInputQueueCreated` —
    /// symmetric with [`native_window`](Self::native_window). Most
    /// projects should prefer [`drain_input_events`](Self::drain_input_events)
    /// over calling this directly.
    pub fn input_queue(&self) -> Option<*mut AInputQueue> {
        let ptr = self.state.input_queue.load(Ordering::Acquire);
        if ptr.is_null() {
            None
        } else {
            Some(ptr)
        }
    }

    /// Safely drains and returns every currently queued input event —
    /// see [`crate::input::drain_events`] for the full behavior
    /// contract. Returns an empty `Vec` (not an error) when there is no
    /// live input queue, since "no input this frame" is the normal case
    /// on most frames, not an exceptional one.
    pub fn drain_input_events(&self) -> Vec<crate::input::InputEvent> {
        match self.input_queue() {
            // SAFETY: input_queue() only returns a non-null pointer
            // while onInputQueueDestroyed hasn't fired for it yet, which
            // is exactly the liveness guarantee drain_events requires.
            Some(queue) => unsafe { crate::input::drain_events(queue) },
            None => Vec::new(),
        }
    }

    fn render_loop(&self, draw: &mut impl FnMut(&mut FrameBuffer, f32) -> bool) -> ExitReason {
        let mut last_frame = std::time::Instant::now();
        let mut configured_generation = -1;

        loop {
            // Drain pending lifecycle/input events without blocking once
            // a window exists (timeout 0 = poll, don't sleep — we have a
            // frame to render), but block indefinitely while there's no
            // window to draw into, so a backgrounded app doesn't spin
            // the CPU waiting on nothing.
            let has_window = self.native_window().is_some();
            let timeout_ms = if has_window { 0 } else { -1 };
            let mut fd = 0;
            let mut events = 0;
            let mut data: *mut c_void = std::ptr::null_mut();
            unsafe {
                looper::ALooper_pollAll(timeout_ms, &mut fd, &mut events, &mut data);
            }

            if self.state.should_destroy.load(Ordering::Acquire) {
                return ExitReason::Destroyed;
            }

            let Some(window) = self.native_window() else {
                continue;
            };

            let generation = self.state.window_generation.load(Ordering::Acquire);
            if generation != configured_generation {
                // A new (or resized) window needs its buffer geometry
                // (re)declared before the first lock — skipping this on
                // resize is the other common cause of a corrupted or
                // black display after rotating the device.
                unsafe {
                    native_window::ANativeWindow_setBuffersGeometry(
                        window,
                        0, // 0 = keep the window's current width
                        0, // 0 = keep the window's current height
                        WindowFormat::Rgba8888 as i32,
                    );
                }
                configured_generation = generation;
            }

            let now = std::time::Instant::now();
            let dt = (now - last_frame).as_secs_f32();
            last_frame = now;

            let mut native_buffer: ANativeWindow_Buffer = unsafe { std::mem::zeroed() };
            let lock_result =
                unsafe { native_window::ANativeWindow_lock(window, &mut native_buffer, std::ptr::null_mut()) };
            if lock_result != 0 {
                // Window became invalid between the pollAll and the lock
                // (e.g. destroyed concurrently) — try again next
                // iteration rather than treating this as fatal.
                continue;
            }

            let byte_len = (native_buffer.stride as usize) * (native_buffer.height as usize) * 4;
            let bits = unsafe { std::slice::from_raw_parts_mut(native_buffer.bits as *mut u8, byte_len) };
            let mut fb = FrameBuffer {
                width: native_buffer.width,
                height: native_buffer.height,
                stride_pixels: native_buffer.stride,
                bits,
            };

            let keep_running = draw(&mut fb, dt);

            unsafe {
                native_window::ANativeWindow_unlockAndPost(window);
            }

            if !keep_running {
                return ExitReason::DrawRequestedExit;
            }
        }
    }
}

/// # Safety
/// `activity` must be non-null and its `instance` field must already
/// point at a live `SharedState` — true for every call site in this
/// file, all of which run inside callbacks Android only invokes after
/// `AppShell::run` has set `instance` up.
unsafe fn state_from<'a>(activity: *mut ANativeActivity) -> &'a SharedState {
    &*((*activity).instance as *const SharedState)
}

extern "C" fn on_start(_activity: *mut ANativeActivity) {}
extern "C" fn on_resume(_activity: *mut ANativeActivity) {}
extern "C" fn on_save_instance_state(
    _activity: *mut ANativeActivity,
    out_len: *mut usize,
) -> *mut c_void {
    unsafe { *out_len = 0 };
    std::ptr::null_mut()
}
extern "C" fn on_pause(_activity: *mut ANativeActivity) {}
extern "C" fn on_stop(_activity: *mut ANativeActivity) {}

extern "C" fn on_destroy(activity: *mut ANativeActivity) {
    unsafe {
        let state = state_from(activity);
        state.should_destroy.store(true, Ordering::Release);
        let looper = looper::ALooper_forThread();
        if !looper.is_null() {
            looper::ALooper_wake(looper);
        }
    }
}

extern "C" fn on_window_focus_changed(activity: *mut ANativeActivity, has_focus: std::os::raw::c_int) {
    unsafe { state_from(activity) }.has_focus.store(has_focus != 0, Ordering::Release);
}

extern "C" fn on_native_window_created(activity: *mut ANativeActivity, window: *mut c_void) {
    let state = unsafe { state_from(activity) };
    state.window.store(window as *mut ANativeWindow, Ordering::Release);
    state.window_generation.fetch_add(1, Ordering::AcqRel);
}

extern "C" fn on_native_window_resized(activity: *mut ANativeActivity, _window: *mut c_void) {
    // The window pointer itself doesn't change on resize, only its
    // dimensions — bumping the generation counter is enough to make the
    // render loop re-declare buffer geometry on the next frame.
    unsafe { state_from(activity) }.window_generation.fetch_add(1, Ordering::AcqRel);
}

extern "C" fn on_native_window_redraw_needed(_activity: *mut ANativeActivity, _window: *mut c_void) {}

extern "C" fn on_native_window_destroyed(activity: *mut ANativeActivity, _window: *mut c_void) {
    unsafe { state_from(activity) }.window.store(std::ptr::null_mut(), Ordering::Release);
}

extern "C" fn on_input_queue_created(activity: *mut ANativeActivity, queue: *mut c_void) {
    unsafe { state_from(activity) }
        .input_queue
        .store(queue as *mut AInputQueue, Ordering::Release);
}

extern "C" fn on_input_queue_destroyed(activity: *mut ANativeActivity, _queue: *mut c_void) {
    unsafe { state_from(activity) }
        .input_queue
        .store(std::ptr::null_mut(), Ordering::Release);
}

extern "C" fn on_content_rect_changed(_activity: *mut ANativeActivity, _rect: *const ARect) {}
extern "C" fn on_configuration_changed(_activity: *mut ANativeActivity) {}
extern "C" fn on_low_memory(_activity: *mut ANativeActivity) {}
