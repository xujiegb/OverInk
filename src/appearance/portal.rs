use crate::sdl::*;
use egui::Theme;
use futures_lite::{StreamExt, future};
use std::{
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use zbus::{
    Proxy,
    connection::Builder,
    zvariant::{OwnedValue, Value},
};

const APPEARANCE: &str = "org.freedesktop.appearance";
const COLOR_SCHEME: &str = "color-scheme";

pub struct Monitor {
    scheme: Arc<Mutex<Option<u32>>>,
    _stop: async_channel::Sender<()>,
}

impl Monitor {
    pub fn start() -> Self {
        let (stop, stopped) = async_channel::bounded::<()>(1);
        let scheme = Arc::new(Mutex::new(Some(0)));
        let update = scheme.clone();
        let wake = unsafe { SDL_RegisterEvents(1) };
        let _ = thread::Builder::new()
            .name("overink-system-theme".into())
            .spawn(move || {
                future::block_on(future::race(
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
    let connection = Builder::session()?
        .method_timeout(Duration::from_millis(500))
        .internal_executor(false)
        .build()
        .await?;
    future::race(listen(&connection, update, wake), async {
        loop {
            connection.executor().tick().await;
        }
    })
    .await
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
            signal.body().deserialize::<(&str, &str, Value<'_>)>()
        {
            publish(update, wake, scheme(&value));
        }
    }
    Ok(())
}

async fn read_scheme(proxy: &Proxy<'_>) -> zbus::Result<u32> {
    let setting = (APPEARANCE, COLOR_SCHEME);
    let value: OwnedValue = match proxy.call("ReadOne", &setting).await {
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod" =>
        {
            proxy.call("Read", &setting).await?
        }
        result => result?,
    };
    Ok(scheme(&value))
}

fn scheme(value: &Value<'_>) -> u32 {
    let value = match value {
        Value::Value(inner) => inner.as_ref(),
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
            unsafe { SDL_PushEvent(&mut event) };
        }
    }
}
