use crate::{
    model::{Document, History, MAX_FILE_BYTES},
    sdl::*,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CStr,
    fs::{self, File},
    io::{Read, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod x11;

#[derive(Clone, Debug, PartialEq)]
pub struct Display {
    pub id: u32,
    pub name: String,
    pub bounds: [i32; 4],
    pub refresh: f64,
    pub scale: f32,
}
pub struct Topology {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    randr: Option<x11::Randr>,
}
impl Topology {
    pub fn new(_video: &str) -> Self {
        Self {
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            randr: (_video == "x11").then(x11::Randr::new).flatten(),
        }
    }
    pub fn connected(&self, _window: &Window) -> Result<Vec<Display>, String> {
        let result = sdl_displays()?;
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        let result = if let Some(areas) = self
            .randr
            .as_ref()
            .and_then(|api| api.active_areas(_window))
        {
            result
                .into_iter()
                .filter(|d| areas.contains(&d.bounds))
                .collect()
        } else {
            result
        };
        Ok(result)
    }
}
fn sdl_displays() -> Result<Vec<Display>, String> {
    unsafe {
        let mut count = 0;
        let list = SDL_GetDisplays(&mut count);
        if list.is_null() {
            return Err(crate::sdl::error());
        }
        let result = (0..count.max(0) as usize)
            .map(|i| {
                let id = *list.add(i);
                let mut bounds = SDL_Rect::default();
                if !SDL_GetDisplayBounds(id, &mut bounds) {
                    return Err(crate::sdl::error());
                }
                let name = SDL_GetDisplayName(id);
                let mode = SDL_GetCurrentDisplayMode(id);
                let refresh = if mode.is_null() {
                    0.0
                } else if (*mode).refresh_rate_numerator > 0 && (*mode).refresh_rate_denominator > 0
                {
                    (*mode).refresh_rate_numerator as f64 / (*mode).refresh_rate_denominator as f64
                } else {
                    (*mode).refresh_rate as f64
                };
                Ok(Display {
                    id: id.0,
                    name: if name.is_null() {
                        format!("Display {}", i + 1)
                    } else {
                        CStr::from_ptr(name).to_string_lossy().into_owned()
                    },
                    bounds: [bounds.x, bounds.y, bounds.w, bounds.h],
                    refresh,
                    scale: SDL_GetDisplayContentScale(id),
                })
            })
            .collect::<Result<Vec<_>, String>>();
        SDL_free(list.cast());
        result
    }
}

// SDL instance IDs identify live outputs. Use names and positions on disk.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct DisplayKey {
    name: String,
    origin: [i32; 2],
    slot: u32,
}

const SESSION_VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
struct Session<D> {
    version: u32,
    pages: Vec<(DisplayKey, D)>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SessionFile {
    Multi(Session<Document>),
    Single(Document),
}

#[derive(Default)]
pub struct Pages {
    pub active: Option<u32>,
    parked: BTreeMap<u32, History>,
    keys: BTreeMap<u32, DisplayKey>,
    unmatched: BTreeMap<DisplayKey, Document>,
    changed: bool,
}
impl Pages {
    pub fn new(active: Option<u32>) -> Self {
        Self {
            active,
            ..Self::default()
        }
    }
    pub fn switch(&mut self, history: &mut History, id: u32) -> bool {
        if self.active == Some(id) {
            return false;
        }
        if let Some(previous) = self.active {
            self.parked.insert(previous, std::mem::take(history));
        }
        if let Some(next) = self.parked.remove(&id) {
            *history = next;
        }
        self.active = Some(id);
        true
    }
    fn retain(&mut self, history: &mut History, connected: &BTreeSet<u32>) -> bool {
        self.parked.retain(|id, page| {
            let keep = connected.contains(id);
            self.changed |= !keep && (page.changed || !page.doc.strokes.is_empty());
            keep
        });
        if self.active.is_some_and(|id| !connected.contains(&id)) {
            self.changed |= history.changed || !history.doc.strokes.is_empty();
            *history = History::default();
            self.active = None;
            true
        } else {
            false
        }
    }
    pub fn sync(&mut self, history: &mut History, displays: &[Display]) -> bool {
        let ids = displays.iter().map(|d| d.id).collect();
        let removed = self.retain(history, &ids);
        self.keys.retain(|id, _| ids.contains(id));
        for display in displays {
            if self.keys.contains_key(&display.id) {
                continue;
            }
            let mut key = DisplayKey {
                name: display.name.clone(),
                origin: [display.bounds[0], display.bounds[1]],
                slot: 0,
            };
            while self.keys.values().any(|used| *used == key) {
                key.slot += 1;
            }
            if let Some(doc) = self.unmatched.remove(&key) {
                let page = if self.active == Some(display.id) {
                    &mut *history
                } else {
                    self.parked.entry(display.id).or_default()
                };
                page.set_document(doc, true);
            }
            self.keys.insert(display.id, key);
        }
        removed
    }
    pub fn needs_save(&self, history: &History) -> bool {
        self.changed || history.changed || self.parked.values().any(|page| page.changed)
    }
    pub fn mark_saved(&mut self, history: &mut History) {
        self.changed = false;
        history.changed = false;
        for page in self.parked.values_mut() {
            page.changed = false;
        }
    }
    pub fn save(&self, history: &History, path: &Path) -> Result<(), String> {
        if self.active.is_none() && !history.doc.strokes.is_empty() {
            return Err("No display for the restored legacy page".into());
        }
        let mut pages = self
            .unmatched
            .iter()
            .map(|(key, doc)| (key.clone(), doc))
            .collect::<BTreeMap<_, _>>();
        let blank = Document::default();
        for (id, key) in &self.keys {
            let doc = if self.active == Some(*id) {
                &history.doc
            } else {
                self.parked.get(id).map_or(&blank, |page| &page.doc)
            };
            doc.validate()?;
            pages.insert(key.clone(), doc);
        }
        let session = Session {
            version: SESSION_VERSION,
            pages: pages.into_iter().collect(),
        };
        let data = serde_json::to_vec(&session).map_err(|e| e.to_string())?;
        if data.len() as u64 > MAX_FILE_BYTES {
            return Err("Session exceeds 64 MiB".into());
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // Create a unique sibling before replacing the previous session.
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("overink-{}-{serial}.tmp", std::process::id()));
        let mut file = File::create_new(&tmp).map_err(|e| e.to_string())?;
        let written = file.write_all(&data).and_then(|()| file.sync_all());
        drop(file);
        let result = written.and_then(|()| fs::rename(&tmp, path));
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result.map_err(|e| e.to_string())
    }
    pub fn load(
        &mut self,
        history: &mut History,
        path: &Path,
        restore: bool,
    ) -> Result<(), String> {
        let mut data = Vec::new();
        File::open(path)
            .and_then(|file| file.take(MAX_FILE_BYTES + 1).read_to_end(&mut data))
            .map_err(|e| e.to_string())?;
        if data.len() as u64 > MAX_FILE_BYTES {
            return Err("Session exceeds 64 MiB".into());
        }
        match serde_json::from_slice(&data).map_err(|e| e.to_string())? {
            SessionFile::Single(doc) => {
                doc.validate()?;
                history.set_document(doc, restore);
                self.changed = true;
            }
            SessionFile::Multi(session) => {
                if session.version != SESSION_VERSION {
                    return Err("Unsupported session version".into());
                }
                let mut docs = BTreeMap::new();
                for (key, doc) in session.pages {
                    doc.validate()?;
                    if docs.insert(key, doc).is_some() {
                        return Err("Duplicate display in session".into());
                    }
                }
                let mut relocated = false;
                for (id, key) in &self.keys {
                    let doc = docs.remove(key).or_else(|| {
                        // A uniquely named display may move between runs.
                        if self.keys.values().filter(|k| k.name == key.name).count() != 1 {
                            return None;
                        }
                        let mut candidates = docs.keys().filter(|k| k.name == key.name);
                        let candidate = candidates.next()?.clone();
                        if candidates.next().is_some() {
                            return None;
                        }
                        relocated = true;
                        docs.remove(&candidate)
                    });
                    let page = if self.active == Some(*id) {
                        Some(&mut *history)
                    } else if doc.is_some() {
                        Some(self.parked.entry(*id).or_default())
                    } else {
                        self.parked.get_mut(id)
                    };
                    if let Some(page) = page {
                        page.set_document(doc.unwrap_or_default(), restore);
                    }
                }
                self.unmatched = docs;
                self.changed = !restore || relocated;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        color::InkColor,
        model::{Point, Stroke, Tool},
    };
    fn draw(history: &mut History, x: f32) {
        history.active = Some(Stroke {
            tool: Tool::Pen,
            color: InkColor::default(),
            width: 5.0,
            opacity: 1.0,
            pressure_curve: 1.0,
            pressure_enabled: true,
            points: vec![Point::new(x, 10.0, 0.5, [0.0; 2])],
        });
        history.commit();
    }
    fn screen(id: u32, name: &str, x: i32) -> Display {
        Display {
            id,
            name: name.into(),
            bounds: [x, 0, 800, 600],
            scale: 1.,
            refresh: 60.,
        }
    }
    #[test]
    fn saved_session_restores_parked_pages_and_forgets_unplugged_displays() {
        let path = std::env::temp_dir().join(format!("overink-pages-{}.json", std::process::id()));
        let mut pages = Pages::new(Some(10));
        let mut history = History::default();
        pages.sync(
            &mut history,
            &[screen(10, "Left", 0), screen(20, "Right", 800)],
        );
        draw(&mut history, 11.);
        pages.switch(&mut history, 20);
        assert!(history.doc.strokes.is_empty());
        assert!(pages.needs_save(&history));
        pages.save(&history, &path).unwrap();
        pages.mark_saved(&mut history);
        assert!(!pages.needs_save(&history));
        draw(&mut history, 22.);
        pages.save(&history, &path).unwrap();

        // SDL display IDs and list order can differ after a restart.
        let right = screen(31, "Right", 800);
        let left = screen(32, "Left", 0);
        let mut restored = Pages::new(Some(right.id));
        let mut history = History::default();
        restored.sync(&mut history, &[right.clone(), left.clone()]);
        restored.load(&mut history, &path, true).unwrap();
        assert_eq!(history.doc.strokes[0].points[0].x, 22.);
        restored.switch(&mut history, left.id);
        assert_eq!(history.doc.strokes[0].points[0].x, 11.);
        history.undo();
        assert_eq!(history.doc.strokes.len(), 1);
        assert!(!restored.needs_save(&history));
        restored.sync(&mut history, std::slice::from_ref(&left));
        assert!(restored.needs_save(&history));
        restored.save(&history, &path).unwrap();
        restored.mark_saved(&mut history);
        restored.sync(&mut history, &[left, right.clone()]);
        restored.switch(&mut history, right.id);
        assert!(history.doc.strokes.is_empty());
        restored.load(&mut history, &path, true).unwrap();
        assert!(history.doc.strokes.is_empty());
        fs::remove_file(&path).unwrap();
    }
    #[test]
    fn legacy_session_startup_has_no_undo_or_dirty_document() {
        let path = std::env::temp_dir().join(format!("overink-legacy-{}.json", std::process::id()));
        let mut legacy = History::default();
        draw(&mut legacy, 15.);
        fs::write(&path, serde_json::to_vec(&legacy.doc).unwrap()).unwrap();
        let mut restored = Pages::new(Some(20));
        let mut history = History::default();
        restored.sync(&mut history, &[screen(20, "Right", 800)]);
        restored.load(&mut history, &path, true).unwrap();
        assert!(!history.changed);
        history.undo();
        assert_eq!(history.doc.strokes[0].points[0].x, 15.);
        assert!(restored.needs_save(&history));
        restored.save(&history, &path).unwrap();
        let file: Session<Document> = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(file.version, SESSION_VERSION);
        assert_eq!(file.pages[0].0.name, "Right");
        fs::remove_file(&path).unwrap();
    }
    #[test]
    fn switching_screens_keeps_separate_ink_and_undo() {
        let mut pages = Pages::new(Some(10));
        let mut history = History::default();
        draw(&mut history, 11.0);
        assert!(pages.switch(&mut history, 20));
        assert!(history.doc.strokes.is_empty());
        draw(&mut history, 22.0);
        pages.switch(&mut history, 10);
        assert_eq!(history.doc.strokes[0].points[0].x, 11.0);
        history.undo();
        assert!(history.doc.strokes.is_empty());
        pages.switch(&mut history, 20);
        assert_eq!(history.doc.strokes[0].points[0].x, 22.0);
    }
    #[test]
    fn disconnect_drops_active_and_parked_ink_including_undo_redo() {
        let mut pages = Pages::new(Some(10));
        let mut history = History::default();
        draw(&mut history, 11.0);
        pages.switch(&mut history, 20);
        draw(&mut history, 22.0);
        assert!(pages.retain(&mut history, &BTreeSet::new()));
        assert!(history.doc.strokes.is_empty());
        history.undo();
        history.redo();
        assert!(history.doc.strokes.is_empty());
        pages.switch(&mut history, 10);
        history.undo();
        history.redo();
        assert!(history.doc.strokes.is_empty());
    }
    #[test]
    fn removing_an_inactive_screen_preserves_the_active_page() {
        let mut pages = Pages::new(Some(10));
        let mut history = History::default();
        draw(&mut history, 11.0);
        pages.switch(&mut history, 20);
        draw(&mut history, 22.0);
        assert!(!pages.retain(&mut history, &BTreeSet::from([20])));
        assert_eq!(history.doc.strokes[0].points[0].x, 22.0);
        pages.switch(&mut history, 10);
        assert!(history.doc.strokes.is_empty());
    }
}
