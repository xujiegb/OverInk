#![forbid(unsafe_code)]

use crate::model::{Document, History, MAX_FILE_BYTES, Stroke, TextBox};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
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
            randr: (_video == "x11").then(x11::Randr::connect).flatten(),
        }
    }
    pub fn invalidate(&mut self) {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if let Some(randr) = &mut self.randr {
            randr.invalidate();
        }
    }

    pub fn poll_x11(&mut self) -> bool {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if let Some(randr) = &mut self.randr {
            return randr.poll();
        }
        true
    }

    pub fn connected(&mut self) -> Result<Vec<Display>, String> {
        let displays = sdl_displays()?;
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if let Some(areas) = self.randr.as_mut().and_then(x11::Randr::active_areas) {
            return Ok(filter_x11_displays(displays, areas));
        }
        Ok(displays)
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd", test))]
fn filter_x11_displays(mut displays: Vec<Display>, areas: &[[i32; 4]]) -> Vec<Display> {
    // SDL and RandR run on separate connections. Until SDL sees the whole
    // layout, a live output can have stale bounds. Keep its page in that case.
    if areas
        .iter()
        .all(|area| displays.iter().any(|display| display.bounds == *area))
    {
        displays.retain(|display| areas.contains(&display.bounds));
    }
    displays
}
fn sdl_displays() -> Result<Vec<Display>, String> {
    crate::sdl::display_info().map(|displays| {
        displays
            .into_iter()
            .map(|info| Display {
                id: info.id,
                name: info.name,
                bounds: info.bounds,
                refresh: info.refresh,
                scale: info.scale,
            })
            .collect()
    })
}

// SDL instance IDs identify live outputs. Use names and positions on disk.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct DisplayKey {
    name: String,
    origin: [i32; 2],
    slot: u32,
}

fn take_unique_page(
    docs: &mut BTreeMap<DisplayKey, Document>,
    predicate: impl Fn(&DisplayKey, &Document) -> bool,
) -> Option<Document> {
    let mut matches = docs.iter().filter(|(key, doc)| predicate(key, doc));
    matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    docs.extract_if(.., |key, doc| predicate(key, doc))
        .next()
        .map(|(_, doc)| doc)
}

fn take_named_page(
    key: &DisplayKey,
    connected: &BTreeMap<u32, DisplayKey>,
    docs: &mut BTreeMap<DisplayKey, Document>,
) -> Option<(Document, bool)> {
    if let Some(doc) = docs.remove(key) {
        return Some((doc, false));
    }
    if connected
        .values()
        .filter(|other| other.name == key.name)
        .count()
        != 1
    {
        return None;
    }
    take_unique_page(docs, |saved, _| saved.name == key.name).map(|doc| (doc, true))
}

const SESSION_VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
struct Session<K, D> {
    version: u32,
    pages: Vec<(K, D)>,
}

// Decode the envelope directly into typed fields. The legacy and multi-page
// writers share a version field, so serde never needs an untagged value tree.
#[derive(Deserialize)]
struct SessionFile {
    version: u32,
    pages: Option<Vec<(DisplayKey, Document)>>,
    size: Option<[f32; 2]>,
    strokes: Option<Vec<Arc<Stroke>>>,
    #[serde(default)]
    text_boxes: Vec<Arc<TextBox>>,
}

fn read_session(path: &Path) -> Result<SessionFile, String> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    if file.metadata().map_err(|error| error.to_string())?.len() > MAX_FILE_BYTES {
        return Err("Session exceeds 64 MiB".into());
    }
    let mut limited = file.take(MAX_FILE_BYTES + 1);
    let result = serde_json::from_reader(BufReader::new(&mut limited));
    if limited.limit() == 0 {
        return Err("Session exceeds 64 MiB".into());
    }
    result.map_err(|error| error.to_string())
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
            self.changed |= !keep && (page.changed || !page.document().is_empty());
            keep
        });
        if self.active.is_some_and(|id| !connected.contains(&id)) {
            self.changed |= history.changed || !history.document().is_empty();
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
        let mut added = Vec::new();
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
            self.keys.insert(display.id, key);
            added.push(display.id);
        }
        for id in added {
            if let Some((doc, relocated)) =
                take_named_page(&self.keys[&id], &self.keys, &mut self.unmatched)
            {
                let page = if self.active == Some(id) {
                    &mut *history
                } else {
                    self.parked.entry(id).or_default()
                };
                page.set_document(doc, true);
                self.changed |= relocated;
            }
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
        if self.active.is_none() && !history.document().is_empty() {
            return Err("No display for the restored legacy page".into());
        }
        // The serialized session borrows live display keys and documents.
        // This avoids copying every display name on each autosave.
        let mut pages = self.unmatched.iter().collect::<BTreeMap<_, _>>();
        let blank = Document::default();
        for (id, key) in &self.keys {
            let doc = if self.active == Some(*id) {
                history.document()
            } else {
                self.parked.get(id).map_or(&blank, |page| page.document())
            };
            doc.validate()?;
            pages.insert(key, doc);
        }
        let session = Session {
            version: SESSION_VERSION,
            pages: pages.into_iter().collect(),
        };
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
        let written = write_session(&mut file, &session).and_then(|()| file.sync_all());
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
    ) -> Result<usize, String> {
        let file = read_session(path)?;
        match file.version {
            1 => {
                let doc = Document {
                    version: file.version,
                    size: file.size.ok_or("Session has no canvas dimensions")?,
                    strokes: file.strokes.ok_or("Session has no strokes")?,
                    text_boxes: file.text_boxes,
                };
                doc.validate()?;
                // A legacy single-page document replaces the session, not
                // only the active display. Otherwise parked and unmatched
                // pages silently reappear after a display change/save.
                self.parked.clear();
                self.unmatched.clear();
                history.set_document(doc, restore);
                self.changed = true;
                Ok(0)
            }
            SESSION_VERSION => {
                let mut docs = BTreeMap::new();
                for (key, doc) in file.pages.ok_or("Session has no display pages")? {
                    doc.validate()?;
                    if docs.insert(key, doc).is_some() {
                        return Err("Duplicate display in session".into());
                    }
                }
                let saved_count = docs.len();
                let mut relocated = false;
                let mut matched = BTreeMap::new();
                for (id, key) in &self.keys {
                    if let Some((doc, moved)) = take_named_page(key, &self.keys, &mut docs) {
                        matched.insert(*id, doc);
                        relocated |= moved;
                    }
                }
                // An unmatched blank page cannot supply ink to another screen.
                docs.retain(|_, doc| !doc.is_empty());
                // Different backends can report different names for the same
                // layout. Match unique origins only with the same screen count.
                if saved_count == self.keys.len() {
                    for (id, key) in &self.keys {
                        if matched.contains_key(id)
                            || self
                                .keys
                                .values()
                                .filter(|other| other.origin == key.origin)
                                .count()
                                != 1
                        {
                            continue;
                        }
                        if let Some(doc) =
                            take_unique_page(&mut docs, |saved, _| saved.origin == key.origin)
                        {
                            matched.insert(*id, doc);
                            relocated = true;
                        }
                    }
                }
                let single_screen = saved_count == 1 && self.keys.len() == 1;
                if (!restore || single_screen)
                    && let Some(id) = self.active.filter(|id| self.keys.contains_key(id))
                    && !matched.contains_key(&id)
                    && let Some(doc) = take_unique_page(&mut docs, |_, _| true)
                {
                    matched.insert(id, doc);
                    relocated = true;
                }
                let pending = docs.len();
                for id in self.keys.keys() {
                    let doc = matched.remove(id);
                    if doc.is_none() && pending > 0 {
                        // Keep the current page while the saved page is unassigned.
                        continue;
                    }
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
                Ok(pending)
            }
            _ => Err("Unsupported session version".into()),
        }
    }
}

struct SessionWriter<W> {
    writer: W,
    remaining: u64,
}

impl<W: Write> Write for SessionWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.len() as u64 > self.remaining {
            return Err(io::Error::other("Session exceeds 64 MiB"));
        }
        let written = self.writer.write(data)?;
        self.remaining -= written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

fn write_session(file: &mut File, session: &impl Serialize) -> io::Result<()> {
    let mut writer = BufWriter::new(SessionWriter {
        writer: file,
        remaining: MAX_FILE_BYTES,
    });
    serde_json::to_writer(&mut writer, session)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        color::InkColor,
        model::{Point, Tool},
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn x11_does_not_remove_live_pages_while_sdl_reports_stale_bounds() {
        let displays = || vec![screen(10, "Left", 0), screen(20, "Right", 800)];
        let matching = [displays()[0].bounds, displays()[1].bounds];
        let moved = [matching[0], [960, 0, matching[1][2], matching[1][3]]];
        assert_eq!(filter_x11_displays(displays(), &moved).len(), 2);
        assert_eq!(filter_x11_displays(displays(), &matching[..1]).len(), 1);
        assert!(filter_x11_displays(displays(), &[]).is_empty());
    }

    #[test]
    fn unique_page_extraction_preserves_ambiguous_matches() {
        let mut docs = BTreeMap::new();
        for (index, name) in ["Left", "Right", "Right"].into_iter().enumerate() {
            docs.insert(
                DisplayKey {
                    name: name.into(),
                    origin: [index as i32 * 800, 0],
                    slot: 0,
                },
                Document::default(),
            );
        }
        assert!(take_unique_page(&mut docs, |key, _| key.name == "Right").is_none());
        assert_eq!(docs.len(), 3);
        assert!(take_unique_page(&mut docs, |key, _| key.name == "Left").is_some());
        assert_eq!(docs.len(), 2);
        assert!(take_unique_page(&mut docs, |key, _| key.name == "Left").is_none());
    }

    #[test]
    fn opening_legacy_page_discards_parked_and_unmatched_pages() -> TestResult {
        let path = std::env::temp_dir().join(format!(
            "overink-legacy-replace-{}.json",
            std::process::id()
        ));
        let mut pages = Pages::new(Some(10));
        let mut history = History::default();
        pages.sync(
            &mut history,
            &[screen(10, "Left", 0), screen(20, "Right", 800)],
        );
        draw(&mut history, 10.);
        pages.switch(&mut history, 20);
        draw(&mut history, 20.);
        assert!(!pages.parked.is_empty());
        pages.unmatched.insert(
            DisplayKey {
                name: "Detached".into(),
                origin: [1600, 0],
                slot: 0,
            },
            Document::default(),
        );
        let mut legacy = History::default();
        draw(&mut legacy, 99.);
        fs::write(&path, serde_json::to_vec(legacy.document())?)?;

        assert_eq!(pages.load(&mut history, &path, false)?, 0);
        assert!(pages.parked.is_empty());
        assert!(pages.unmatched.is_empty());
        assert_eq!(history.strokes()[0].points[0].x, 99.);
        fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn oversized_or_incomplete_session_keeps_the_current_page() -> TestResult {
        let file = tempfile::NamedTempFile::new()?;
        let mut pages = Pages::new(Some(10));
        let mut history = History::default();
        pages.sync(&mut history, &[screen(10, "Left", 0)]);
        draw(&mut history, 25.);

        file.as_file().set_len(MAX_FILE_BYTES + 1)?;
        assert_eq!(
            pages.load(&mut history, file.path(), false).unwrap_err(),
            "Session exceeds 64 MiB"
        );
        fs::write(file.path(), br#"{"version":1,"size":[800,600]}"#)?;
        assert!(pages.load(&mut history, file.path(), false).is_err());
        assert_eq!(history.strokes()[0].points[0].x, 25.);
        Ok(())
    }

    #[test]
    fn session_writer_enforces_its_limit_when_flushed() -> TestResult {
        let mut bytes = Vec::new();
        let mut writer = BufWriter::new(SessionWriter {
            writer: &mut bytes,
            remaining: 6,
        });
        serde_json::to_writer(&mut writer, "ink")?;
        writer.flush()?;
        writer.write_all(b"  ")?;
        assert_eq!(
            writer.flush().unwrap_err().to_string(),
            "Session exceeds 64 MiB"
        );
        drop(writer);
        assert_eq!(bytes, b"\"ink\"");
        Ok(())
    }

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
    fn saved_session_restores_parked_pages_and_forgets_unplugged_displays() -> TestResult {
        let path = std::env::temp_dir().join(format!("overink-pages-{}.json", std::process::id()));
        let mut pages = Pages::new(Some(10));
        let mut history = History::default();
        pages.sync(
            &mut history,
            &[screen(10, "Left", 0), screen(20, "Right", 800)],
        );
        draw(&mut history, 11.);
        pages.switch(&mut history, 20);
        assert!(history.strokes().is_empty());
        assert!(pages.needs_save(&history));
        pages.save(&history, &path)?;
        pages.mark_saved(&mut history);
        assert!(!pages.needs_save(&history));
        draw(&mut history, 22.);
        pages.save(&history, &path)?;

        // SDL display IDs and list order can differ after a restart.
        let right = screen(31, "Right", 800);
        let left = screen(32, "Left", 0);
        let mut restored = Pages::new(Some(right.id));
        let mut history = History::default();
        restored.sync(&mut history, &[right.clone(), left.clone()]);
        restored.load(&mut history, &path, true)?;
        assert_eq!(history.strokes()[0].points[0].x, 22.);
        restored.switch(&mut history, left.id);
        assert_eq!(history.strokes()[0].points[0].x, 11.);
        history.undo();
        assert_eq!(history.strokes().len(), 1);
        assert!(!restored.needs_save(&history));
        restored.sync(&mut history, std::slice::from_ref(&left));
        assert!(restored.needs_save(&history));
        restored.save(&history, &path)?;
        restored.mark_saved(&mut history);
        restored.sync(&mut history, &[left, right.clone()]);
        restored.switch(&mut history, right.id);
        assert!(history.strokes().is_empty());
        restored.load(&mut history, &path, true)?;
        assert!(history.strokes().is_empty());
        fs::remove_file(&path)?;
        Ok(())
    }
    #[test]
    fn legacy_session_startup_has_no_undo_or_dirty_document() -> TestResult {
        let path = std::env::temp_dir().join(format!("overink-legacy-{}.json", std::process::id()));
        let mut legacy = History::default();
        draw(&mut legacy, 15.);
        fs::write(&path, serde_json::to_vec(legacy.document())?)?;
        let mut restored = Pages::new(Some(20));
        let mut history = History::default();
        restored.sync(&mut history, &[screen(20, "Right", 800)]);
        restored.load(&mut history, &path, true)?;
        assert!(!history.changed);
        history.undo();
        assert_eq!(history.strokes()[0].points[0].x, 15.);
        assert!(restored.needs_save(&history));
        restored.save(&history, &path)?;
        let file: Session<DisplayKey, Document> = serde_json::from_slice(&fs::read(&path)?)?;
        assert_eq!(file.version, SESSION_VERSION);
        assert_eq!(file.pages[0].0.name, "Right");
        fs::remove_file(&path)?;
        Ok(())
    }
    #[test]
    fn single_screen_restore_accepts_a_new_display_name_and_origin() -> TestResult {
        let path =
            std::env::temp_dir().join(format!("overink-renamed-{}.json", std::process::id()));
        let mut source = Pages::new(Some(10));
        let mut history = History::default();
        source.sync(&mut history, &[screen(10, "eDP-1", 0)]);
        draw(&mut history, 25.);
        source.save(&history, &path)?;

        let mut restored = Pages::new(Some(20));
        let mut history = History::default();
        restored.sync(&mut history, &[screen(20, "BOE Panel", 300)]);
        assert_eq!(restored.load(&mut history, &path, true)?, 0);
        history.undo();
        assert_eq!(history.strokes()[0].points[0].x, 25.);
        assert!(restored.needs_save(&history));
        fs::remove_file(path)?;
        Ok(())
    }
    #[test]
    fn manual_open_imports_the_only_ink_page_onto_the_active_screen() -> TestResult {
        let path = std::env::temp_dir().join(format!("overink-import-{}.json", std::process::id()));
        let mut source = Pages::new(Some(20));
        let mut history = History::default();
        source.sync(
            &mut history,
            &[screen(10, "Blank", 0), screen(20, "Saved", 800)],
        );
        draw(&mut history, 22.);
        source.save(&history, &path)?;

        let mut restored = Pages::new(Some(30));
        let mut history = History::default();
        restored.sync(&mut history, &[screen(30, "Laptop", 0)]);
        draw(&mut history, 33.);
        assert_eq!(restored.load(&mut history, &path, false)?, 0);
        assert_eq!(history.strokes()[0].points[0].x, 22.);
        history.undo();
        assert_eq!(history.strokes()[0].points[0].x, 33.);
        fs::remove_file(path)?;
        Ok(())
    }
    #[test]
    fn renamed_layout_uses_origins_and_unassigned_pages_can_reconnect_by_name() -> TestResult {
        let path =
            std::env::temp_dir().join(format!("overink-pending-{}.json", std::process::id()));
        let mut source = Pages::new(Some(10));
        let mut history = History::default();
        source.sync(
            &mut history,
            &[screen(10, "Left", 0), screen(20, "Right", 800)],
        );
        draw(&mut history, 11.);
        source.switch(&mut history, 20);
        draw(&mut history, 22.);
        source.save(&history, &path)?;

        let mut restored = Pages::new(Some(40));
        let mut history = History::default();
        restored.sync(
            &mut history,
            &[screen(30, "Display A", 0), screen(40, "Display B", 800)],
        );
        assert_eq!(restored.load(&mut history, &path, true)?, 0);
        assert_eq!(history.strokes()[0].points[0].x, 22.);
        restored.switch(&mut history, 30);
        assert_eq!(history.strokes()[0].points[0].x, 11.);

        let mut imported = Pages::new(Some(50));
        let mut history = History::default();
        let laptop = screen(50, "Laptop", 0);
        imported.sync(&mut history, std::slice::from_ref(&laptop));
        draw(&mut history, 55.);
        assert_eq!(imported.load(&mut history, &path, false)?, 2);
        assert_eq!(history.strokes()[0].points[0].x, 55.);
        imported.sync(&mut history, &[laptop, screen(60, "Right", 1200)]);
        assert_eq!(imported.unmatched.len(), 1);
        imported.switch(&mut history, 60);
        assert_eq!(history.strokes()[0].points[0].x, 22.);
        fs::remove_file(path)?;
        Ok(())
    }
    #[test]
    fn switching_screens_keeps_separate_ink_and_undo() {
        let mut pages = Pages::new(Some(10));
        let mut history = History::default();
        draw(&mut history, 11.0);
        assert!(pages.switch(&mut history, 20));
        assert!(history.strokes().is_empty());
        draw(&mut history, 22.0);
        pages.switch(&mut history, 10);
        assert_eq!(history.strokes()[0].points[0].x, 11.0);
        history.undo();
        assert!(history.strokes().is_empty());
        pages.switch(&mut history, 20);
        assert_eq!(history.strokes()[0].points[0].x, 22.0);
    }
    #[test]
    fn disconnect_drops_active_and_parked_ink_including_undo_redo() {
        let mut pages = Pages::new(Some(10));
        let mut history = History::default();
        draw(&mut history, 11.0);
        pages.switch(&mut history, 20);
        draw(&mut history, 22.0);
        assert!(pages.retain(&mut history, &BTreeSet::new()));
        assert!(history.strokes().is_empty());
        history.undo();
        history.redo();
        assert!(history.strokes().is_empty());
        pages.switch(&mut history, 10);
        history.undo();
        history.redo();
        assert!(history.strokes().is_empty());
    }
    #[test]
    fn removing_an_inactive_screen_preserves_the_active_page() {
        let mut pages = Pages::new(Some(10));
        let mut history = History::default();
        draw(&mut history, 11.0);
        pages.switch(&mut history, 20);
        draw(&mut history, 22.0);
        assert!(!pages.retain(&mut history, &BTreeSet::from([20])));
        assert_eq!(history.strokes()[0].points[0].x, 22.0);
        pages.switch(&mut history, 10);
        assert!(history.strokes().is_empty());
    }
}
