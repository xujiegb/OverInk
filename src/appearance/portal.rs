#![forbid(unsafe_code)]

use crate::sdl::*;
use egui::Theme;
use futures_lite::{StreamExt, future};
use std::{
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::Duration,
};
use zbus::{OwnedVariant, Proxy, Variant, connection::Builder};

const APPEARANCE: &str = "org.freedesktop.appearance";
const COLOR_SCHEME: &str = "color-scheme";
static WAKE_EVENT: OnceLock<u32> = OnceLock::new();

pub struct Monitor {
    scheme: Arc<Mutex<Option<u32>>>,
    _stop: async_channel::Sender<()>,
}

impl Monitor {
    pub fn start() -> Self {
        let (stop, stopped) = async_channel::bounded::<()>(1);
        let scheme = Arc::new(Mutex::new(Some(0)));
        let update = scheme.clone();
        let wake = *WAKE_EVENT.get_or_init(crate::sdl::register_user_event);
        let _ = thread::Builder::new()
            .name("overink-system-theme".into())
            .spawn(move || {
                zbus::block_on(future::race(
                    async {
                        let _ = watch(&update, wake).await;
                    },
                    async {
                        let _ = stopped.recv().await;
                    },
                ));
            });
        Self {
            scheme,
            _stop: stop,
        }
    }

    pub fn theme(&self) -> Option<Theme> {
        match *self.scheme.lock().unwrap() {
            Some(1) => Some(Theme::Dark),
            Some(2) => Some(Theme::Light),
            _ => None,
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        // Serialize the last wake with teardown of the SDL event queue.
        *self.scheme.lock().unwrap() = None;
    }
}

async fn watch(update: &Mutex<Option<u32>>, wake: u32) -> zbus::Result<()> {
    let connection = Builder::session()
        .method_timeout(Duration::from_millis(500))
        .build()
        .await?;
    listen(&connection, update, wake).await
}

async fn listen(
    connection: &zbus::Connection,
    update: &Mutex<Option<u32>>,
    wake: u32,
) -> zbus::Result<()> {
    let proxy = Proxy::new(
        connection,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Settings",
    )
    .await?;
    // Probe before subscribing. Missing portal services end the worker here.
    let initial = read_scheme(&proxy).await?;
    publish(update, wake, initial);
    let mut signals = proxy.receive_signal("SettingChanged").await?;
    publish(update, wake, read_scheme(&proxy).await.unwrap_or(initial));
    while let Some(signal) = signals.next().await {
        if let Ok((APPEARANCE, COLOR_SCHEME, value)) =
            signal.body().deserialize::<(&str, &str, Variant<'_>)>()
        {
            publish(update, wake, scheme(&value));
        }
    }
    Ok(())
}

async fn read_scheme(proxy: &Proxy<'_>) -> zbus::Result<u32> {
    let setting = (APPEARANCE, COLOR_SCHEME);
    let value: OwnedVariant = match proxy.call("ReadOne", &setting).await {
        Err(zbus::Error::FDO(error))
            if matches!(error.as_ref(), zbus::fdo::Error::UnknownMethod(_)) =>
        {
            proxy.call("Read", &setting).await?
        }
        result => result?,
    };
    Ok(scheme(&value))
}

fn scheme(value: &Variant<'_>) -> u32 {
    let value = match value {
        Variant::Variant(inner) => inner.as_ref(),
        value => value,
    };
    u32::try_from(value).unwrap_or(0)
}

fn publish(update: &Mutex<Option<u32>>, wake: u32, value: u32) {
    let mut state = update.lock().unwrap();
    if let Some(current) = state.as_mut()
        && *current != value
    {
        *current = value;
        if wake != 0 {
            let mut event = SDL_Event::default();
            event.r#type = wake;
            crate::sdl::push_event(&mut event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct LegacySettings;

    #[zbus::interface(name = "org.freedesktop.portal.Settings")]
    impl LegacySettings {
        fn read(&self, namespace: &str, key: &str) -> OwnedVariant {
            assert_eq!((namespace, key), (APPEARANCE, COLOR_SCHEME));
            OwnedVariant::try_from(Variant::Variant(Box::new(Variant::U32(1)))).unwrap()
        }
    }

    #[test]
    #[ignore = "requires an isolated session bus: dbus-run-session cargo test portal_response -- --ignored"]
    fn portal_response_falls_back_to_legacy_settings_read() {
        zbus::block_on(async {
            let name = "org.freedesktop.portal.LegacySettingsTest";
            let path = "/org/freedesktop/portal/desktop";
            let service = Builder::session()
                .name(name)
                .serve_at(path, LegacySettings)
                .build()
                .await
                .unwrap();
            let proxy = Proxy::new(&service, name, path, "org.freedesktop.portal.Settings")
                .await
                .unwrap();
            assert_eq!(read_scheme(&proxy).await.unwrap(), 1);
        });
    }
}
