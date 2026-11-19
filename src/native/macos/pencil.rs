//! Sidecar Apple Pencil barrel double-tap adapter (AppKit changeMode).
//!
//! All AppKit pointer and monitor lifetime handling stays in this macOS-only
//! module. The app sees only the number of pending gestures.
use block2::RcBlock;
use objc2::{
    rc::Retained,
    runtime::AnyObject,
};
use objc2_app_kit::{NSEvent, NSEventMask};
use std::{
    ptr::NonNull,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

pub struct PencilModeMonitor {
    token: Retained<AnyObject>,
    pending: Arc<AtomicU32>,
}

impl PencilModeMonitor {
    /// Must be called after SDL initializes Cocoa, on its AppKit main thread.
    pub fn install() -> Result<Self, String> {
        let pending = Arc::new(AtomicU32::new(0));
        let signal = Arc::clone(&pending);
        let handler = RcBlock::new(move |event: NonNull<NSEvent>| {
            signal.fetch_update(Ordering::AcqRel, Ordering::Relaxed, |count| {
                Some(count.saturating_add(1))
            }).ok();
            // AppKit owns the NSEvent for this callback and expects the same
            // event back for normal responder dispatch. Never retain it.
            event.as_ptr()
        });
        // SAFETY: AppKit invokes the copied block with a live NSEvent; the
        // block returns the unmodified borrowed event pointer (not a dangling
        // or synthetic pointer). The retained token is removed on this same
        // main thread before the SDL/Cocoa runtime shuts down.
        let token = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(
                NSEventMask::ChangeMode,
                &handler,
            )
        }
        .ok_or("AppKit did not create an Apple Pencil change-mode monitor")?;
        Ok(Self { token, pending })
    }

    /// Drain pending events only on OverInk's SDL main event-loop thread.
    pub fn take(&self) -> u32 {
        self.pending.swap(0, Ordering::AcqRel)
    }
}

impl Drop for PencilModeMonitor {
    fn drop(&mut self) {
        // SAFETY: The token was returned by the local-monitor registration
        // above, is still retained, and is removed before Cocoa shutdown on
        // the same main thread. AppKit no longer calls the block after this.
        unsafe { NSEvent::removeMonitor(&self.token) };
    }
}
