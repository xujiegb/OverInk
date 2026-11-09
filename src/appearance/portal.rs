use egui::Theme;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
use zbus::{
    blocking::{Connection, Proxy, connection::Builder},
    zvariant::{OwnedValue, Value},
};

pub struct Monitor {
    scheme: Arc<AtomicU8>,
    _stop: mpsc::Sender<()>,
}

impl Monitor {
    pub fn start() -> Self {
        let (stop, stopped) = mpsc::channel();
        let scheme = Arc::new(AtomicU8::new(0));
        let update = scheme.clone();
        let _ = thread::Builder::new()
            .name("overink-system-theme".into())
            .spawn(move || {
                let mut connection = None;
                loop {
                    if connection.is_none() {
                        connection = Builder::session()
                            .and_then(|builder| {
                                builder.method_timeout(Duration::from_millis(500)).build()
                            })
                            .ok();
                    }
                    let current = connection.as_ref().and_then(read_scheme);
                    update.store(current.unwrap_or(0), Ordering::Relaxed);
                    if current.is_none() {
                        connection = None;
                    }
                    let interval = Duration::from_secs(if current.is_some() { 1 } else { 5 });
                    if stopped.recv_timeout(interval) != Err(mpsc::RecvTimeoutError::Timeout) {
                        break;
                    }
                }
            });
        Self {
            scheme,
            _stop: stop,
        }
    }

    pub fn theme(&self) -> Theme {
        if self.scheme.load(Ordering::Relaxed) == 1 {
            Theme::Dark
        } else {
            Theme::Light
        }
    }
}

fn read_scheme(connection: &Connection) -> Option<u8> {
    let proxy = Proxy::new(
        connection,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Settings",
    )
    .ok()?;
    let setting = ("org.freedesktop.appearance", "color-scheme");
    let value: OwnedValue = proxy
        .call("ReadOne", &setting)
        .or_else(|_| proxy.call("Read", &setting))
        .ok()?;
    let value = match &*value {
        Value::Value(inner) => inner.as_ref(),
        value => value,
    };
    u32::try_from(value)
        .ok()
        .and_then(|scheme| u8::try_from(scheme).ok())
}
