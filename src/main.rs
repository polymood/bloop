use std::{
    collections::{BTreeMap, VecDeque},
    io::{self, BufRead, BufReader, Write},
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc::{self, Sender},
    thread,
    time::{Duration, Instant},
};

use image::{DynamicImage, imageops::FilterType};
use ratatui::{
    DefaultTerminal, Frame,
    backend::{Backend, ClearType},
    crossterm::event::{
        self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    layout::{Alignment, Constraint, Layout, Margin, Position, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, LineGauge, List, ListState, Paragraph},
};
use ratatui_image::{
    FontSize, Image, Resize,
    picker::{Picker, ProtocolType},
    protocol::Protocol,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

mod sidebar;
use sidebar::{BrowseData, Sidebar, Tab};

const ACCENT: Color = Color::Rgb(0, 200, 255);
const PINK: Color = Color::Rgb(255, 80, 200);
const BORDER: Color = Color::Rgb(70, 72, 92);
const DIM: Color = Color::Rgb(115, 118, 140);
const SEL_BG: Color = Color::Rgb(40, 42, 58);
const CHIP_BG: Color = Color::Rgb(58, 60, 78);

const EIGHTHS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const LOGO: [&str; 10] = [
    "",
    "",
    r" _     _",
    r"| |   | |",
    r"| |__ | | ___   ___  _ __",
    r"| '_ \| |/ _ \ / _ \| '_ \",
    r"| |_) | | (_) | (_) | |_) |",
    r"|_.__/|_|\___/ \___/| .__/",
    r"                    | |",
    r"                    |_|",
];

// After a longer pause the buffered audio is old and the server has often closed the connection.
const RESUME_LIVE_AFTER: Duration = Duration::from_secs(15);
const MAX_RETRIES: u32 = 3;

const MOTD_EVERY: Duration = Duration::from_secs(30 * 60);
// how long the sparkles last when a phrase appears
const SPARKLE_MS: u128 = 2000;

const MOTD: [&str; 45] = [
    "thank you for tuning in",
    "have a great day",
    "have a wonderful day",
    "have a lovely evening",
    "you are doing great today",
    "sending you good vibes on every frequency",
    "thanks for listening, you are the best",
    "hope your day sounds as good as this song",
    "take a deep breath and enjoy the music",
    "you deserve a little break",
    "smile, the radio is on",
    "today is a good day to dance in the kitchen",
    "be gentle with yourself today",
    "good things are on the way",
    "you make the airwaves brighter",
    "have a cozy day",
    "remember to take a break",
    "the music is better because you are here",
    "wishing you a calm and happy day",
    "you are tuned in to something good",
    "hello friend, nice to hear you",
    "go easy, enjoy the ride",
    "a little music makes everything better",
    "thank you for keeping the radio alive",
    "the night is young and the music is good",
    "tuning in to good vibes only",
    "the airwaves missed you",
    "stay tuned, stay kind",
    "every song is somebody's favorite",
    "turn it up, the neighbors love jazz",
    "bloop bloop. that is the sound of radio",
    "no ads, no algorithms, just radio",
    "somewhere, a DJ plays this one for you",
    "frequency of the day: happiness",
    "drink some water. stretch. bloop.",
    "an antenna is just a hug for radio waves",
    "life is better with a soundtrack",
    "keep calm and stream on",
    "you are listening to the tiniest radio",
    "radio waves travel at the speed of light. so does good taste",
    "talk less, listen more",
    "play it again, bloop",
    "the best playlist is the one you did not make",
    "made with love and a little bit of static",
    "close your eyes, it is a concert now",
];
const WAVES: [&str; 4] = [
    "        ●        ",
    "      ( ● )      ",
    "    ( ( ● ) )    ",
    "  ( ( ( ● ) ) )  ",
];

// Spectrum: band-pass filters inside mpv, measured with astats.
// 24 bands is about a third of an octave each. More bands cost CPU in astats.
const BANDS: usize = 24;
const F_LO: f32 = 40.0;
const F_HI: f32 = 16000.0;
const VIS_REQUEST: u64 = 1;
const PING_REQUEST: u64 = 2;
// no answer for this long: mpv hangs, start a new one
const MPV_TIMEOUT: Duration = Duration::from_secs(8);

fn band_freq(i: usize) -> f32 {
    F_LO * (F_HI / F_LO).powf(i as f32 / (BANDS - 1) as f32)
}

// Audio passes through unchanged. A mono copy is split into bands, astats measures them, pan drops them again.
fn vis_filter() -> String {
    let w = (F_HI / F_LO).log2() / (BANDS - 1) as f32;
    let ids = |p: &str| (0..BANDS).map(|i| format!("[{p}{i}]")).collect::<String>();
    let mut g = format!(
        "aformat=channel_layouts=stereo,asplit=2[a][b];[b]pan=mono|c0=0.5*c0+0.5*c1,asplit={BANDS}{};",
        ids("b")
    );
    for i in 0..BANDS {
        let f = band_freq(i);
        g += &format!("[b{i}]bandpass=f={f:.0}:width_type=o:w={w:.3}[f{i}];");
    }
    g += &format!(
        "[a]{}amerge=inputs={},astats=metadata=1:reset=1:measure_overall=none:measure_perchannel=RMS_level+Peak_level,pan=stereo|c0=c0|c1=c1",
        ids("f"),
        BANDS + 1
    );
    // %len% is mpv's escape for special characters
    // one thread: 24 tiny filters do not need a thread pool
    format!("@vis:lavfi=graph=%{}%{g}:o=[threads=1]", g.len())
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct Station {
    name: String,
    url: String,
    #[serde(default)]
    image: String,
    #[serde(default)]
    section: String,
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    spectrum: bool,
    level_meter: bool,
    // sends song titles to iTunes
    cover_art: bool,
    hires_images: bool,
    // mpv processes audio before the speakers play it
    meter_delay_ms: u64,
    volume: f64,
    last_url: String,
    last_section: String,
    folded: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            spectrum: false,
            level_meter: false,
            cover_art: true,
            hires_images: true,
            meter_delay_ms: 250,
            volume: 50.0,
            last_url: String::new(),
            last_section: String::new(),
            folded: vec![],
        }
    }
}

enum Msg {
    Mpv(Value),
    Image(String, Option<DynamicImage>),
    Cover(String, Option<DynamicImage>),
    Browse(u64, Result<BrowseData, String>),
    Term(Event),
    // mpv number n closed the socket: it crashed or was killed
    MpvGone(u64),
}

const USER_AGENT: &str = "bloop/0.1";

// mpv does all the audio work. We talk to it over JSON IPC.
struct Mpv {
    sock: UnixStream,
    // a hung mpv never notices that the socket closed, so we kill it ourselves
    child: std::process::Child,
    // tells a replaced mpv from the current one
    id: u64,
}

impl Drop for Mpv {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Mpv {
    fn start(tx: Sender<Msg>, volume: f64) -> io::Result<Self> {
        // mpv quits when this socket closes, so it never outlives us.
        let (sock, theirs) = UnixStream::pair()?;
        let child = Command::new("mpv")
            .args([
                "--idle=yes",
                "--no-video",
                "--no-terminal",
                "--volume-max=100",
                // lean: no Lua scripts, no youtube-dl, no video
                "--load-scripts=no",
                "--ytdl=no",
                "--osc=no",
                "--vid=no",
                "--audio-display=no",
                // live radio never seeks back, a small read-ahead is enough
                "--demuxer-max-bytes=1MiB",
                // mpv applies song titles from packets it keeps here, 0 loses every title
                "--demuxer-max-back-bytes=256KiB",
                "--msg-level=all=no",
                "--input-ipc-client=fd://0",
            ])
            .arg(format!("--volume={volume}"))
            .stdin(Stdio::from(OwnedFd::from(theirs)))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                io::Error::other(format!(
                    "cannot start mpv ({e}). Install it: sudo apt install mpv"
                ))
            })?;
        static STARTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = STARTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reader = BufReader::new(sock.try_clone()?);
        thread::spawn(move || {
            for line in reader.lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str(&line)
                    && tx.send(Msg::Mpv(v)).is_err()
                {
                    return;
                }
            }
            let _ = tx.send(Msg::MpvGone(id));
        });
        let mut mpv = Mpv { sock, child, id };
        for (i, p) in [
            "metadata",
            "pause",
            "volume",
            "idle-active",
            "audio-codec-name",
            "core-idle",
            "mute",
            "seekable",
        ]
        .iter()
        .enumerate()
        {
            mpv.cmd(json!(["observe_property", i + 1, p]));
        }
        Ok(mpv)
    }

    fn cmd(&mut self, c: Value) {
        let _ = writeln!(self.sock, "{}", json!({ "command": c }));
    }

    fn request(&mut self, id: u64, c: Value) {
        let _ = writeln!(self.sock, "{}", json!({ "command": c, "request_id": id }));
    }

    fn set(&mut self, prop: &str, v: Value) {
        self.cmd(json!(["set_property", prop, v]));
    }
}

enum FormKind {
    Add,
    Edit(usize),
    Open,
    Move(usize),
    Rename(String),
    SaveTo(Station),
}

// What a mouse click on a screen area does.
#[derive(Clone, PartialEq)]
enum Hit {
    // same as pushing this key
    Key(KeyCode),
    Tab(Tab),
    Search,
    // the row comes from the click position
    List,
    Category(usize),
    // the mouse wheel changes the volume here
    Player,
    Volume,
    Setting(usize),
    Popup,
}

enum Art {
    Blocks(Vec<Line<'static>>),
    Graphic(Protocol),
}

struct Form {
    kind: FormKind,
    fields: Vec<(&'static str, String)>,
    idx: usize,
}

struct App {
    mpv: Mpv,
    tx: Sender<Msg>,
    stations: Vec<Station>,
    side: Sidebar,
    form: Option<Form>,
    settings: Settings,
    settings_row: Option<usize>,
    vis_on: bool,
    now: Option<Station>,
    meta: BTreeMap<String, String>,
    logo: Option<DynamicImage>,
    cover: Option<(String, DynamicImage)>,
    art_cache: Option<(Rect, Art)>,
    picker: Option<Picker>,
    // sixel pixels can stay on screen, so clear once after an image change
    clear_screen: bool,
    paused: bool,
    paused_at: Option<Instant>,
    // live streams are not seekable, files are
    live: bool,
    retries: u32,
    reconnect_at: Option<Instant>,
    // watchdog: last ping sent, oldest ping without an answer
    ping_at: Instant,
    unanswered: Option<Instant>,
    idle: bool,
    core_idle: bool,
    muted: bool,
    volume: f64,
    codec: String,
    recording: Option<(PathBuf, Instant)>,
    status: String,
    // 10 Hz
    tick: usize,
    // current phrase and when it started to slide
    motd: (usize, Instant),
    meters: Meters,
    // click areas of the last frame, the last one is on top
    hits: Vec<(Rect, Hit)>,
    // for double clicks
    last_click: Option<(Instant, u16, u16)>,
}

// Each vec holds [L, R, bands...], 0..1.
struct Meters {
    // dB values, delayed until the speakers play them
    queue: VecDeque<(Instant, Vec<f32>)>,
    delay: Duration,
    db: Vec<f32>,
    level: Vec<f32>,
    hold: Vec<f32>,
}

impl Meters {
    fn new(delay_ms: u64) -> Self {
        Meters {
            queue: VecDeque::new(),
            delay: Duration::from_millis(delay_ms),
            db: vec![],
            level: vec![0.0; 2 + BANDS],
            hold: vec![0.0; 2 + BANDS],
        }
    }

    // bars on screen that still move
    fn moving(&self) -> bool {
        !self.db.is_empty() || self.level.iter().chain(&self.hold).any(|v| *v > 0.001)
    }

    fn push(&mut self, data: &Value) {
        let get = |ch: usize, k: &str| {
            data[format!("lavfi.astats.{ch}.{k}")]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(f32::NEG_INFINITY)
        };
        let mut db = vec![
            get(1, "RMS_level"),
            get(2, "RMS_level"),
            get(1, "Peak_level"),
            get(2, "Peak_level"),
        ];
        db.extend((0..BANDS).map(|i| get(i + 3, "RMS_level")));
        self.queue.push_back((Instant::now(), db));
    }

    fn step(&mut self, dt: f32, live: bool) {
        while self
            .queue
            .front()
            .is_some_and(|(t, _)| t.elapsed() >= self.delay)
        {
            self.db = self.queue.pop_front().unwrap().1;
        }
        if !live {
            self.queue.clear();
            self.db.clear();
        }
        let norm = |db: f32, lo: f32, hi: f32| ((db - lo) / (hi - lo)).clamp(0.0, 1.0);
        for i in 0..2 + BANDS {
            let (target, peak) = match (self.db.is_empty(), i) {
                (true, _) => (0.0, 0.0),
                (_, 0 | 1) => (
                    norm(self.db[i], -60.0, 0.0),
                    norm(self.db[i + 2], -60.0, 0.0),
                ),
                // +3 dB/octave, so music does not look bass-heavy
                _ => {
                    let b = i - 2;
                    let v = norm(
                        self.db[4 + b] + 3.0 * (band_freq(b) / 1000.0).log2(),
                        -55.0,
                        -10.0,
                    );
                    (v, v)
                }
            };
            self.level[i] = target.max(self.level[i] - 1.5 * dt);
            self.hold[i] = peak.max(self.level[i]).max(self.hold[i] - 0.35 * dt);
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

// "/home/me/x" -> "~/x"
fn tilde(p: &std::path::Path) -> String {
    match p.strip_prefix(home()) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

fn config_file(name: &str) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("bloop")
        .join(name)
}

fn load_json<T: DeserializeOwned>(name: &str) -> Option<T> {
    serde_json::from_str(&std::fs::read_to_string(config_file(name)).ok()?).ok()
}

fn save_json<T: Serialize>(name: &str, v: &T) -> Result<(), String> {
    let path = config_file(name);
    std::fs::create_dir_all(path.parent().unwrap())
        .and_then(|_| std::fs::write(&path, serde_json::to_string_pretty(v).unwrap()))
        .map_err(|e| format!("cannot save {}: {e}", tilde(&path)))
}

fn default_stations() -> Vec<Station> {
    serde_json::from_str(include_str!("../stations.default.json"))
        .expect("stations.default.json is valid")
}

fn fetch_image(src: &str) -> Option<DynamicImage> {
    let bytes = if src.starts_with("http://") || src.starts_with("https://") {
        ureq::get(src)
            .header("User-Agent", USER_AGENT)
            .call()
            .ok()?
            .body_mut()
            .read_to_vec()
            .ok()?
    } else {
        std::fs::read(src).ok()?
    };
    let img = image::load_from_memory(&bytes).ok()?;
    // the art area is about 300 px wide, a 1024 px logo would only waste RAM
    Some(if img.width() > 400 || img.height() > 400 {
        img.thumbnail(400, 400)
    } else {
        img
    })
}

fn load_image(src: String, tx: Sender<Msg>) {
    thread::spawn(move || {
        let img = fetch_image(&src);
        let _ = tx.send(Msg::Image(src, img));
    });
}

// Album cover from iTunes, for titles like "Artist - Song".
fn load_cover(title: String, tx: Sender<Msg>) {
    thread::spawn(move || {
        let lookup = || -> Option<DynamicImage> {
            // "(live)", "[edit]" and so on confuse the search
            let mut term = String::new();
            let mut depth = 0;
            for c in title.chars() {
                match c {
                    '(' | '[' => depth += 1,
                    ')' | ']' => depth -= 1,
                    _ if depth == 0 => term.push(c),
                    _ => {}
                }
            }
            let term = term.replace(" - ", " ");
            let res: Value = ureq::get("https://itunes.apple.com/search")
                .query("term", term.trim())
                .query("media", "music")
                .query("entity", "song")
                .query("limit", "1")
                .call()
                .ok()?
                .body_mut()
                .read_json()
                .ok()?;
            let hit = &res["results"][0];
            // wrong artist = wrong cover
            let artist = hit["artistName"].as_str()?.to_lowercase();
            if !title.to_lowercase().contains(&artist) {
                return None;
            }
            fetch_image(
                &hit["artworkUrl100"]
                    .as_str()?
                    .replace("100x100bb", "600x600bb"),
            )
        };
        let _ = tx.send(Msg::Cover(title.clone(), lookup()));
    });
}

// Two pixels per cell: top pixel = foreground of '▀', bottom pixel = background.
fn halfblock_art(img: &DynamicImage, area: Rect) -> Vec<Line<'static>> {
    let aspect = img.width() as f32 / img.height() as f32;
    let (mut cols, mut px_rows) = (
        (area.height as f32 * 2.0 * aspect) as u32,
        area.height as u32 * 2,
    );
    if cols > area.width as u32 {
        cols = area.width as u32;
        px_rows = ((cols as f32 / aspect) as u32 + 1) & !1;
    }
    let small = img
        .resize_exact(cols.max(1), px_rows.max(2), FilterType::Triangle)
        .to_rgba8();
    let px = |x, y| {
        let [r, g, b, a] = small.get_pixel(x, y).0;
        let m = |c: u8| (c as u16 * a as u16 / 255) as u8;
        Color::Rgb(m(r), m(g), m(b))
    };
    (0..small.height() / 2)
        .map(|y| {
            Line::from(
                (0..small.width())
                    .map(|x| Span::styled("▀", Style::new().fg(px(x, 2 * y)).bg(px(x, 2 * y + 1))))
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

// cheap pseudo random number
fn hash(k: usize) -> usize {
    k.wrapping_mul(2654435761) >> 7
}

fn marquee(s: &str, w: usize, tick: usize) -> String {
    let chars: Vec<char> = s.chars().chain("   ·   ".chars()).collect();
    if s.chars().count() <= w {
        return s.to_string();
    }
    (0..w)
        .map(|i| chars[(tick / 2 + i) % chars.len()])
        .collect()
}

fn lerp(a: Color, b: Color, t: f32) -> Color {
    let (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) = (a, b) else {
        return a;
    };
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t.clamp(0.0, 1.0)) as u8;
    Color::Rgb(m(r1, r2), m(g1, g2), m(b1, b2))
}

fn heat(t: f32) -> Color {
    let (g, y, r) = (
        Color::Rgb(0, 215, 95),
        Color::Rgb(255, 210, 0),
        Color::Rgb(255, 60, 60),
    );
    if t < 0.6 {
        lerp(g, y, t / 0.6)
    } else {
        lerp(y, r, (t - 0.6) / 0.4)
    }
}

fn eighth(v: f32) -> &'static str {
    EIGHTHS[(v.clamp(0.0, 1.0) * 8.0).round() as usize]
}

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn panel(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .title(Span::raw(format!(" {title} ")).fg(ACCENT).bold())
}

// "enter" -> Enter, "a" -> 'a'. Chips like "J K" have no single key.
fn chip_key(label: &str) -> Option<KeyCode> {
    match label {
        "enter" => Some(KeyCode::Enter),
        "esc" => Some(KeyCode::Esc),
        "tab" => Some(KeyCode::Tab),
        "space" => Some(KeyCode::Char(' ')),
        _ if label.chars().count() == 1 => label.chars().next().map(KeyCode::Char),
        _ => None,
    }
}

impl App {
    // Key chips that wrap to the next line and react to clicks.
    fn chip_bar(
        &mut self,
        f: &mut Frame,
        area: Rect,
        first: Vec<Span<'static>>,
        keys: &[(&str, &str)],
    ) {
        let mut lines = vec![Line::from(first)];
        let mut x = lines[0].width() as u16;
        for (k, d) in keys {
            let chip = Span::raw(format!(" {k} "))
                .fg(Color::White)
                .bg(CHIP_BG)
                .bold();
            let desc = Span::raw(format!(" {d}  ")).fg(DIM);
            let w = (chip.width() + desc.width()) as u16;
            if x + w > area.width && x > 0 {
                lines.push(Line::default());
                x = 0;
            }
            let y = area.y + lines.len() as u16 - 1;
            if let Some(code) = chip_key(k) {
                let r = Rect::new(area.x + x, y, w, 1).intersection(area);
                self.hits.push((r, Hit::Key(code)));
            }
            x += w;
            lines.last_mut().unwrap().spans.extend([chip, desc]);
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    // false = quit
    fn on_mouse(&mut self, m: MouseEvent) -> bool {
        let pos = Position::new(m.column, m.row);
        let under: Vec<(Rect, Hit)> = self
            .hits
            .iter()
            .filter(|(r, _)| r.contains(pos))
            .cloned()
            .collect();
        let popup = self.settings_row.is_some() || self.form.is_some();
        if let MouseEventKind::ScrollUp | MouseEventKind::ScrollDown = m.kind {
            let up = m.kind == MouseEventKind::ScrollUp;
            let on = |h: Hit| !popup && under.iter().any(|(_, x)| *x == h);
            if on(Hit::List) {
                self.side.typing = false;
                self.on_side_key(if up { KeyCode::Up } else { KeyCode::Down });
            } else if on(Hit::Player) {
                self.mpv
                    .cmd(json!(["add", "volume", if up { 5 } else { -5 }]));
            }
            return true;
        }
        if m.kind != MouseEventKind::Down(MouseButton::Left) {
            return true;
        }
        let double = self.last_click.is_some_and(|(t, x, y)| {
            t.elapsed() < Duration::from_millis(400) && (x, y) == (m.column, m.row)
        });
        self.last_click = Some((Instant::now(), m.column, m.row));
        let Some((rect, hit)) = under.last().cloned() else {
            self.settings_row = None;
            return true;
        };
        // a popup only reacts to its own areas and to the key chips
        if self.settings_row.is_some() && !matches!(hit, Hit::Popup | Hit::Setting(_) | Hit::Key(_))
        {
            self.settings_row = None;
            return true;
        }
        if self.form.is_some() && !matches!(hit, Hit::Key(_)) {
            return true;
        }
        match hit {
            Hit::Key(k) => return self.on_key(k),
            Hit::Tab(t) if t != self.side.tab => self.switch_tab(),
            Hit::Search => self.side.typing = true,
            Hit::List => self.click_row((m.row - rect.y) as usize, double),
            Hit::Category(i) => {
                self.side.typing = false;
                self.on_side_key(KeyCode::Char((b'1' + i as u8) as char));
            }
            Hit::Volume => {
                let v = (m.column - rect.x) as f64 / rect.width.saturating_sub(1).max(1) as f64;
                self.mpv.set("volume", json!((v * 100.0).round()));
            }
            Hit::Setting(i) => {
                self.settings_row = Some(i);
                self.on_key(KeyCode::Enter);
            }
            _ => {}
        }
        true
    }

    fn save_stations(&mut self) {
        if let Err(e) = save_json("stations.json", &self.stations) {
            self.status = e;
        }
    }

    fn persist_settings(&mut self) {
        if let Err(e) = save_json("settings.json", &self.settings) {
            self.status = e;
        }
    }

    // after a change in the settings page
    fn save_settings(&mut self) {
        self.persist_settings();
        self.meters.delay = Duration::from_millis(self.settings.meter_delay_ms);
        self.art_cache = None;
        self.apply_vis();
    }

    // The filter costs CPU, so it only runs while a meter is on.
    fn apply_vis(&mut self) {
        let want = self.settings.spectrum || self.settings.level_meter;
        if want != self.vis_on {
            self.mpv.cmd(if want {
                json!(["af", "add", vis_filter()])
            } else {
                json!(["af", "remove", "@vis"])
            });
            self.vis_on = want;
        }
    }

    // Redraw only as often as something on screen moves.
    fn frame_interval(&self) -> Duration {
        let sparkles = self.motd.1.elapsed().as_millis() < SPARKLE_MS;
        let buffering = self.now.is_some() && !self.idle && self.core_idle && !self.paused;
        let still = self.now.is_some() && self.idle && !self.side.typing && self.form.is_none();
        let ms = if sparkles || self.meters.moving() {
            33
        } else if buffering
            || self.side.busy()
            || self.recording.is_some()
            || self.reconnect_at.is_some()
        {
            100
        } else if still {
            1000
        } else {
            200
        };
        Duration::from_millis(ms)
    }

    fn playing(&self) -> bool {
        self.now.is_some() && !self.idle && !self.paused && !self.core_idle
    }

    // A new mpv, with the volume and meters of the old one. Then tune in again.
    fn restart_mpv(&mut self, why: &str) {
        self.recording = None;
        match Mpv::start(self.tx.clone(), self.settings.volume) {
            Ok(mpv) => self.mpv = mpv,
            Err(e) => {
                self.status = format!("{why}, cannot restart it: {e}");
                return;
            }
        }
        (self.ping_at, self.unanswered) = (Instant::now(), None);
        self.vis_on = false;
        self.apply_vis();
        let resume = !self.paused && !self.idle;
        (self.paused, self.idle, self.core_idle) = (false, true, true);
        if let (true, Some(st)) = (resume, self.now.clone()) {
            self.play(st);
        }
        self.status = format!("{why}, restarted it");
    }

    // Called every frame: ping mpv, restart it when it hangs, release a long pause.
    fn watchdog(&mut self) {
        if self.ping_at.elapsed() > Duration::from_secs(2) {
            self.mpv
                .request(PING_REQUEST, json!(["get_property", "pid"]));
            self.ping_at = Instant::now();
            self.unanswered.get_or_insert(self.ping_at);
        }
        if self.unanswered.is_some_and(|t| t.elapsed() > MPV_TIMEOUT) {
            return self.restart_mpv("audio engine did not answer");
        }
        // a long pause keeps the audio device and the connection open for nothing
        if self.paused
            && !self.idle
            && self
                .paused_at
                .is_some_and(|t| t.elapsed() > RESUME_LIVE_AFTER)
        {
            self.stop_recording();
            self.mpv.cmd(json!(["stop"]));
        }
    }

    // Live radio: resume a long pause or a dropped stream by tuning in again.
    fn toggle_pause(&mut self) {
        let Some(st) = self.now.clone() else { return };
        let stale = self
            .paused_at
            .is_some_and(|t| t.elapsed() > RESUME_LIVE_AFTER);
        if self.idle || stale {
            self.play(st);
        } else {
            self.mpv.cmd(json!(["cycle", "pause"]));
        }
    }

    fn play(&mut self, st: Station) {
        self.stop_recording();
        self.reconnect_at = None;
        self.mpv.set("pause", json!(false));
        self.mpv.cmd(json!(["loadfile", st.url]));
        self.meta.clear();
        self.codec.clear();
        self.logo = None;
        self.cover = None;
        self.art_cache = None;
        if !st.image.is_empty() {
            load_image(st.image.clone(), self.tx.clone());
        }
        self.status = format!("tuning in to {}", st.url);
        if self.settings.last_url != st.url {
            self.settings.last_url = st.url.clone();
            self.persist_settings();
        }
        self.now = Some(st);
    }

    fn stop_recording(&mut self) {
        if let Some((p, _)) = self.recording.take() {
            self.mpv.set("stream-record", json!(""));
            self.status = format!("recording saved: {}", tilde(&p));
        }
    }

    fn toggle_recording(&mut self) {
        if self.recording.is_some() {
            return self.stop_recording();
        }
        let Some(st) = self.now.as_ref().filter(|_| !self.idle) else {
            self.status = "nothing to record".into();
            return;
        };
        // copy the stream as is, mpv picks the container from the extension
        let ext = match self.codec.as_str() {
            "mp3" => "mp3",
            "aac" => "aac",
            "vorbis" | "opus" => "ogg",
            "flac" => "flac",
            _ => "mka",
        };
        let name: String = st
            .name
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        let stamp = Command::new("date")
            .arg("+%Y%m%d-%H%M%S")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let dir = home().join("Music/bloop");
        let path = dir.join(format!("{name}-{stamp}.{ext}"));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.status = format!("cannot create {}: {e}", tilde(&dir));
            return;
        }
        self.mpv.set("stream-record", json!(path));
        self.status = format!("recording to {}", tilde(&path));
        self.recording = Some((path, Instant::now()));
    }

    fn on_msg(&mut self, m: Msg) {
        match m {
            // handled in run()
            Msg::Term(_) => {}
            Msg::Image(src, img) => {
                if self.now.as_ref().is_some_and(|s| s.image == src) {
                    if img.is_none() {
                        self.status = format!("cannot load image {src}");
                    }
                    self.logo = img;
                    self.art_cache = None;
                }
            }
            Msg::Cover(title, img) => {
                if let Some(img) = img.filter(|_| self.meta.get("icy-title") == Some(&title)) {
                    self.cover = Some((title, img));
                    self.art_cache = None;
                }
            }
            Msg::Browse(id, res) => self.on_browse_data(id, res),
            Msg::MpvGone(id) if id == self.mpv.id => self.restart_mpv("audio engine stopped"),
            // an mpv we replaced ourselves
            Msg::MpvGone(_) => {}
            Msg::Mpv(v) if v["request_id"] == PING_REQUEST => self.unanswered = None,
            Msg::Mpv(v) if v["request_id"] == VIS_REQUEST => {
                if v["data"].is_object() {
                    self.meters.push(&v["data"]);
                }
            }
            Msg::Mpv(v) => match (v["event"].as_str(), v["name"].as_str()) {
                (Some("property-change"), Some(name)) => {
                    let d = &v["data"];
                    match name {
                        "metadata" => {
                            let old = self.meta.remove("icy-title");
                            self.meta = d
                                .as_object()
                                .into_iter()
                                .flatten()
                                .filter_map(|(k, v)| {
                                    Some((k.clone(), v.as_str()?.trim().to_string()))
                                })
                                .filter(|(_, v)| !v.is_empty())
                                .collect();
                            let title = self.meta.get("icy-title");
                            if title != old.as_ref() {
                                self.cover = None;
                                self.art_cache = None;
                                if let Some(t) =
                                    title.filter(|t| self.settings.cover_art && t.contains(" - "))
                                {
                                    load_cover(t.clone(), self.tx.clone());
                                }
                            }
                        }
                        "pause" => {
                            self.paused = d.as_bool().unwrap_or(false);
                            self.paused_at = self.paused.then(Instant::now);
                        }
                        "seekable" => self.live = d.as_bool() == Some(false),
                        "volume" => {
                            self.volume = d.as_f64().unwrap_or(self.volume);
                            if (self.volume - self.settings.volume).abs() > 0.1 {
                                self.settings.volume = self.volume;
                                self.persist_settings();
                            }
                        }
                        "mute" => self.muted = d.as_bool().unwrap_or(false),
                        "core-idle" => self.core_idle = d.as_bool().unwrap_or(true),
                        "idle-active" => {
                            self.idle = d.as_bool().unwrap_or(true);
                            if !self.idle && self.status.starts_with("tuning") {
                                self.status.clear();
                            }
                        }
                        "audio-codec-name" => self.codec = d.as_str().unwrap_or("").to_string(),
                        _ => {}
                    }
                }
                (Some("playback-restart"), _) => self.retries = 0,
                (Some("end-file"), _) => {
                    // "stop" is our own stop or a new station, "eof" on a file is its normal end
                    let reason = v["reason"].as_str().unwrap_or("");
                    let error = v["file_error"].as_str().unwrap_or("");
                    // the sound server is down: reconnecting to the station does not help
                    if error.contains("audio output") {
                        self.stop_recording();
                        self.status = "no sound device: the sound server does not answer \
                            (on WSL, run `wsl --shutdown` in Windows)"
                            .into();
                        return;
                    }
                    let dropped = reason == "error" || (reason == "eof" && self.live);
                    if !dropped || self.now.is_none() {
                        return;
                    }
                    // a reconnect would overwrite the file
                    self.stop_recording();
                    if self.retries < MAX_RETRIES {
                        self.retries += 1;
                        self.reconnect_at =
                            Some(Instant::now() + Duration::from_secs(2 * self.retries as u64));
                        self.status =
                            format!("stream lost, reconnecting ({}/{MAX_RETRIES})", self.retries);
                    } else {
                        let why = v["file_error"].as_str().unwrap_or("stream ended");
                        self.status = format!("playback error: {why}");
                    }
                }
                _ => {}
            },
        }
    }

    // false = quit
    // Ctrl+C quits, Ctrl+U clears the text field, other Ctrl keys do nothing.
    fn on_key_event(&mut self, key: KeyCode, mods: KeyModifiers) -> bool {
        if !mods.contains(KeyModifiers::CONTROL) {
            return self.on_key(key);
        }
        match key {
            KeyCode::Char('c') => return false,
            KeyCode::Char('u') => {
                if let Some(form) = &mut self.form {
                    form.fields[form.idx].1.clear();
                } else if self.side.typing {
                    self.clear_search();
                }
            }
            _ => {}
        }
        true
    }

    fn on_key(&mut self, key: KeyCode) -> bool {
        if let Some(form) = &mut self.form {
            match key {
                KeyCode::Esc => self.form = None,
                KeyCode::Backspace => _ = form.fields[form.idx].1.pop(),
                KeyCode::Char(c) => form.fields[form.idx].1.push(c),
                KeyCode::Enter | KeyCode::Tab if form.idx + 1 < form.fields.len() => form.idx += 1,
                KeyCode::Enter => self.submit_form(),
                _ => {}
            }
            return true;
        }
        if let Some(row) = self.settings_row {
            let s = &mut self.settings;
            match key {
                KeyCode::Esc | KeyCode::Char('c') | KeyCode::Char('q') => self.settings_row = None,
                KeyCode::Up | KeyCode::Char('k') => self.settings_row = Some(row.saturating_sub(1)),
                KeyCode::Down | KeyCode::Char('j') => self.settings_row = Some((row + 1).min(4)),
                KeyCode::Enter | KeyCode::Char(' ') if row == 0 => s.spectrum = !s.spectrum,
                KeyCode::Enter | KeyCode::Char(' ') if row == 1 => s.level_meter = !s.level_meter,
                KeyCode::Enter | KeyCode::Char(' ') if row == 2 => s.cover_art = !s.cover_art,
                KeyCode::Enter | KeyCode::Char(' ') if row == 3 => s.hires_images = !s.hires_images,
                KeyCode::Left | KeyCode::Char('-') if row == 4 => {
                    s.meter_delay_ms = s.meter_delay_ms.saturating_sub(25)
                }
                KeyCode::Right | KeyCode::Char('+') if row == 4 => {
                    s.meter_delay_ms = (s.meter_delay_ms + 25).min(2000)
                }
                _ => return true,
            }
            self.save_settings();
            return true;
        }
        if self.side.typing {
            self.on_typing_key(key);
            return true;
        }
        match key {
            KeyCode::Char('q') => return false,
            KeyCode::Tab => self.switch_tab(),
            KeyCode::Char('/') => self.side.typing = true,
            KeyCode::Char(' ') => self.toggle_pause(),
            KeyCode::Char('s') => {
                self.reconnect_at = None;
                self.stop_recording();
                self.mpv.set("pause", json!(false));
                self.mpv.cmd(json!(["stop"]));
            }
            // relative, so fast key presses never use a stale volume
            KeyCode::Char('+') | KeyCode::Char('=') => self.mpv.cmd(json!(["add", "volume", 5])),
            KeyCode::Char('-') => self.mpv.cmd(json!(["add", "volume", -5])),
            KeyCode::Char('m') => self.mpv.cmd(json!(["cycle", "mute"])),
            KeyCode::Char('r') => self.toggle_recording(),
            KeyCode::Char('c') => self.settings_row = Some(0),
            KeyCode::Char('o') => {
                self.open_form(FormKind::Open, vec![("Stream URL", String::new())])
            }
            _ => self.on_side_key(key),
        }
        true
    }

    fn draw(&mut self, f: &mut Frame) {
        self.hits.clear();
        let [main, motd, status, help] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(2),
        ])
        .areas(f.area());
        f.render_widget(Paragraph::new(self.motd_line(motd.width as usize)), motd);
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(38), Constraint::Fill(1)]).areas(main);
        self.draw_sidebar(f, left);
        self.draw_player(f, right);

        f.render_widget(Paragraph::new(format!(" {}", self.status)).fg(DIM), status);
        match &self.form {
            Some(form) => {
                let (label, value) = &form.fields[form.idx];
                let cursor = if (self.tick / 5).is_multiple_of(2) {
                    "▏"
                } else {
                    " "
                };
                let first = vec![
                    Span::raw(format!(" {label} "))
                        .fg(Color::Black)
                        .bg(ACCENT)
                        .bold(),
                    Span::raw(format!(" {value}{cursor}   ")),
                ];
                self.chip_bar(f, help, first, &[("enter", "next / ok"), ("esc", "cancel")]);
            }
            None => {
                let keys = self.help_keys();
                self.chip_bar(f, help, vec![], &keys);
            }
        }

        if let Some(row) = self.settings_row {
            self.draw_settings(f, row);
        }
    }

    fn draw_settings(&mut self, f: &mut Frame, row: usize) {
        let area = f
            .area()
            .centered(Constraint::Length(58), Constraint::Length(11));
        f.render_widget(Clear, area);
        let block = panel("settings");
        let inner = block.inner(area).inner(Margin::new(1, 1));
        f.render_widget(block, area);
        self.hits.push((area, Hit::Popup));
        for i in 0..5 {
            self.hits.push((
                Rect {
                    y: inner.y + i as u16,
                    height: 1,
                    ..inner
                },
                Hit::Setting(i),
            ));
        }
        let toggle = |on: bool| {
            if on {
                Span::raw("● on ").fg(Color::Green).bold()
            } else {
                Span::raw("○ off").fg(DIM)
            }
        };
        let items = [
            ("Spectrum analyzer", toggle(self.settings.spectrum)),
            ("Audio level meter", toggle(self.settings.level_meter)),
            (
                "Album covers (iTunes lookup)",
                toggle(self.settings.cover_art),
            ),
            (
                match self.picker.as_ref().map(|p| p.protocol_type()) {
                    Some(ProtocolType::Sixel) => "High-res images (sixel)",
                    Some(ProtocolType::Kitty) => "High-res images (kitty)",
                    Some(ProtocolType::Iterm2) => "High-res images (iTerm2)",
                    _ => "High-res images (not supported)",
                },
                toggle(self.settings.hires_images),
            ),
            (
                "Meter delay",
                Span::raw(format!("◂ {:>4} ms ▸", self.settings.meter_delay_ms)).fg(ACCENT),
            ),
        ];
        let w = inner.width as usize;
        let lines: Vec<Line> = items
            .into_iter()
            .enumerate()
            .map(|(i, (label, value))| {
                let pad =
                    w.saturating_sub(label.chars().count() + value.content.chars().count() + 3);
                let line = Line::from(vec![
                    Span::raw(if i == row { " ▸ " } else { "   " }).fg(PINK),
                    Span::raw(label),
                    Span::raw(" ".repeat(pad)),
                    value,
                ]);
                if i == row {
                    line.bg(SEL_BG).bold()
                } else {
                    line
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
        let keys = [
            ("↑↓", "select"),
            ("space", "toggle"),
            ("←→", "adjust"),
            ("esc", "close"),
        ];
        self.chip_bar(
            f,
            Rect {
                y: inner.y + 6,
                height: 1,
                ..inner
            },
            vec![],
            &keys,
        );
    }

    fn station_line(&self, s: &Station, info: Option<&str>) -> Line<'static> {
        let playing = self.now.as_ref().is_some_and(|n| n.url == s.url) && !self.idle;
        let mut l = vec![if playing {
            Span::raw(s.name.clone()).fg(Color::Green).bold()
        } else {
            Span::raw(s.name.clone())
        }];
        if playing {
            let eq = ["▁▃▅", "▂▅▃", "▃▆▂", "▅▂▄", "▆▃▅", "▃▅▇", "▂▄▆", "▄▂▅"][self.tick % 8];
            l.push(if self.playing() {
                Span::raw(format!(" {eq}")).fg(ACCENT)
            } else {
                Span::raw(" ▮▮").fg(DIM)
            });
            if self.recording.is_some() {
                l.push(Span::raw(" ● REC").fg(Color::Red).bold());
            }
        }
        if let Some(info) = info {
            l.push(Span::raw(format!("  {info}")).fg(DIM));
        }
        Line::from(l)
    }

    fn draw_player(&mut self, f: &mut Frame, area: Rect) {
        let block = panel("now playing");
        let inner = block.inner(area);
        f.render_widget(block, area);
        self.hits.push((inner, Hit::Player));
        let vis = self.settings.spectrum || self.settings.level_meter;
        let rec_h = if self.recording.is_some() { 5 } else { 0 };
        let top_c = if vis {
            Constraint::Length(14)
        } else {
            Constraint::Fill(1)
        };
        let vis_c = if vis {
            Constraint::Fill(1)
        } else {
            Constraint::Length(0)
        };
        let [top, vis_area, vol, rec] = Layout::vertical([
            top_c,
            vis_c,
            Constraint::Length(1),
            Constraint::Length(rec_h),
        ])
        .areas(inner);

        let top = top.inner(Margin::new(1, 1));
        let [art, info, sign] = Layout::horizontal([
            Constraint::Length(30),
            Constraint::Fill(1),
            Constraint::Length(10),
        ])
        .spacing(2)
        .areas(top);
        let art = Rect {
            height: art.height.min(12),
            ..art
        };
        self.draw_art(f, art);
        self.draw_info(f, info);
        self.hits.push((art, Hit::Key(KeyCode::Char(' '))));
        self.hits.push((sign, Hit::Key(KeyCode::Char(' '))));
        let (style, border) = if self.playing() {
            (
                Style::new().fg(Color::White).bg(Color::Red).bold(),
                Color::Red,
            )
        } else {
            (Style::new().fg(DIM), BORDER)
        };
        let sign_block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(border));
        f.render_widget(
            Paragraph::new(Line::styled(" ON AIR ", style)).block(sign_block),
            sign.intersection(Rect { height: 3, ..sign }),
        );

        if vis {
            let block = Block::new()
                .borders(Borders::TOP)
                .border_style(Style::new().fg(BORDER))
                .title(Span::raw(" analyzer ").fg(DIM));
            let a = block.inner(vis_area).inner(Margin::new(1, 0));
            f.render_widget(block, vis_area);
            let spec_c = if self.settings.spectrum {
                Constraint::Fill(1)
            } else {
                Constraint::Length(0)
            };
            let vu_c = if self.settings.level_meter {
                Constraint::Length(13)
            } else {
                Constraint::Length(0)
            };
            let [spec, vu] = Layout::horizontal([spec_c, vu_c]).spacing(2).areas(a);
            if self.settings.spectrum {
                self.draw_spectrum(f, spec);
            }
            if self.settings.level_meter {
                self.draw_vu(f, vu);
            }
        }

        let (ratio, label, color) = if self.muted {
            (0.0, "muted".to_string(), Color::Red)
        } else {
            (
                self.volume / 100.0,
                format!("{:>3.0}%", self.volume),
                ACCENT,
            )
        };
        let label_w = label.chars().count() as u16;
        let gauge = LineGauge::default()
            .filled_symbol("━")
            .unfilled_symbol("─")
            .filled_style(Style::new().fg(color))
            .unfilled_style(Style::new().fg(BORDER))
            .ratio(ratio)
            .label(Line::from(vec![
                Span::raw("vol ").fg(DIM),
                Span::raw(label).fg(color).bold(),
            ]));
        let vol = vol.inner(Margin::new(1, 0));
        f.render_widget(gauge, vol);
        // the line starts after "vol 50% "
        let line_x = 4 + label_w + 1;
        self.hits.push((
            Rect {
                x: vol.x + line_x,
                width: vol.width.saturating_sub(line_x),
                ..vol
            },
            Hit::Volume,
        ));

        if let Some((path, started)) = &self.recording {
            let reel = ["◐", "◓", "◑", "◒"][self.tick % 4];
            let tape: String = (0..8).map(|i| ["═", "─"][(i + self.tick) % 2]).collect();
            let cassette = [
                "╭────────────────────╮".to_string(),
                "│ ╭────────────────╮ │".to_string(),
                format!("│ │  {reel} {tape} {reel}  │ │"),
                "│ ╰────────────────╯ │".to_string(),
                "╰────────────────────╯".to_string(),
            ];
            let [c, txt] = Layout::horizontal([Constraint::Length(22), Constraint::Fill(1)])
                .spacing(2)
                .areas(rec.inner(Margin::new(1, 0)));
            f.render_widget(
                Paragraph::new(cassette.map(|s| Line::raw(s).fg(Color::Red)).to_vec()),
                c,
            );
            let dot = if (self.tick / 5).is_multiple_of(2) {
                Span::raw("● REC ").fg(Color::Red).bold()
            } else {
                Span::raw("  REC ").fg(Color::Red).bold()
            };
            let size = std::fs::metadata(path)
                .map(|m| m.len() as f64 / 1e6)
                .unwrap_or(0.0);
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let txt_lines = vec![
                Line::raw(""),
                Line::from(vec![dot, Span::raw(fmt_dur(started.elapsed())).bold()]),
                Line::raw(format!("{size:.1} MB")).fg(DIM),
                Line::raw(marquee(&name, txt.width as usize, self.tick)).fg(DIM),
            ];
            f.render_widget(Paragraph::new(txt_lines), txt);
        }
    }

    fn draw_art(&mut self, f: &mut Frame, area: Rect) {
        // song cover first, then station logo
        if let Some(img) = self.cover.as_ref().map(|(_, i)| i).or(self.logo.as_ref()) {
            if self.art_cache.as_ref().is_none_or(|(a, _)| *a != area) {
                let graphic = self
                    .picker
                    .as_ref()
                    .filter(|_| self.settings.hires_images)
                    .and_then(|p| {
                        p.new_protocol(img.clone(), area.as_size(), Resize::Fit(None))
                            .ok()
                    });
                self.clear_screen = true;
                self.art_cache = Some((
                    area,
                    graphic
                        .map(Art::Graphic)
                        .unwrap_or_else(|| Art::Blocks(halfblock_art(img, area))),
                ));
            }
            match &self.art_cache.as_ref().unwrap().1 {
                Art::Blocks(lines) => f.render_widget(
                    Paragraph::new(lines.clone()).alignment(Alignment::Center),
                    area,
                ),
                Art::Graphic(p) => {
                    let size = p.size();
                    let x = area.x + area.width.saturating_sub(size.width) / 2;
                    f.render_widget(
                        Image::new(p),
                        Rect {
                            x,
                            width: size.width.min(area.width),
                            ..area
                        },
                    );
                }
            }
            return;
        }
        let on = self.playing();
        let wave = if on {
            WAVES[self.tick / 3 % 4]
        } else {
            WAVES[0]
        };
        let (g1, g2) = if on && (self.tick / 2).is_multiple_of(2) {
            ("▒░▒░▒░", "░▒░▒░▒")
        } else {
            ("░▒░▒░▒", "▒░▒░▒░")
        };
        let body = if self.now.is_some() { PINK } else { DIM };
        let knob = if on { Color::Red } else { DIM };
        let mut lines = vec![
            Line::raw(wave).fg(if on { ACCENT } else { DIM }),
            Line::raw("        │        ").fg(body),
            Line::raw("╭───────┴───────╮").fg(body),
        ];
        for (g, mid) in [(g1, "╭───╮"), (g2, "│ ● │"), (g1, "╰───╯")] {
            lines.push(Line::from(vec![
                Span::raw("│ ").fg(body),
                Span::raw(g).fg(body),
                Span::raw("  "),
                Span::raw(mid).fg(if mid.contains('●') { knob } else { body }),
                Span::raw(" │").fg(body),
            ]));
        }
        lines.push(Line::raw("╰───────────────╯").fg(body));
        f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
    }

    // bubbles rise and grow: . -> o -> O
    fn logo_lines(&self) -> Vec<Line<'static>> {
        let step = self.tick / 4;
        let mut lines: Vec<Line> = LOGO
            .iter()
            .enumerate()
            .map(|(row, text)| {
                let mut chars: Vec<char> = format!("{text:27}").chars().collect();
                if row < 3 {
                    let k = (step + row).wrapping_sub(2);
                    for col in [12 + hash(k) % 14, 12 + hash(k + 99) % 14]
                        .into_iter()
                        .take(1 + hash(k).is_multiple_of(3) as usize)
                    {
                        if chars[col] == ' ' {
                            chars[col] = ['O', 'o', '.'][row];
                        }
                    }
                }
                Line::from(
                    chars
                        .iter()
                        .enumerate()
                        .map(|(i, c)| {
                            Span::raw(c.to_string())
                                .fg(lerp(ACCENT, PINK, i as f32 / 27.0))
                                .bold()
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        lines.push(Line::raw(""));
        lines.push(Line::raw("a tiny webradio for your terminal").fg(DIM));
        lines
    }

    // Centered phrase. Letters pop in with a sparkle, little stars twinkle around, then it stays.
    fn motd_line(&mut self, w: usize) -> Line<'static> {
        let (idx, started) = self.motd;
        if started.elapsed() >= MOTD_EVERY {
            self.motd = (idx + 1, Instant::now());
        }
        let t = started.elapsed().as_millis() as usize;
        let text: Vec<char> = MOTD[idx % MOTD.len()].chars().collect();
        let pad = w.saturating_sub(text.len()) / 2;
        let gold = Style::new().fg(Color::Rgb(255, 225, 140));
        let mut cells = vec![Span::raw(" "); w];
        for (i, &c) in text.iter().enumerate().take(w) {
            // each letter has its own moment, with a sparkle just before it
            let at = 100 + hash(i + idx * 31) % 1200;
            cells[pad + i] = match t {
                _ if c == ' ' || t >= at + 160 => Span::raw(c.to_string()).fg(PINK).italic(),
                _ if t >= at => Span::styled(
                    if (at / 10).is_multiple_of(2) {
                        "✦"
                    } else {
                        "✧"
                    },
                    gold,
                ),
                _ => Span::raw(" "),
            };
        }
        if t < SPARKLE_MS as usize {
            for k in 0..8 {
                let d = 2 + hash(k + idx * 7) % 9;
                let col = if k % 2 == 0 {
                    pad.checked_sub(d)
                } else {
                    Some(pad + text.len() + d - 1)
                };
                let twinkle = ["·", "✧", "✦", "⋆", " "][(t / 120 + k) % 5];
                if let Some(col) = col.filter(|&c| c < w) {
                    cells[col] = Span::styled(twinkle, gold);
                }
            }
        }
        Line::from(cells)
    }

    fn draw_info(&self, f: &mut Frame, area: Rect) {
        if self.now.is_none() {
            return f.render_widget(Paragraph::new(self.logo_lines()), area);
        }
        let (label, color) = match () {
            _ if self.reconnect_at.is_some() => (
                format!("{} RECONNECTING", SPINNER[self.tick % 10]),
                Color::Yellow,
            ),
            // a long pause stops mpv, it still is a pause for the user
            _ if self.now.is_some() && self.paused => (
                if (self.tick / 5).is_multiple_of(2) {
                    "▮▮ PAUSED"
                } else {
                    ""
                }
                .to_string(),
                Color::Yellow,
            ),
            _ if self.now.is_none() || self.idle => ("■ STOPPED".to_string(), DIM),
            _ if self.core_idle => (
                format!("{} BUFFERING", SPINNER[self.tick % 10]),
                Color::Yellow,
            ),
            _ => ("► PLAYING".to_string(), Color::Green),
        };
        let mut lines = vec![Line::raw(label).fg(color).bold(), Line::raw("")];
        let w = area.width as usize;
        if let Some(st) = &self.now {
            lines.push(
                Line::raw(marquee(&st.name, w, self.tick))
                    .fg(Color::White)
                    .bold(),
            );
            if let Some(t) = self.meta.get("icy-title") {
                lines.push(Line::raw(marquee(t, w, self.tick)).fg(ACCENT));
            }
            lines.push(Line::raw(""));
            let br = self
                .meta
                .get("icy-br")
                .map(|b| format!(" · {b} kbps"))
                .unwrap_or_default();
            if !self.codec.is_empty() {
                lines.push(Line::raw(format!("{}{br}", self.codec)).fg(DIM));
            }
            for (k, v) in self.meta.iter().filter(|(k, _)| {
                !["icy-title", "icy-br", "icy-pub"].contains(&k.as_str())
                    && !k.starts_with("icy-notice")
            }) {
                lines.push(Line::from(vec![
                    Span::raw(format!("{} ", k.trim_start_matches("icy-"))).fg(DIM),
                    Span::raw(v.clone()),
                ]));
            }
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    fn draw_spectrum(&self, f: &mut Frame, area: Rect) {
        let [bars_area, axis] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
        let n = (bars_area.width as usize).div_ceil(2);
        let h = bars_area.height as usize;
        if n < 2 || h == 0 {
            return;
        }
        let at = |v: &[f32], i: usize| {
            let p = i as f32 * (BANDS - 1) as f32 / (n - 1) as f32;
            let (j, fr) = (p as usize, p.fract());
            (v[j] * (1.0 - fr) + v[(j + 1).min(BANDS - 1)] * fr) * h as f32
        };
        let bars: Vec<(f32, f32)> = (0..n)
            .map(|i| {
                (
                    at(&self.meters.level[2..], i),
                    at(&self.meters.hold[2..], i),
                )
            })
            .collect();
        let lines: Vec<Line> = (0..h)
            .rev()
            .map(|row| {
                let color = heat(row as f32 / (h - 1).max(1) as f32);
                let r = row as f32;
                Line::from(
                    bars.iter()
                        .flat_map(|&(lv, pk)| {
                            let cell = if lv - r > 0.06 {
                                Span::raw(eighth(lv - r)).fg(color)
                            } else if pk > 0.15 && pk.floor() == r {
                                Span::raw(if pk.fract() < 0.5 { "▁" } else { "▔" }).fg(Color::Gray)
                            } else {
                                Span::raw(" ")
                            };
                            [cell, Span::raw(" ")]
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        f.render_widget(Paragraph::new(lines), bars_area);
        f.render_widget(Paragraph::new(freq_axis(2 * n - 1)).fg(DIM), axis);
    }

    fn draw_vu(&self, f: &mut Frame, area: Rect) {
        let [bars_area, labels] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
        let h = bars_area.height as usize;
        if h == 0 {
            return;
        }
        // which dB label goes on which row
        let mut scale = vec![None; h];
        for db in [0, -6, -12, -24, -36, -48, -60] {
            let r = (((db + 60) as f32 / 60.0 * h as f32) as usize).min(h - 1);
            scale[r].get_or_insert(db);
        }
        let lines: Vec<Line> = (0..h)
            .rev()
            .map(|row| {
                let r = row as f32;
                let mut l = match scale[row] {
                    Some(db) => vec![Span::raw(format!("{db:>3}┤")).fg(DIM)],
                    None => vec![Span::raw("   │").fg(BORDER)],
                };
                for ch in 0..2 {
                    let (lv, pk) = (
                        self.meters.level[ch] * h as f32,
                        self.meters.hold[ch] * h as f32,
                    );
                    l.push(Span::raw(" "));
                    let cell = if lv - r > 0.06 {
                        Span::raw(eighth(lv - r).repeat(3)).fg(heat(r / (h - 1).max(1) as f32))
                    } else if pk > 0.1 && pk.floor() == r {
                        Span::raw(if pk.fract() < 0.5 {
                            "▁▁▁"
                        } else {
                            "▔▔▔"
                        })
                        .fg(Color::White)
                    } else {
                        Span::raw("   ")
                    };
                    l.push(cell);
                }
                Line::from(l)
            })
            .collect();
        f.render_widget(Paragraph::new(lines), bars_area);
        f.render_widget(Paragraph::new("dB   L   R ").fg(DIM), labels);
    }
}

fn freq_axis(w: usize) -> String {
    let mut s = vec![' '; w];
    for (f, l) in [
        (50.0, "50"),
        (100.0, "100"),
        (200.0, "200"),
        (500.0, "500"),
        (1e3, "1k"),
        (2e3, "2k"),
        (5e3, "5k"),
        (1e4, "10k"),
        (F_HI, "16k"),
    ] {
        if w < l.len() + 2 {
            break;
        }
        let x = ((f / F_LO).ln() / (F_HI / F_LO).ln() * (w - 1) as f32).round() as usize;
        let start = x.saturating_sub(l.len() / 2).min(w - l.len());
        if s[start.saturating_sub(1)..(start + l.len() + 1).min(w)]
            .iter()
            .all(|&c| c == ' ')
        {
            s.splice(start..start + l.len(), l.chars());
        }
    }
    s.into_iter().collect()
}

// Sleeps until a key, a mouse click, an mpv message or the next animation frame.
fn run(term: &mut DefaultTerminal, app: &mut App, rx: &mpsc::Receiver<Msg>) -> io::Result<()> {
    let start = Instant::now();
    let mut last = start;
    let mut redraw = true;
    loop {
        let now = Instant::now();
        if redraw || now - last >= app.frame_interval() {
            app.tick = (start.elapsed().as_millis() / 100) as usize;
            let live = app.playing() && app.vis_on;
            app.meters.step((now - last).as_secs_f32().min(0.2), live);
            if live {
                app.mpv
                    .request(VIS_REQUEST, json!(["get_property", "af-metadata/vis"]));
            }
            if app.side.search_due() {
                app.side.reload(&app.tx);
            }
            app.watchdog();
            if app.reconnect_at.is_some_and(|t| Instant::now() >= t)
                && let Some(st) = app.now.clone()
            {
                app.play(st);
            }
            if std::mem::take(&mut app.clear_screen) {
                // not term.clear(): it asks the terminal for the cursor position,
                // and the input thread would swallow the answer
                term.backend_mut().clear_region(ClearType::All)?;
                // two swaps leave an empty "previous" buffer, so the next draw repaints all
                term.swap_buffers();
                term.swap_buffers();
            }
            term.draw(|f| app.draw(f))?;
            last = now;
            redraw = false;
        }
        let wait = app.frame_interval().saturating_sub(last.elapsed());
        let Ok(m) = rx.recv_timeout(wait) else {
            continue;
        };
        // meter values are drawn with the next frame, not at once
        redraw = !matches!(&m, Msg::Mpv(v) if v["request_id"] == VIS_REQUEST);
        let go_on = match m {
            Msg::Term(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                app.on_key_event(k.code, k.modifiers)
            }
            Msg::Term(Event::Mouse(m)) => app.on_mouse(m),
            Msg::Term(_) => true,
            m => {
                app.on_msg(m);
                true
            }
        };
        if !go_on {
            return Ok(());
        }
    }
}

// Clicks and the wheel only. EnableMouseCapture also reports every mouse move.
fn mouse(on: bool) {
    let mode = if on { "h" } else { "l" };
    let mut out = io::stdout();
    let _ = write!(out, "\x1b[?1000{mode}\x1b[?1006{mode}");
    let _ = out.flush();
}

// Ask the terminal for its graphics protocol and cell size.
// Runs in a child process: if the terminal never answers, the query leaves a
// thread that eats stdin. That thread dies with the child.
fn probe_graphics() -> Option<Picker> {
    let out = Command::new(std::env::current_exe().ok()?)
        .arg("--probe")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    let out = String::from_utf8_lossy(&out.stderr);
    let mut it = out.split_whitespace();
    let proto = match it.next()? {
        "Sixel" => ProtocolType::Sixel,
        "Kitty" => ProtocolType::Kitty,
        "Iterm2" => ProtocolType::Iterm2,
        _ => return None,
    };
    let font = FontSize::new(it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    // deprecated in favor of the query, which the probe child does
    #[allow(deprecated)]
    let mut picker = Picker::from_fontsize(font);
    picker.set_protocol_type(proto);
    Some(picker)
}

fn main() -> io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--probe") {
        if let Ok(p) = Picker::from_query_stdio() {
            eprintln!(
                "{:?} {} {}",
                p.protocol_type(),
                p.font_size().width,
                p.font_size().height
            );
        }
        return Ok(());
    }
    let settings: Settings = load_json("settings.json").unwrap_or_default();
    let defaults = default_stations();
    let mut stations: Vec<Station> = load_json("stations.json").unwrap_or_else(|| defaults.clone());
    // stations saved before sections existed get the section of the same default station
    for s in stations.iter_mut().filter(|s| s.section.is_empty()) {
        if let Some(d) = defaults.iter().find(|d| d.url == s.url) {
            s.section = d.section.clone();
        }
    }
    let (tx, rx) = mpsc::channel();
    let mpv = Mpv::start(tx.clone(), settings.volume)?;
    let mut term = ratatui::init();
    let settings_volume = settings.volume;
    let mut app = App {
        mpv,
        tx,
        stations,
        side: Sidebar::new(),
        form: None,
        meters: Meters::new(settings.meter_delay_ms),
        settings,
        settings_row: None,
        vis_on: false,
        now: None,
        meta: BTreeMap::new(),
        logo: None,
        cover: None,
        art_cache: None,
        picker: None,
        clear_screen: false,
        paused: false,
        paused_at: None,
        live: false,
        retries: 0,
        reconnect_at: None,
        ping_at: Instant::now(),
        unanswered: None,
        idle: true,
        core_idle: true,
        muted: false,
        volume: settings_volume,
        codec: String::new(),
        recording: None,
        status: String::new(),
        tick: 0,
        hits: vec![],
        last_click: None,
        motd: (
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as usize)
                .unwrap_or(0),
            Instant::now(),
        ),
    };
    app.apply_vis();
    // select the last station, without playing it
    let last = app
        .stations
        .iter()
        .position(|s| s.url == app.settings.last_url);
    app.select_station(last.unwrap_or(usize::MAX));
    // draw first, the probe can take 2 s
    term.draw(|f| app.draw(f))?;
    app.picker = probe_graphics();
    // after the probe, mouse events would confuse it
    mouse(true);
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        mouse(false);
        hook(info);
    }));
    let input = app.tx.clone();
    thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if input.send(Msg::Term(ev)).is_err() {
                break;
            }
        }
    });
    // start the sparkles after the probe, it can block for 2 s
    app.motd.1 = Instant::now();
    let res = run(&mut term, &mut app, &rx);
    mouse(false);
    ratatui::restore();
    res
}
