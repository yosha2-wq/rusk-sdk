//! Bindings for `<android/input.h>` — reading touch/key events off the
//! `AInputQueue` the runtime hands to `onInputQueueCreated`.

use std::os::raw::{c_int, c_void};

#[repr(C)]
pub struct AInputQueue {
    _private: [u8; 0],
}

#[repr(C)]
pub struct AInputEvent {
    _private: [u8; 0],
}

pub const AINPUT_EVENT_TYPE_KEY: i32 = 1;
pub const AINPUT_EVENT_TYPE_MOTION: i32 = 2;

pub const AMOTION_EVENT_ACTION_DOWN: i32 = 0;
pub const AMOTION_EVENT_ACTION_UP: i32 = 1;
pub const AMOTION_EVENT_ACTION_MOVE: i32 = 2;
pub const AMOTION_EVENT_ACTION_CANCEL: i32 = 3;

pub const AKEY_EVENT_ACTION_DOWN: i32 = 0;
pub const AKEY_EVENT_ACTION_UP: i32 = 1;

extern "C" {
    pub fn AInputQueue_attachLooper(
        queue: *mut AInputQueue,
        looper: *mut super::looper::ALooper,
        ident: c_int,
        callback: Option<super::looper::ALooper_callbackFunc>,
        data: *mut c_void,
    );
    pub fn AInputQueue_detachLooper(queue: *mut AInputQueue);
    pub fn AInputQueue_hasEvents(queue: *mut AInputQueue) -> i32;
    pub fn AInputQueue_getEvent(queue: *mut AInputQueue, out_event: *mut *mut AInputEvent) -> i32;
    pub fn AInputQueue_preDispatchEvent(queue: *mut AInputQueue, event: *mut AInputEvent) -> i32;
    pub fn AInputQueue_finishEvent(queue: *mut AInputQueue, event: *mut AInputEvent, handled: c_int);

    pub fn AInputEvent_getType(event: *const AInputEvent) -> i32;
    pub fn AInputEvent_getDeviceId(event: *const AInputEvent) -> i32;
    pub fn AInputEvent_getSource(event: *const AInputEvent) -> i32;

    // Motion events.
    pub fn AMotionEvent_getAction(event: *const AInputEvent) -> i32;
    pub fn AMotionEvent_getPointerCount(event: *const AInputEvent) -> usize;
    pub fn AMotionEvent_getX(event: *const AInputEvent, pointer_index: usize) -> f32;
    pub fn AMotionEvent_getY(event: *const AInputEvent, pointer_index: usize) -> f32;
    pub fn AMotionEvent_getEventTime(event: *const AInputEvent) -> i64;

    // Key events.
    pub fn AKeyEvent_getAction(event: *const AInputEvent) -> i32;
    pub fn AKeyEvent_getKeyCode(event: *const AInputEvent) -> i32;
    pub fn AKeyEvent_getRepeatCount(event: *const AInputEvent) -> i32;
    pub fn AKeyEvent_getEventTime(event: *const AInputEvent) -> i64;
}
