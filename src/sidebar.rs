// Left sidebar: "My stations" (sections), "Browse" (radio-browser.info) and "Songs" (history).
use super::*;

const API: &str = "https://all.api.radio-browser.info/json/";
const CATEGORIES: [&str; 4] = ["Popular", "Top voted", "Genres", "Countries"];
const SONG_VIEWS: [&str; 2] = ["History", "Liked"];

#[derive(Clone, Copy, PartialEq)]
pub enum Tab {
    Mine,
    Browse,
    Songs,
}

pub enum Row {
    // section name, number of stations
    Header(String, usize),
    // index in App::stations
    Item(usize),
}

pub enum BrowseData {
    Stations(Vec<(Station, String)>),
    // label, API value, number of stations
    Groups(Vec<(String, String, u64)>),
}

pub struct Browse {
    query: String,
    cat: usize,
    // the open genre or country: label, API value
    pick: Option<(String, String)>,
    groups: Vec<(String, String, u64)>,
    items: Vec<(Station, String)>,
    list: ListState,
    loading: bool,
    // id of the last request, older answers are dropped
    request_id: u64,
    // search once the user stops typing
    due: Option<Instant>,
}

pub struct Sidebar {
    pub tab: Tab,
    // the search bar has the keyboard
    pub typing: bool,
    filter: String,
    pub mine: ListState,
    pub browse: Browse,
    songs: ListState,
    liked_only: bool,
    song_filter: String,
}

impl Sidebar {
    pub fn new() -> Self {
        Sidebar {
            tab: Tab::Mine,
            typing: false,
            filter: String::new(),
            mine: ListState::default(),
            browse: Browse {
                query: String::new(),
                cat: 0,
                pick: None,
                groups: vec![],
                items: vec![],
                list: ListState::default(),
                loading: false,
                request_id: 0,
                due: None,
            },
            songs: ListState::default(),
            liked_only: false,
            song_filter: String::new(),
        }
    }

    // waiting for radio-browser, or for a pause in typing
    pub fn busy(&self) -> bool {
        self.browse.loading || self.browse.due.is_some()
    }

    pub fn search_due(&self) -> bool {
        self.browse.due.is_some_and(|d| Instant::now() >= d)
    }

    fn showing_groups(&self) -> bool {
        let b = &self.browse;
        b.query.trim().is_empty() && b.pick.is_none() && b.cat >= 2
    }

    // Ask radio-browser for what Browse shows now.
    pub fn reload(&mut self, tx: &Sender<Msg>) {
        let b = &mut self.browse;
        b.request_id += 1;
        b.loading = true;
        b.due = None;
        let request_id = b.request_id;
        let popular = [("order", "clickcount"), ("reverse", "true")];
        let biggest = [("order", "stationcount"), ("reverse", "true")];
        let (path, mut params): (&str, Vec<(&str, String)>) = match (&b.pick, b.query.trim(), b.cat)
        {
            (_, q, _) if !q.is_empty() => ("stations/search", vec![("name", q.into())]),
            (Some((_, tag)), _, 2) => (
                "stations/search",
                vec![("tag", tag.clone()), ("tagExact", "true".into())],
            ),
            (Some((_, code)), _, _) => ("stations/search", vec![("countrycode", code.clone())]),
            (None, _, 0) => ("stations/topclick", vec![]),
            (None, _, 1) => ("stations/topvote", vec![]),
            (None, _, 2) => ("tags", vec![]),
            _ => ("countries", vec![]),
        };
        let order = if path == "tags" || path == "countries" {
            &biggest
        } else {
            &popular
        };
        if path == "stations/search" || path == "tags" || path == "countries" {
            params.extend(order.iter().map(|(k, v)| (*k, v.to_string())));
        }
        params.push(("hidebroken", "true".into()));
        params.push(("limit", if path == "tags" { "300" } else { "100" }.into()));
        let tx = tx.clone();
        thread::spawn(move || {
            let res = api(path, &params).map(|list| match path {
                "tags" => BrowseData::Groups(groups(&list, "name")),
                "countries" => BrowseData::Groups(groups(&list, "iso_3166_1")),
                _ => BrowseData::Stations(list.iter().filter_map(found_station).collect()),
            });
            let _ = tx.send(Msg::Browse(request_id, res));
        });
    }
}

fn api(path: &str, params: &[(&str, String)]) -> Result<Vec<Value>, String> {
    let mut req = ureq::get(format!("{API}{path}")).header("User-Agent", USER_AGENT);
    for (k, v) in params {
        req = req.query(k, v);
    }
    req.call()
        .and_then(|mut r| r.body_mut().read_json())
        .map_err(|e| e.to_string())
}

fn txt(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").trim().to_string()
}

// genres and countries with at least a few stations
fn groups(list: &[Value], value_key: &str) -> Vec<(String, String, u64)> {
    list.iter()
        .map(|g| {
            (
                txt(g, "name"),
                txt(g, value_key),
                g["stationcount"].as_u64().unwrap_or(0),
            )
        })
        .filter(|(name, value, n)| !name.is_empty() && !value.is_empty() && *n >= 5)
        .collect()
}

fn found_station(s: &Value) -> Option<(Station, String)> {
    let st = Station {
        name: txt(s, "name"),
        url: txt(s, "url_resolved"),
        image: txt(s, "favicon"),
        section: String::new(),
    };
    if st.url.is_empty() {
        return None;
    }
    let bitrate = s["bitrate"]
        .as_u64()
        .filter(|b| *b > 0)
        .map(|b| format!(" {b}k"))
        .unwrap_or_default();
    let info = format!("{} · {}{bitrate}", txt(s, "countrycode"), txt(s, "codec"));
    Some((st, info))
}

pub fn section_of(s: &Station) -> &str {
    match s.section.trim() {
        "" => "Other",
        sec => sec,
    }
}

// cut to `w` chars, with "…" when too long
fn fit(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        s.to_string()
    } else {
        s.chars().take(w.saturating_sub(1)).chain(['…']).collect()
    }
}

fn station_fields(st: &Station) -> Vec<(&'static str, String)> {
    vec![
        ("Stream URL", st.url.clone()),
        ("Name", st.name.clone()),
        ("Section", st.section.clone()),
        ("Image URL or file (optional)", st.image.clone()),
    ]
}

fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(path),
    }
}

// #EXTM3U playlist with the logo and the section of each station, as most players read it
pub fn to_m3u(stations: &[Station]) -> String {
    let mut out = String::from("#EXTM3U\n");
    for st in stations {
        out += &format!(
            "#EXTINF:-1 tvg-logo=\"{}\" group-title=\"{}\",{}\n{}\n",
            st.image, st.section, st.name, st.url
        );
    }
    out
}

pub fn from_m3u(text: &str) -> Vec<Station> {
    let mut out = vec![];
    let mut info = "";
    for line in text.lines().map(str::trim) {
        if let Some(i) = line.strip_prefix("#EXTINF:") {
            info = i;
        } else if !line.is_empty() && !line.starts_with('#') {
            // the name follows the first comma outside the quotes
            let mut quoted = false;
            let comma = info.char_indices().find(|&(_, c)| {
                quoted ^= c == '"';
                c == ',' && !quoted
            });
            let name = comma.map_or("", |(i, _)| info[i + 1..].trim());
            let attr = |k: &str| {
                let start = info.find(&format!("{k}=\""))? + k.len() + 2;
                let len = info[start..].find('"')?;
                Some(info[start..start + len].to_string())
            };
            out.push(Station {
                name: if name.is_empty() { line } else { name }.to_string(),
                url: line.to_string(),
                image: attr("tvg-logo").unwrap_or_default(),
                section: attr("group-title").unwrap_or_default(),
            });
            info = "";
        }
    }
    out
}

impl App {
    // Sections in order of first appearance, stations in list order.
    pub fn mine_rows(&self) -> Vec<Row> {
        let filter = self.side.filter.to_lowercase();
        let mut sections: Vec<&str> = vec![];
        for s in &self.stations {
            if !sections.contains(&section_of(s)) {
                sections.push(section_of(s));
            }
        }
        let mut rows = vec![];
        for sec in sections {
            let items: Vec<usize> = (0..self.stations.len())
                .filter(|&i| section_of(&self.stations[i]) == sec)
                .filter(|&i| {
                    filter.is_empty() || self.stations[i].name.to_lowercase().contains(&filter)
                })
                .collect();
            if items.is_empty() {
                continue;
            }
            rows.push(Row::Header(sec.to_string(), items.len()));
            // a filter shows matches in folded sections too
            if filter.is_empty() && self.settings.folded.iter().any(|f| f == sec) {
                continue;
            }
            rows.extend(items.into_iter().map(Row::Item));
        }
        rows
    }

    pub fn select_station(&mut self, idx: usize) {
        let pos = self
            .mine_rows()
            .iter()
            .position(|r| matches!(r, Row::Item(i) if *i == idx));
        self.side.mine.select(pos.or(Some(0)));
    }

    pub fn switch_tab(&mut self) {
        self.set_tab(match self.side.tab {
            Tab::Mine => Tab::Browse,
            Tab::Browse => Tab::Songs,
            Tab::Songs => Tab::Mine,
        });
    }

    pub fn set_tab(&mut self, tab: Tab) {
        self.side.tab = tab;
        if tab == Tab::Browse && self.side.browse.request_id == 0 {
            self.side.reload(&self.tx);
        }
        if tab == Tab::Songs {
            self.side.songs.select(Some(0));
        }
    }

    pub fn on_browse_data(&mut self, request_id: u64, res: Result<BrowseData, String>) {
        let b = &mut self.side.browse;
        if request_id != b.request_id {
            return;
        }
        b.loading = false;
        match res {
            Ok(BrowseData::Stations(items)) => b.items = items,
            Ok(BrowseData::Groups(groups)) => b.groups = groups,
            Err(e) => self.status = format!("radio-browser.info: {e}"),
        }
        b.list.select(Some(0));
    }

    // Keys while the search bar has the keyboard.
    pub fn on_typing_key(&mut self, key: KeyCode) {
        let tab = self.side.tab;
        let text = match tab {
            Tab::Mine => &mut self.side.filter,
            Tab::Browse => &mut self.side.browse.query,
            Tab::Songs => &mut self.side.song_filter,
        };
        match key {
            KeyCode::Char(c) => text.push(c),
            KeyCode::Backspace => _ = text.pop(),
            KeyCode::Esc => {
                text.clear();
                self.side.typing = false;
            }
            KeyCode::Enter | KeyCode::Down | KeyCode::Tab => self.side.typing = false,
            _ => return,
        }
        match tab {
            Tab::Browse => {
                // wait for a pause in typing, Enter searches at once
                let wait = if self.side.typing { 400 } else { 0 };
                self.side.browse.due = Some(Instant::now() + Duration::from_millis(wait));
            }
            Tab::Mine => self.side.mine.select(Some(0)),
            Tab::Songs => self.side.songs.select(Some(0)),
        }
    }

    pub fn clear_search(&mut self) {
        match self.side.tab {
            Tab::Mine => self.side.filter.clear(),
            Tab::Browse => {
                self.side.browse.query.clear();
                self.side.browse.due = Some(Instant::now() + Duration::from_millis(400));
            }
            Tab::Songs => self.side.song_filter.clear(),
        }
    }

    pub fn on_side_key(&mut self, key: KeyCode) {
        match self.side.tab {
            Tab::Mine => self.on_mine_key(key),
            Tab::Browse => self.on_browse_key(key),
            Tab::Songs => self.on_songs_key(key),
        }
    }

    fn on_mine_key(&mut self, key: KeyCode) {
        let rows = self.mine_rows();
        let row = self.side.mine.selected().and_then(|i| rows.get(i));
        let item = match row {
            Some(Row::Item(i)) => Some(*i),
            _ => None,
        };
        let section = match row {
            Some(Row::Header(sec, _)) => sec.clone(),
            Some(Row::Item(i)) => self.stations[*i].section.clone(),
            None => String::new(),
        };
        match key {
            KeyCode::Down | KeyCode::Char('j') => self.side.mine.select_next(),
            KeyCode::Left | KeyCode::Right => {
                // the header of the selected row
                let sel = self
                    .side
                    .mine
                    .selected()
                    .unwrap_or(0)
                    .min(rows.len().saturating_sub(1));
                let Some((h, Row::Header(sec, _))) = rows
                    .iter()
                    .enumerate()
                    .take(sel + 1)
                    .rev()
                    .find(|(_, r)| matches!(r, Row::Header(..)))
                else {
                    return;
                };
                let folded = self.settings.folded.iter().any(|f| f == sec);
                if (key == KeyCode::Left) != folded {
                    self.side.mine.select(Some(h));
                    self.on_mine_key(KeyCode::Enter);
                }
                self.side.mine.select(Some(h));
            }
            KeyCode::Up | KeyCode::Char('k') => self.side.mine.select_previous(),
            KeyCode::Esc => self.side.filter.clear(),
            KeyCode::Enter => match row {
                Some(Row::Header(sec, _)) => {
                    let folded = &mut self.settings.folded;
                    match folded.iter().position(|f| f == sec) {
                        Some(p) => _ = folded.remove(p),
                        None => folded.push(sec.clone()),
                    }
                    self.persist_settings();
                }
                Some(Row::Item(i)) => self.play(self.stations[*i].clone()),
                None => {}
            },
            KeyCode::Char('a') => {
                // prefill with the current stream, so "o" then "a" saves it
                let mut st = self
                    .now
                    .clone()
                    .filter(|n| !self.stations.iter().any(|s| s.url == n.url))
                    .unwrap_or_default();
                st.section = section;
                self.open_form(FormKind::Add, station_fields(&st));
            }
            KeyCode::Char('e') => match row {
                Some(Row::Header(sec, _)) => self.open_form(
                    FormKind::Rename(sec.clone()),
                    vec![("Rename section", sec.clone())],
                ),
                Some(Row::Item(i)) => {
                    self.open_form(FormKind::Edit(*i), station_fields(&self.stations[*i]))
                }
                None => {}
            },
            KeyCode::Char('g') => {
                if let Some(i) = item {
                    self.open_form(FormKind::Move(i), vec![("Move to section", section)]);
                }
            }
            KeyCode::Char('J') | KeyCode::Char('K') => {
                // swap with the next or previous station of the same section
                let Some(i) = item else { return };
                let same =
                    |j: &usize| section_of(&self.stations[*j]) == section_of(&self.stations[i]);
                let other = if key == KeyCode::Char('J') {
                    (i + 1..self.stations.len()).find(same)
                } else {
                    (0..i).rev().find(same)
                };
                if let Some(j) = other {
                    self.stations.swap(i, j);
                    self.save_stations();
                    self.select_station(j);
                }
            }
            KeyCode::Char('D') => {
                if let Some(i) = item {
                    let st = self.stations.remove(i);
                    self.status = format!("deleted {}", st.name);
                    self.save_stations();
                }
            }
            KeyCode::Char('i') => self.open_form(
                FormKind::Import,
                vec![("Import stations from .m3u file", "~/".into())],
            ),
            KeyCode::Char('x') => self.open_form(
                FormKind::Export,
                vec![(
                    "Export stations to .m3u file",
                    "~/bloop-stations.m3u".into(),
                )],
            ),
            _ => {}
        }
    }

    // Songs tab rows: indices in App::songs, newest first.
    fn song_rows(&self) -> Vec<usize> {
        let filter = self.side.song_filter.to_lowercase();
        (0..self.songs.len())
            .rev()
            .filter(|&i| !self.side.liked_only || self.songs[i].liked)
            .filter(|&i| {
                let s = &self.songs[i];
                filter.is_empty()
                    || s.title.to_lowercase().contains(&filter)
                    || s.station.to_lowercase().contains(&filter)
            })
            .collect()
    }

    fn on_songs_key(&mut self, key: KeyCode) {
        let rows = self.song_rows();
        let song = self
            .side
            .songs
            .selected()
            .and_then(|i| rows.get(i))
            .copied();
        match key {
            KeyCode::Down | KeyCode::Char('j') => self.side.songs.select_next(),
            KeyCode::Up | KeyCode::Char('k') => self.side.songs.select_previous(),
            KeyCode::Char(c @ '1'..='2') => {
                self.side.liked_only = c == '2';
                self.side.songs.select(Some(0));
            }
            KeyCode::Left | KeyCode::Right => {
                self.side.liked_only = !self.side.liked_only;
                self.side.songs.select(Some(0));
            }
            KeyCode::Esc => self.side.song_filter.clear(),
            KeyCode::Enter => {
                if let Some(i) = song {
                    let s = &self.songs[i];
                    // a saved station keeps its name and logo
                    let st = self
                        .stations
                        .iter()
                        .find(|st| st.url == s.url)
                        .cloned()
                        .unwrap_or_else(|| Station {
                            name: s.station.clone(),
                            url: s.url.clone(),
                            ..Default::default()
                        });
                    self.play(st);
                }
            }
            KeyCode::Char('l') => self.toggle_like(song),
            KeyCode::Char('y') => {
                if let Some(i) = song {
                    self.copy_title(Some(self.songs[i].title.clone()));
                }
            }
            KeyCode::Char('D') => {
                if let Some(i) = song {
                    self.delete_song(i);
                }
            }
            _ => {}
        }
    }

    fn import_m3u(&mut self, path: &str) {
        let path = expand_tilde(path);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                self.status = format!("cannot read {}: {e}", tilde(&path));
                return;
            }
        };
        let mut n = 0;
        for mut st in from_m3u(&text) {
            if self.stations.iter().any(|s| s.url == st.url) {
                continue;
            }
            if st.section.is_empty() {
                st.section = "Imported".into();
            }
            self.stations.push(st);
            n += 1;
        }
        self.status = format!("imported {n} new stations from {}", tilde(&path));
        self.save_stations();
    }

    fn export_m3u(&mut self, path: &str) {
        let path = expand_tilde(path);
        self.status = match std::fs::write(&path, to_m3u(&self.stations)) {
            Ok(()) => format!(
                "exported {} stations to {}",
                self.stations.len(),
                tilde(&path)
            ),
            Err(e) => format!("cannot write {}: {e}", tilde(&path)),
        };
    }

    fn on_browse_key(&mut self, key: KeyCode) {
        let groups = self.side.showing_groups();
        let b = &mut self.side.browse;
        let sel = b.list.selected().unwrap_or(0);
        match key {
            KeyCode::Down | KeyCode::Char('j') => b.list.select_next(),
            KeyCode::Left | KeyCode::Right if b.pick.is_none() && b.query.is_empty() => {
                b.cat = (b.cat + if key == KeyCode::Left { 3 } else { 1 }) % 4;
                self.side.reload(&self.tx);
            }
            KeyCode::Up | KeyCode::Char('k') => b.list.select_previous(),
            KeyCode::Char(c @ '1'..='4') => {
                b.cat = c as usize - '1' as usize;
                b.pick = None;
                b.query.clear();
                self.side.reload(&self.tx);
            }
            KeyCode::Enter if groups => {
                if let Some((label, value, _)) = b.groups.get(sel) {
                    b.pick = Some((label.clone(), value.clone()));
                    self.side.reload(&self.tx);
                }
            }
            KeyCode::Enter => {
                if let Some(st) = b.items.get(sel).map(|(st, _)| st.clone()) {
                    self.play(st);
                }
            }
            KeyCode::Char('a') if !groups => {
                if let Some((st, _)) = b.items.get(sel) {
                    let st = st.clone();
                    let sec = match self.settings.last_section.as_str() {
                        "" => "Saved".to_string(),
                        s => s.to_string(),
                    };
                    self.open_form(FormKind::SaveTo(st), vec![("Save to section", sec)]);
                }
            }
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Left
                if b.pick.is_some() || !b.query.is_empty() =>
            {
                b.pick = None;
                b.query.clear();
                self.side.reload(&self.tx);
            }
            _ => {}
        }
    }

    pub fn open_form(&mut self, kind: FormKind, fields: Vec<(&'static str, String)>) {
        self.form = Some(Form {
            kind,
            fields,
            idx: 0,
        });
    }

    pub fn submit_form(&mut self) {
        let Some(form) = self.form.take() else { return };
        let v: Vec<String> = form
            .fields
            .into_iter()
            .map(|(_, s)| s.trim().to_string())
            .collect();
        let first = v[0].clone();
        match form.kind {
            FormKind::Open if !first.is_empty() => self.play(Station {
                name: first.clone(),
                url: first,
                ..Default::default()
            }),
            FormKind::Add | FormKind::Edit(_) if !first.is_empty() => {
                let st = Station {
                    name: if v[1].is_empty() {
                        first.clone()
                    } else {
                        v[1].clone()
                    },
                    url: first,
                    section: v[2].clone(),
                    image: v[3].clone(),
                };
                match form.kind {
                    FormKind::Edit(i) => self.stations[i] = st,
                    _ => self.stations.push(st),
                }
                self.save_stations();
            }
            FormKind::Move(i) => {
                self.stations[i].section = first;
                self.save_stations();
                self.select_station(i);
            }
            FormKind::Rename(old) if !first.is_empty() => {
                for s in self.stations.iter_mut().filter(|s| section_of(s) == old) {
                    s.section = first.clone();
                }
                for f in self.settings.folded.iter_mut().filter(|f| **f == old) {
                    *f = first.clone();
                }
                self.save_stations();
                self.persist_settings();
            }
            FormKind::Import if !first.is_empty() => self.import_m3u(&first),
            FormKind::Export if !first.is_empty() => self.export_m3u(&first),
            FormKind::Token => {
                self.settings.listenbrainz_token = first;
                self.persist_settings();
                if self.settings.listenbrainz_token.is_empty() {
                    self.status = "ListenBrainz off".into();
                } else {
                    self.status = "ListenBrainz: checking the token…".into();
                    self.check_token();
                }
            }
            FormKind::SaveTo(mut st) => {
                st.section = first.clone();
                self.status = format!("saved {} in {}", st.name, section_of(&st));
                self.settings.last_section = first;
                self.stations.push(st);
                self.save_stations();
                self.persist_settings();
            }
            _ => {}
        }
    }

    // Click on a list row: select it. Double click plays or opens, a section header folds at once.
    pub fn click_row(&mut self, y: usize, double: bool) {
        self.side.typing = false;
        match self.side.tab {
            Tab::Mine => {
                let i = self.side.mine.offset() + y;
                let Some(header) = self
                    .mine_rows()
                    .get(i)
                    .map(|r| matches!(r, Row::Header(..)))
                else {
                    return;
                };
                self.side.mine.select(Some(i));
                // a double click on a header would fold and unfold again
                if header != double {
                    self.on_mine_key(KeyCode::Enter);
                }
            }
            Tab::Browse => {
                let b = &self.side.browse;
                let n = if self.side.showing_groups() {
                    b.groups.len()
                } else {
                    b.items.len()
                };
                let i = b.list.offset() + y;
                if i < n {
                    self.side.browse.list.select(Some(i));
                    if double {
                        self.on_browse_key(KeyCode::Enter);
                    }
                }
            }
            Tab::Songs => {
                let i = self.side.songs.offset() + y;
                if i < self.song_rows().len() {
                    self.side.songs.select(Some(i));
                    if double {
                        self.on_songs_key(KeyCode::Enter);
                    }
                }
            }
        }
    }

    pub fn help_keys(&self) -> Vec<(&'static str, &'static str)> {
        if self.side.typing {
            return vec![("enter", "done"), ("esc", "clear")];
        }
        let mut keys = vec![
            ("enter", "play"),
            ("space", "pause"),
            ("- +", "volume"),
            ("/", "search"),
        ];
        keys.extend(match self.side.tab {
            Tab::Mine => vec![
                ("tab", "browse"),
                ("↑↓", "move"),
                ("←→", "fold"),
                ("a", "add"),
                ("e", "edit"),
                ("g", "section"),
                ("J K", "order"),
                ("D", "delete"),
                ("i", "import"),
                ("x", "export"),
            ],
            Tab::Browse => vec![
                ("tab", "songs"),
                ("↑↓", "move"),
                ("←→", "category"),
                ("a", "save"),
                ("esc", "back"),
            ],
            Tab::Songs => vec![
                ("tab", "my stations"),
                ("↑↓", "move"),
                ("←→", "liked"),
                ("D", "delete"),
            ],
        });
        keys.extend([
            ("l", "like"),
            ("y", "copy"),
            ("t", "sleep"),
            ("s", "stop"),
            ("m", "mute"),
            ("r", "rec"),
            ("o", "url"),
            ("c", "settings"),
            ("q", "quit"),
        ]);
        keys
    }

    pub fn draw_sidebar(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(BORDER));
        let inner = block.inner(area).inner(Margin::new(1, 0));
        f.render_widget(block, area);
        let browse = self.side.tab == Tab::Browse;
        let songs = self.side.tab == Tab::Songs;
        let [tabs, search, cats, list_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(if browse || songs { 2 } else { 1 }),
            Constraint::Fill(1),
        ])
        .areas(inner);

        let tab = |name: &str, on: bool| {
            let s = Span::raw(format!(" {name} "));
            if on {
                s.fg(Color::Black).bg(ACCENT).bold()
            } else {
                s.fg(DIM)
            }
        };
        let mut tab_line = vec![
            tab("My stations", self.side.tab == Tab::Mine),
            Span::raw(" "),
            tab("Browse", browse),
            Span::raw(" "),
            tab("Songs", songs),
        ];
        if browse && self.side.browse.loading {
            tab_line.push(Span::raw(format!("  {}", SPINNER[self.tick % 10])).fg(ACCENT));
        }
        f.render_widget(Paragraph::new(Line::from(tab_line)), tabs);
        self.hits
            .push((Rect { width: 13, ..tabs }, Hit::Tab(Tab::Mine)));
        self.hits.push((
            Rect {
                x: tabs.x + 14,
                width: 8,
                ..tabs
            },
            Hit::Tab(Tab::Browse),
        ));
        self.hits.push((
            Rect {
                x: tabs.x + 23,
                width: 7,
                ..tabs
            },
            Hit::Tab(Tab::Songs),
        ));

        let (text, hint) = match self.side.tab {
            Tab::Mine => (&self.side.filter, "filter my stations"),
            Tab::Browse => (&self.side.browse.query, "search all stations"),
            Tab::Songs => (&self.side.song_filter, "filter songs"),
        };
        let mut line = vec![Span::raw("/ ").fg(PINK)];
        if text.is_empty() && !self.side.typing {
            line.push(Span::raw(hint).fg(DIM));
        } else {
            line.push(Span::raw(text.clone()));
            if self.side.typing && (self.tick / 5).is_multiple_of(2) {
                line.push(Span::raw("▏").fg(ACCENT));
            }
        }
        let border = if self.side.typing { ACCENT } else { BORDER };
        let search_box = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(border));
        f.render_widget(Paragraph::new(Line::from(line)).block(search_box), search);
        self.hits.push((search, Hit::Search));

        if songs {
            let views = SONG_VIEWS.iter().enumerate().flat_map(|(i, v)| {
                let s = Span::raw(format!("{} {v}", i + 1));
                let on = (i == 1) == self.side.liked_only;
                [
                    if on { s.fg(PINK).bold() } else { s.fg(DIM) },
                    Span::raw("  "),
                ]
            });
            f.render_widget(Paragraph::new(Line::from(views.collect::<Vec<_>>())), cats);
            let mut x = cats.x;
            for (i, v) in SONG_VIEWS.iter().enumerate() {
                let w = v.len() as u16 + 2;
                let r = Rect {
                    x,
                    width: w,
                    height: 1,
                    ..cats
                };
                self.hits.push((r, Hit::Category(i)));
                x += w + 2;
            }
        }
        if browse {
            let b = &self.side.browse;
            let line = match (&b.pick, b.query.trim().is_empty()) {
                (_, false) => Line::from(vec![
                    Span::raw("results for ").fg(DIM),
                    Span::raw(b.query.trim().to_string()).fg(PINK),
                ]),
                (Some((label, _)), _) => Line::from(vec![
                    Span::raw(format!("{} › ", CATEGORIES[b.cat])).fg(DIM),
                    Span::raw(label.clone()).fg(PINK).bold(),
                    Span::raw("   esc back").fg(DIM),
                ]),
                (None, _) => Line::from(
                    CATEGORIES
                        .iter()
                        .enumerate()
                        .flat_map(|(i, c)| {
                            let s = Span::raw(format!("{} {c}", i + 1));
                            [
                                if i == b.cat {
                                    s.fg(PINK).bold()
                                } else {
                                    s.fg(DIM)
                                },
                                Span::raw("  "),
                            ]
                        })
                        .collect::<Vec<_>>(),
                ),
            };
            f.render_widget(Paragraph::new(line), cats);
            match (&b.pick, b.query.is_empty()) {
                (None, true) => {
                    let mut x = cats.x;
                    for (i, c) in CATEGORIES.iter().enumerate() {
                        let w = c.len() as u16 + 2;
                        self.hits.push((
                            Rect {
                                x,
                                width: w,
                                height: 1,
                                ..cats
                            },
                            Hit::Category(i),
                        ));
                        x += w + 2;
                    }
                }
                _ => self
                    .hits
                    .push((Rect { height: 1, ..cats }, Hit::Key(KeyCode::Esc))),
            }
        }

        // room for the name next to the right column, minus the " ▸ " marker
        let w = list_area.width.saturating_sub(3) as usize;
        let items: Vec<Line> = match self.side.tab {
            Tab::Mine => self
                .mine_rows()
                .iter()
                .map(|r| match r {
                    Row::Header(sec, n) => {
                        let open = !self.side.filter.is_empty()
                            || !self.settings.folded.iter().any(|f| f == sec);
                        Line::from(vec![
                            Span::raw(if open { "▾ " } else { "▸ " }).fg(PINK),
                            Span::raw(sec.clone()).fg(ACCENT).bold(),
                            Span::raw(format!("  {n}")).fg(DIM),
                        ])
                    }
                    Row::Item(i) => {
                        let mut l = self.station_line(&self.stations[*i], None);
                        l.spans.insert(0, Span::raw("  "));
                        l
                    }
                })
                .collect(),
            Tab::Browse if self.side.showing_groups() => self
                .side
                .browse
                .groups
                .iter()
                .map(|(label, _, n)| {
                    let count = format!("{n}");
                    let name = fit(label, w.saturating_sub(count.len() + 1));
                    let pad = w.saturating_sub(name.chars().count() + count.len());
                    Line::from(vec![
                        Span::raw(name),
                        Span::raw(" ".repeat(pad)),
                        Span::raw(count).fg(DIM),
                    ])
                })
                .collect(),
            Tab::Songs => self
                .song_rows()
                .into_iter()
                .map(|i| {
                    let s = &self.songs[i];
                    // "MM-DD HH:MM" of "YYYY-MM-DD HH:MM"
                    let when = s.time.get(5..).unwrap_or(&s.time);
                    let info = format!("{} · {when}", fit(&s.station, 14));
                    let title = fit(&s.title, w.saturating_sub(info.chars().count() + 4));
                    let pad = w.saturating_sub(title.chars().count() + info.chars().count() + 2);
                    Line::from(vec![
                        Span::raw(if s.liked { "♥ " } else { "  " }).fg(PINK),
                        Span::raw(title),
                        Span::raw(" ".repeat(pad)),
                        Span::raw(info).fg(DIM),
                    ])
                })
                .collect(),
            Tab::Browse => self
                .side
                .browse
                .items
                .iter()
                .map(|(st, info)| {
                    let playing = self.now.as_ref().is_some_and(|n| n.url == st.url) && !self.idle;
                    let name = fit(&st.name, w.saturating_sub(info.chars().count() + 2));
                    let pad = w.saturating_sub(name.chars().count() + info.chars().count());
                    let name = if playing {
                        Span::raw(name).fg(Color::Green).bold()
                    } else {
                        Span::raw(name)
                    };
                    Line::from(vec![
                        name,
                        Span::raw(" ".repeat(pad)),
                        Span::raw(info.clone()).fg(DIM),
                    ])
                })
                .collect(),
        };
        if items.is_empty() {
            let msg = match self.side.tab {
                _ if self.side.browse.loading && browse => "loading…",
                Tab::Mine if !self.side.filter.is_empty() => "no station matches",
                Tab::Mine => "no stations yet, push a to add one",
                Tab::Browse => "no stations found",
                Tab::Songs if !self.side.song_filter.is_empty() => "no song matches",
                Tab::Songs if self.side.liked_only => "no liked songs yet, push l to like a song",
                Tab::Songs => "no songs yet, the song titles show here as they play",
            };
            f.render_widget(Paragraph::new(msg).fg(DIM), list_area);
            return;
        }
        let list = List::new(items)
            .highlight_symbol(Span::raw(" ▸ ").fg(PINK))
            .highlight_style(Style::new().bg(SEL_BG).bold());
        let state = match self.side.tab {
            Tab::Mine => &mut self.side.mine,
            Tab::Browse => &mut self.side.browse.list,
            Tab::Songs => &mut self.side.songs,
        };
        f.render_stateful_widget(list, list_area, state);
        self.hits.push((list_area, Hit::List));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m3u_keeps_name_logo_and_section() {
        let st = vec![Station {
            name: "FIP, Jazz".into(),
            url: "https://example.com/fip.mp3".into(),
            image: "https://example.com/fip.png".into(),
            section: "Radio France".into(),
        }];
        let back = from_m3u(&to_m3u(&st));
        assert_eq!(back.len(), 1);
        let b = &back[0];
        assert_eq!(
            (&b.name[..], &b.url[..], &b.image[..], &b.section[..]),
            (
                "FIP, Jazz",
                "https://example.com/fip.mp3",
                "https://example.com/fip.png",
                "Radio France"
            )
        );
    }

    #[test]
    fn m3u_reads_plain_lists() {
        let list =
            "#EXTM3U\n#EXTINF:-1 group-title=\"Rock, Pop\",Radio X\nhttp://a/x\n\nhttp://b/y\n";
        let st = from_m3u(list);
        assert_eq!(st.len(), 2);
        assert_eq!(
            (&st[0].name[..], &st[0].section[..]),
            ("Radio X", "Rock, Pop")
        );
        // no #EXTINF: the URL is the name
        assert_eq!((&st[1].name[..], &st[1].section[..]), ("http://b/y", ""));
    }
}
