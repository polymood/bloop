// Song history, liked songs, ListenBrainz and the clipboard.
use super::*;

// unliked songs over this number are dropped, oldest first
const MAX_HISTORY: usize = 1000;
// shorter listens are not sent to ListenBrainz
const MIN_LISTEN: Duration = Duration::from_secs(30);
const LISTENBRAINZ: &str = "https://api.listenbrainz.org/1/";

#[derive(Clone, Serialize, Deserialize)]
pub struct Song {
    pub title: String,
    pub station: String,
    pub url: String,
    // local time, "2026-10-30 15:42"
    pub time: String,
    // seconds since 1970, for ListenBrainz
    pub unix: u64,
    #[serde(default)]
    pub liked: bool,
}

// "Artist - Title" -> ("Artist", "Title")
pub fn artist_track(title: &str) -> Option<(&str, &str)> {
    let (a, t) = title.split_once(" - ")?;
    let (a, t) = (a.trim(), t.trim());
    (!a.is_empty() && !t.is_empty()).then_some((a, t))
}

pub fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = c.iter().enumerate().fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= c.len() {
                ABC[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

// OSC 52: the terminal puts the text on the clipboard (Windows Terminal, kitty, iTerm2...)
pub fn copy(text: &str) {
    let mut out = io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let _ = out.flush();
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl App {
    fn save_songs(&mut self) {
        if let Err(e) = save_json("history.json", &self.songs) {
            self.status = e;
        }
    }

    // A new song title (None: no title any more): add it to the history.
    pub fn on_title(&mut self, title: Option<String>) {
        self.end_listen();
        let (Some(title), Some(st)) = (title, self.now.as_ref()) else {
            return;
        };
        // a reconnect gives the same title again
        if !self
            .songs
            .last()
            .is_some_and(|s| s.title == title && s.url == st.url)
        {
            self.songs.push(Song {
                title: title.clone(),
                station: st.name.clone(),
                url: st.url.clone(),
                time: date("+%Y-%m-%d %H:%M"),
                unix: unix_now(),
                liked: false,
            });
            if self.songs.len() > MAX_HISTORY
                && let Some(i) = self.songs.iter().position(|s| !s.liked)
            {
                self.songs.remove(i);
            }
            self.save_songs();
        }
        self.listening = Some((title, unix_now(), Instant::now()));
    }

    // The current song ends (a new title, a new station or a stop): send it to ListenBrainz.
    // ponytail: a pause counts as listening time
    pub fn end_listen(&mut self) {
        let Some((title, unix, started)) = self.listening.take() else {
            return;
        };
        let token = self.settings.listenbrainz_token.clone();
        let Some((artist, track)) = artist_track(&title) else {
            return;
        };
        if token.is_empty() || started.elapsed() < MIN_LISTEN {
            return;
        }
        let body = json!({
            "listen_type": "single",
            "payload": [{
                "listened_at": unix,
                "track_metadata": {
                    "artist_name": artist,
                    "track_name": track,
                    "additional_info": {
                        "submission_client": "bloop",
                        "submission_client_version": env!("CARGO_PKG_VERSION"),
                    },
                },
            }],
        });
        let tx = self.tx.clone();
        thread::spawn(move || {
            let res = ureq::post(format!("{LISTENBRAINZ}submit-listens"))
                .header("Authorization", format!("Token {token}"))
                .header("User-Agent", USER_AGENT)
                .send_json(&body);
            if let Err(e) = res {
                let _ = tx.send(Msg::Status(format!("ListenBrainz: {e}")));
            }
        });
    }

    // Tells the user at once if the token works.
    pub fn check_token(&self) {
        let token = self.settings.listenbrainz_token.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let res = ureq::get(format!("{LISTENBRAINZ}validate-token"))
                .header("Authorization", format!("Token {token}"))
                .header("User-Agent", USER_AGENT)
                .call()
                .and_then(|mut r| r.body_mut().read_json::<Value>());
            let msg = match res {
                Ok(v) if v["valid"] == true => format!(
                    "ListenBrainz: songs go to {}",
                    v["user_name"].as_str().unwrap_or("your account")
                ),
                Ok(_) => "ListenBrainz: this token is not valid".into(),
                Err(e) => format!("ListenBrainz: {e}"),
            };
            let _ = tx.send(Msg::Status(msg));
        });
    }

    // the history entry of the song that plays now
    fn current_song(&self) -> Option<usize> {
        let title = self.meta.get("icy-title")?;
        let url = &self.now.as_ref()?.url;
        self.songs
            .iter()
            .rposition(|s| &s.title == title && &s.url == url)
    }

    pub fn current_liked(&self) -> bool {
        self.current_song().is_some_and(|i| self.songs[i].liked)
    }

    // Likes or unlikes a history entry, or the current song.
    pub fn toggle_like(&mut self, idx: Option<usize>) {
        let Some(s) = idx
            .or_else(|| self.current_song())
            .and_then(|i| self.songs.get_mut(i))
        else {
            self.status = "no song title to like".into();
            return;
        };
        s.liked = !s.liked;
        self.status = format!("{} {}", if s.liked { "liked" } else { "unliked" }, s.title);
        self.save_songs();
    }

    pub fn copy_title(&mut self, title: Option<String>) {
        let Some(t) = title.or_else(|| self.meta.get("icy-title").cloned()) else {
            self.status = "no song title to copy".into();
            return;
        };
        copy(&t);
        self.status = format!("copied {t}");
    }

    pub fn delete_song(&mut self, i: usize) {
        if i < self.songs.len() {
            let s = self.songs.remove(i);
            self.status = format!("deleted {}", s.title);
            self.save_songs();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }

    #[test]
    fn artist_track_splits_on_the_first_dash() {
        assert_eq!(
            artist_track("Daft Punk - One More Time - Live"),
            Some(("Daft Punk", "One More Time - Live"))
        );
        assert_eq!(artist_track("Station jingle"), None);
        assert_eq!(artist_track(" - Song"), None);
    }
}
