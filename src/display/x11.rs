use std::rc::Rc;
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        randr::{ConnectionExt as _, GetCrtcInfoReply, NotifyMask, SetConfig},
    },
    rust_connection::RustConnection,
};

const MAX_SCREENS: usize = 256;
const MAX_CRTCS: usize = 4096;

// SDL can retain a display while its output is disabled. Consult RandR
// before assigning a page, and fall back to SDL if the snapshot is incomplete.
pub(super) struct Randr {
    connection: Rc<RustConnection>,
    areas: Option<Vec<[i32; 4]>>,
    watch: bool,
    dirty: bool,
    disconnected: bool,
}

impl Randr {
    pub(super) fn connect() -> Option<Self> {
        let connection = crate::x11::connection().ok()?;
        let version = connection.randr_query_version(1, 3).ok()?.reply().ok()?;
        let screens = &connection.setup().roots;
        if (version.major_version, version.minor_version) < (1, 3)
            || !(1..=MAX_SCREENS).contains(&screens.len())
        {
            return None;
        }
        let mask = NotifyMask::SCREEN_CHANGE | NotifyMask::CRTC_CHANGE | NotifyMask::OUTPUT_CHANGE;
        let watch = screens.iter().all(|screen| {
            connection
                .randr_select_input(screen.root, mask)
                .is_ok_and(|request| request.check().is_ok())
        });
        Some(Self {
            connection,
            areas: None,
            watch,
            dirty: true,
            disconnected: false,
        })
    }

    pub(super) fn invalidate(&mut self) {
        self.dirty = true;
    }

    pub(super) fn poll(&mut self) -> bool {
        if self.disconnected {
            return false;
        }
        loop {
            match self.connection.poll_for_event() {
                Ok(Some(Event::RandrNotify(_) | Event::RandrScreenChangeNotify(_))) => {
                    self.dirty = true;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {
                    self.disconnected = true;
                    self.areas = None;
                    return true;
                }
            }
        }
        self.dirty |= !self.watch;
        self.dirty
    }

    pub(super) fn active_areas(&mut self) -> Option<&[[i32; 4]]> {
        if self.disconnected {
            return None;
        }
        if self.dirty {
            self.areas = self.query_areas();
            self.dirty = self.areas.is_none();
        }
        self.areas.as_deref()
    }

    fn query_areas(&self) -> Option<Vec<[i32; 4]>> {
        let resources = self
            .connection
            .setup()
            .roots
            .iter()
            .map(|screen| {
                self.connection
                    .randr_get_screen_resources_current(screen.root)
                    .ok()
            })
            .collect::<Option<Vec<_>>>()?;
        let mut areas = Vec::new();
        let mut queried = false;
        for reply in resources {
            let resources = reply.reply().ok()?;
            if resources.crtcs.len() > MAX_CRTCS {
                return None;
            }
            queried |= !resources.crtcs.is_empty();
            // Queue all requests before waiting for each reply, so a screen
            // with several outputs does not need one round trip per CRTC.
            let queries = resources
                .crtcs
                .into_iter()
                .map(|id| {
                    self.connection
                        .randr_get_crtc_info(id, resources.config_timestamp)
                        .ok()
                })
                .collect::<Option<Vec<_>>>()?;
            for reply in queries {
                if let Some(area) = active_area(&reply.reply().ok()?)? {
                    areas.push(area);
                }
            }
        }
        queried.then_some(areas)
    }
}

fn active_area(reply: &GetCrtcInfoReply) -> Option<Option<[i32; 4]>> {
    if reply.status != SetConfig::SUCCESS {
        return None;
    }
    Some(
        (reply.mode != 0 && reply.width > 0 && reply.height > 0).then_some([
            i32::from(reply.x),
            i32::from(reply.y),
            i32::from(reply.width),
            i32::from(reply.height),
        ]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn randr_distinguishes_disabled_outputs_from_a_failed_query() {
        let mut reply = GetCrtcInfoReply {
            x: -1920,
            width: 1920,
            height: 1080,
            mode: 1,
            ..GetCrtcInfoReply::default()
        };
        assert_eq!(active_area(&reply), Some(Some([-1920, 0, 1920, 1080])));
        reply.mode = 0;
        assert_eq!(active_area(&reply), Some(None));
        reply.status = SetConfig::INVALID_CONFIG_TIME;
        assert_eq!(active_area(&reply), None);
    }
}
