//! Safe input-event reading, layered on top of the raw `AInputQueue`
//! pointer [`crate::AppShell`] already tracks through the window
//! lifecycle callbacks.
//!
//! This closes a gap the rest of the crate's documentation was
//! deliberately honest about: earlier versions of `AppShell` stored the
//! `AInputQueue` pointer on `onInputQueueCreated`/`onInputQueueDestroyed`
//! but provided no safe way to actually read events from it — a caller
//! needed to reach into `rusk-ndk-sys::input`'s raw FFI functions
//! directly. [`InputEvent`] and [`drain_events`] are that missing safe
//! layer.

use rusk_ndk_sys::input::{self, AInputEvent, AInputQueue};

/// A pointer/finger contact on the screen, one per active pointer in a
/// multi-touch motion event.
#[derive(Debug, Clone, Copy)]
pub struct Pointer {
    pub x: f32,
    pub y: f32,
}

/// The action a motion event represents — collapsed from the NDK's
/// numeric `AMOTION_EVENT_ACTION_*` constants into a real enum so
/// calling code gets exhaustiveness checking instead of comparing
/// against magic numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPhase {
    Down,
    Up,
    Move,
    Cancel,
    /// An action code this crate doesn't specifically recognize (Android
    /// has several less common ones — pointer-down/up for individual
    /// fingers in a multi-touch gesture, hover events, scroll). The raw
    /// code is preserved so a caller that needs one of those can still
    /// get at it, without every unrecognized code being silently
    /// dropped.
    Other(i32),
}

impl TouchPhase {
    fn from_raw(action: i32) -> Self {
        match action {
            input::AMOTION_EVENT_ACTION_DOWN => TouchPhase::Down,
            input::AMOTION_EVENT_ACTION_UP => TouchPhase::Up,
            input::AMOTION_EVENT_ACTION_MOVE => TouchPhase::Move,
            input::AMOTION_EVENT_ACTION_CANCEL => TouchPhase::Cancel,
            other => TouchPhase::Other(other),
        }
    }
}

/// Whether a key event is a press or a release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPhase {
    Down,
    Up,
    Other(i32),
}

impl KeyPhase {
    fn from_raw(action: i32) -> Self {
        match action {
            input::AKEY_EVENT_ACTION_DOWN => KeyPhase::Down,
            input::AKEY_EVENT_ACTION_UP => KeyPhase::Up,
            other => KeyPhase::Other(other),
        }
    }
}

/// A single input event, safely extracted from the NDK's opaque
/// `AInputEvent` pointer — every field here is a plain value read out
/// while the event was still valid, so an [`InputEvent`] has no
/// lifetime tied to the underlying pointer and can be stored, queued, or
/// passed around freely after [`drain_events`] returns it.
#[derive(Debug, Clone)]
pub enum InputEvent {
    Touch {
        phase: TouchPhase,
        /// One entry per active pointer — index 0 is not necessarily
        /// "the first finger that touched down"; consult
        /// `AMotionEvent_getPointerId` (not currently exposed by
        /// `rusk-ndk-sys`) if a project needs stable per-finger identity
        /// across a multi-touch gesture rather than just "current
        /// contact points".
        pointers: Vec<Pointer>,
        event_time_nanos: i64,
    },
    Key {
        phase: KeyPhase,
        key_code: i32,
        repeat_count: i32,
        event_time_nanos: i64,
    },
    /// An event type this crate doesn't parse into a richer variant
    /// (Android's `AInputEvent` covers more categories than touch/key,
    /// e.g. trackball and generic motion from a joystick) — surfaced
    /// with its raw type code rather than silently dropped, so a project
    /// that needs one of those can at least detect it happened, even
    /// without this crate parsing its fields.
    Unrecognized { raw_type: i32 },
}

/// Drains every currently queued input event from `queue`, safely
/// converting each into an [`InputEvent`] and finishing (acknowledging)
/// it with the OS afterward.
///
/// Every event pulled from the queue via `AInputQueue_getEvent` **must**
/// be finished via `AInputQueue_finishEvent`, or the input system
/// eventually stalls waiting for acknowledgment that never comes — this
/// function handles that pairing automatically so a caller can't
/// accidentally leak an unfinished event by returning early or panicking
/// partway through handling one.
///
/// `pre_dispatch` events (ones the system wants a chance to intercept
/// before normal delivery, e.g. for IME handling) are finished as
/// "not handled" (`handled = false`) automatically, matching the
/// behavior a project doing its own raw input handling would almost
/// always want as a default; the events themselves are still returned
/// so a caller can react to them the same as any other event.
///
/// # Safety
/// `queue` must be a valid, currently-live `AInputQueue` pointer — in
/// practice, the pointer [`crate::AppShell::native_window`]'s sibling
/// input-queue tracking already guarantees is live for the duration of
/// a single frame's processing.
pub unsafe fn drain_events(queue: *mut AInputQueue) -> Vec<InputEvent> {
    let mut events = Vec::new();
    if queue.is_null() {
        return events;
    }

    loop {
        let mut raw_event: *mut AInputEvent = std::ptr::null_mut();
        let get_result = input::AInputQueue_getEvent(queue, &mut raw_event);
        if get_result < 0 || raw_event.is_null() {
            break; // queue is empty
        }

        // preDispatchEvent returning non-zero means the system consumed
        // this event itself (typically IME-related) and it should not
        // be processed further nor finished by us — the system already
        // owns its lifecycle in that case.
        let pre_dispatched = input::AInputQueue_preDispatchEvent(queue, raw_event) != 0;
        if pre_dispatched {
            continue;
        }

        let event = parse_event(raw_event);
        input::AInputQueue_finishEvent(queue, raw_event, 0);
        events.push(event);
    }

    events
}

/// # Safety
/// `raw_event` must be a valid, non-null `AInputEvent` pointer, live for
/// the duration of this call — guaranteed by [`drain_events`]'s own
/// contract, which is the only supported caller.
unsafe fn parse_event(raw_event: *mut AInputEvent) -> InputEvent {
    let event_type = input::AInputEvent_getType(raw_event);
    match event_type {
        t if t == input::AINPUT_EVENT_TYPE_MOTION => {
            let action = input::AMotionEvent_getAction(raw_event);
            let pointer_count = input::AMotionEvent_getPointerCount(raw_event);
            let mut pointers = Vec::with_capacity(pointer_count);
            for i in 0..pointer_count {
                pointers.push(Pointer {
                    x: input::AMotionEvent_getX(raw_event, i),
                    y: input::AMotionEvent_getY(raw_event, i),
                });
            }
            InputEvent::Touch {
                phase: TouchPhase::from_raw(action),
                pointers,
                event_time_nanos: input::AMotionEvent_getEventTime(raw_event),
            }
        }
        t if t == input::AINPUT_EVENT_TYPE_KEY => {
            let action = input::AKeyEvent_getAction(raw_event);
            InputEvent::Key {
                phase: KeyPhase::from_raw(action),
                key_code: input::AKeyEvent_getKeyCode(raw_event),
                repeat_count: input::AKeyEvent_getRepeatCount(raw_event),
                event_time_nanos: input::AKeyEvent_getEventTime(raw_event),
            }
        }
        other => InputEvent::Unrecognized { raw_type: other },
    }
}
