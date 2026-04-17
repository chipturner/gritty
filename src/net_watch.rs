//! OS-level network path change notifications.
//!
//! On macOS this wraps `nw_path_monitor` from Network.framework, which fires
//! whenever the default network path changes -- wifi join/leave, ethernet
//! plug/unplug, VPN up/down, or a wake-from-sleep route restore. On every
//! other platform it is a no-op whose [`NetWatcher::changed`] future never
//! resolves.
//!
//! The watcher is purely advisory: callers use it to short-circuit a backoff
//! sleep or trigger an immediate liveness probe. All correctness still rests
//! on the existing wall-clock heartbeat / probe machinery.

use std::sync::Arc;
use tokio::sync::Notify;

/// Handle to a background OS network-path monitor.
///
/// Construct with [`NetWatcher::spawn`]; await [`NetWatcher::changed`] inside
/// a `select!` to be woken on the next path-change edge. On non-macOS this is
/// an inert stub.
pub struct NetWatcher {
    notify: Arc<Notify>,
    _imp: imp::Monitor,
}

impl NetWatcher {
    /// Start the platform monitor. Never fails: on error (or on platforms
    /// without an implementation) the returned watcher is inert.
    pub fn spawn() -> Self {
        let notify = Arc::new(Notify::new());
        let _imp = imp::Monitor::start(notify.clone());
        Self { notify, _imp }
    }

    /// Resolve on the next network path change.
    ///
    /// Uses `Notify::notify_one`, so at most one change is latched while no
    /// one is waiting; bursts coalesce into a single wakeup.
    pub async fn changed(&self) {
        self.notify.notified().await;
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use block2::{Block, RcBlock};
    use std::ffi::c_void;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Notify;

    // Network.framework (nw_*) -- opaque object pointers.
    #[link(name = "Network", kind = "framework")]
    unsafe extern "C" {
        fn nw_path_monitor_create() -> *mut c_void;
        fn nw_path_monitor_set_update_handler(
            monitor: *mut c_void,
            handler: &Block<dyn Fn(*mut c_void)>,
        );
        fn nw_path_monitor_set_queue(monitor: *mut c_void, queue: *mut c_void);
        fn nw_path_monitor_start(monitor: *mut c_void);
        fn nw_path_monitor_cancel(monitor: *mut c_void);
        fn nw_release(obj: *mut c_void);
    }

    // libdispatch lives in libSystem; no explicit #[link] needed on macOS.
    unsafe extern "C" {
        fn dispatch_queue_create(label: *const i8, attr: *mut c_void) -> *mut c_void;
        fn dispatch_release(obj: *mut c_void);
    }

    /// Live handle: `Some` only when the monitor actually started. The
    /// framework retains its own copy of the block, but we keep the
    /// `RcBlock` here so its lifetime is visibly tied to the monitor.
    pub(super) struct Monitor(Option<Inner>);

    struct Inner {
        monitor: *mut c_void,
        queue: *mut c_void,
        _handler: RcBlock<dyn Fn(*mut c_void)>,
    }

    // SAFETY: the raw pointers are nw/dispatch objects that are safe to cancel
    // and release from any thread; we only touch them in Drop.
    unsafe impl Send for Monitor {}
    unsafe impl Sync for Monitor {}

    impl Monitor {
        pub(super) fn start(notify: Arc<Notify>) -> Self {
            // SAFETY: straightforward FFI -- all pointers are produced by the
            // framework and only passed back to it. Null returns are handled.
            unsafe {
                let monitor = nw_path_monitor_create();
                if monitor.is_null() {
                    return Self(None);
                }
                let queue =
                    dispatch_queue_create(c"gritty.netwatch".as_ptr(), std::ptr::null_mut());
                if queue.is_null() {
                    nw_release(monitor);
                    return Self(None);
                }
                // nw_path_monitor fires once with the initial path right after
                // start(); swallow that so the first changed().await reflects
                // an actual transition, not startup.
                let primed = AtomicBool::new(false);
                let handler = RcBlock::new(move |_path: *mut c_void| {
                    if primed.swap(true, Ordering::Relaxed) {
                        notify.notify_one();
                    }
                });
                nw_path_monitor_set_update_handler(monitor, &handler);
                nw_path_monitor_set_queue(monitor, queue);
                nw_path_monitor_start(monitor);
                Self(Some(Inner { monitor, queue, _handler: handler }))
            }
        }
    }

    impl Drop for Monitor {
        fn drop(&mut self) {
            if let Some(inner) = self.0.take() {
                // SAFETY: pointers came from the create calls above and are
                // released exactly once here.
                unsafe {
                    nw_path_monitor_cancel(inner.monitor);
                    nw_release(inner.monitor);
                    dispatch_release(inner.queue);
                }
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use std::sync::Arc;
    use tokio::sync::Notify;

    pub(super) struct Monitor;

    impl Monitor {
        pub(super) fn start(_notify: Arc<Notify>) -> Self {
            Self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn spawn_and_drop_does_not_panic() {
        let w = NetWatcher::spawn();
        drop(w);
    }

    #[tokio::test]
    async fn changed_does_not_fire_spuriously_at_startup() {
        let w = NetWatcher::spawn();
        let fired = tokio::time::timeout(Duration::from_millis(200), w.changed()).await;
        assert!(fired.is_err(), "changed() resolved without a real path change");
    }
}
