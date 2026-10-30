// Audio engine: download the stream, strip the ICY song titles, decode, play.
use std::{
    collections::{BTreeMap, VecDeque},
    fs::File,
    io::{self, BufRead, BufReader, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed},
        mpsc::Sender,
    },
    thread,
    time::{Duration, Instant},
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use fdk_aac::dec::{Decoder as AacDecoder, DecoderError, Transport};
use symphonia::core::{
    codecs::audio::AudioDecoderOptions,
    errors::Error as SymError,
    formats::{FormatOptions, TrackType, probe::Hint},
    io::{MediaSourceStream, ReadOnlySource},
    meta::MetadataOptions,
};

use crate::{Msg, USER_AGENT};

// played frames kept for the meters: enough for a 4096 point FFT
pub const SCOPE_FRAMES: usize = 4096;
// decoded audio waiting for the sound card
const QUEUE_SECONDS: usize = 2;
// Sound card period, the sound server buffers two (250 ms, as mpv). PulseAudio's default
// buffer is 2 s and plays on for 2 s after a stop.
const PERIOD_SECONDS: f32 = 0.125;
// the sound card opens once this much is queued, at least the full buffer of the sound
// server: its first request takes it all
const PREBUFFER_SECONDS: f32 = 1.0;
// The sound server must take the audio in real time. If it falls this far behind the clock,
// the stream is stuck: open it again. cpal's PulseAudio client does this on WSLg after a
// few minutes, and the audio cuts in and out.
// ponytail: clock drift of the sound card also adds up, 1 s every few hours at 100 ppm: one skip
const STALL_SECONDS: f32 = 1.0;
// no data for this long: the connection is dead
const READ_TIMEOUT: Duration = Duration::from_secs(15);

pub enum AudioEvent {
    // first decoded audio
    Started { codec: &'static str, rate: u32 },
    // station headers (icy-name, icy-genre, icy-br, ...) and song titles (icy-title)
    Meta(BTreeMap<String, String>),
    // Ok: the source ended. Err: it failed or the connection dropped.
    Ended(Result<(), String>),
}

// State shared by the UI, the stream thread and the sound card callback.
struct Shared {
    stop: AtomicBool,
    paused: AtomicBool,
    muted: AtomicBool,
    // f32 bits
    gain: AtomicU32,
    // the queue ran empty: the sound card plays silence until enough audio is queued again
    starving: AtomicBool,
    // sample rate of the sound card stream
    rate: AtomicU32,
    // samples the sound card asked for, played or not
    pulled: AtomicUsize,
    queue: Mutex<VecDeque<f32>>,
    room: Condvar,
    // stereo frames, interleaved, before the volume
    scope: Mutex<VecDeque<f32>>,
    record: Mutex<Option<File>>,
}

pub struct Engine {
    tx: Sender<Msg>,
    cur: Option<Arc<Shared>>,
    session: u64,
    gain: f32,
    muted: bool,
}

// mpv-like volume curve: 50 % sounds like half as loud
fn gain(volume: f64) -> f32 {
    (volume as f32 / 100.0).powi(3)
}

impl Engine {
    pub fn new(tx: Sender<Msg>, volume: f64) -> Self {
        Engine {
            tx,
            cur: None,
            session: 0,
            gain: gain(volume),
            muted: false,
        }
    }

    // events of older sessions are ignored
    pub fn session(&self) -> u64 {
        self.session
    }

    pub fn play(&mut self, url: &str) {
        self.stop();
        self.session += 1;
        let sh = Arc::new(Shared {
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            muted: AtomicBool::new(self.muted),
            gain: AtomicU32::new(self.gain.to_bits()),
            starving: AtomicBool::new(true),
            rate: AtomicU32::new(0),
            pulled: AtomicUsize::new(0),
            queue: Mutex::new(VecDeque::new()),
            room: Condvar::new(),
            scope: Mutex::new(VecDeque::new()),
            record: Mutex::new(None),
        });
        let (tx, id, url) = (self.tx.clone(), self.session, url.to_string());
        let thread_sh = sh.clone();
        thread::spawn(move || {
            let res = run(&url, &thread_sh, &tx, id);
            if !thread_sh.stop.load(Relaxed) {
                let _ = tx.send(Msg::Audio(id, AudioEvent::Ended(res)));
            }
        });
        self.cur = Some(sh);
    }

    pub fn stop(&mut self) {
        if let Some(sh) = self.cur.take() {
            sh.stop.store(true, Relaxed);
            sh.room.notify_all();
        }
    }

    pub fn set_paused(&self, paused: bool) {
        if let Some(sh) = &self.cur {
            sh.paused.store(paused, Relaxed);
        }
    }

    pub fn set_volume(&mut self, volume: f64) {
        self.gain = gain(volume);
        if let Some(sh) = &self.cur {
            sh.gain.store(self.gain.to_bits(), Relaxed);
        }
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        if let Some(sh) = &self.cur {
            sh.muted.store(muted, Relaxed);
        }
    }

    // playing, but the sound card gets no data
    pub fn starving(&self) -> bool {
        self.cur
            .as_ref()
            .is_some_and(|sh| sh.starving.load(Relaxed))
    }

    // Writes the stream bytes as they come, no re-encoding. None closes the file.
    pub fn record(&self, file: Option<File>) {
        if let Some(sh) = &self.cur {
            *sh.record.lock().unwrap() = file;
        }
    }

    // the last played stereo frames, interleaved
    pub fn scope(&self, out: &mut Vec<f32>) {
        out.clear();
        if let Some(sh) = &self.cur {
            out.extend(sh.scope.lock().unwrap().iter());
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(url: &str, sh: &Arc<Shared>, tx: &Sender<Msg>, id: u64) -> Result<(), String> {
    let meta_tx = tx.clone();
    let send_meta = move |m: BTreeMap<String, String>| {
        let _ = meta_tx.send(Msg::Audio(id, AudioEvent::Meta(m)));
    };
    let (body, headers) = open(url, 0)?;
    let ctype = headers.get("content-type").cloned().unwrap_or_default();
    let meta: BTreeMap<String, String> = headers
        .iter()
        .filter(|(k, v)| k.starts_with("icy-") && *k != "icy-metaint" && !v.is_empty())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if !meta.is_empty() {
        send_meta(meta);
    }
    let metaint = headers
        .get("icy-metaint")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let icy = IcyReader {
        inner: body,
        metaint,
        left: metaint,
        sh: sh.clone(),
        on_title: Box::new(send_meta),
    };
    let mut out = Output {
        sh: sh.clone(),
        stream: None,
        rate: 0,
        check: None,
        tx: tx.clone(),
        id,
    };
    let kind = format!("{ctype} {}", url.to_lowercase());
    if kind.contains("aac") {
        decode_aac(icy, &mut out)
    } else {
        decode_symphonia(icy, &kind, &mut out)
    }
}

// ---------- network ----------

type Headers = BTreeMap<String, String>;

// HTTP/1.0 GET, written by hand: old Shoutcast servers answer "ICY 200 OK", which HTTP
// libraries refuse. Follows redirects and playlists (.pls, .m3u). Plain paths open a file.
fn open(url: &str, depth: u32) -> Result<(Box<dyn Read + Send + Sync>, Headers), String> {
    if depth > 5 {
        return Err("too many redirects".into());
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        let f = File::open(url).map_err(|e| format!("{url}: {e}"))?;
        return Ok((Box::new(f), Headers::new()));
    };
    let tls = match scheme {
        "http" => false,
        "https" => true,
        _ => return Err(format!("unsupported URL: {url}")),
    };
    let (hostport, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let path = if path.is_empty() { "/" } else { path };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (h, p.parse().unwrap()),
        _ => (hostport, if tls { 443 } else { 80 }),
    };
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("{host}: {e}"))?
        .next()
        .ok_or_else(|| format!("{host}: no address"))?;
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
        .map_err(|e| format!("{host}: {e}"))?;
    tcp.set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let mut conn: Box<dyn ReadWrite> = if tls {
        Box::new(tls_stream(host, tcp)?)
    } else {
        Box::new(tcp)
    };
    write!(
        conn,
        "GET {path} HTTP/1.0\r\nHost: {hostport}\r\nUser-Agent: {USER_AGENT}\r\nIcy-MetaData: 1\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    let mut r = BufReader::new(conn);
    let mut status = String::new();
    r.read_line(&mut status).map_err(|e| e.to_string())?;
    let mut headers = Headers::new();
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).map_err(|e| e.to_string())? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    // "HTTP/1.1 200 OK" or "ICY 200 OK"
    let code = status.split_whitespace().nth(1).unwrap_or("");
    if matches!(code, "301" | "302" | "303" | "307" | "308") {
        let to = headers
            .get("location")
            .ok_or("redirect without a location")?;
        let to = if to.starts_with('/') {
            format!("{scheme}://{hostport}{to}")
        } else {
            to.clone()
        };
        return open(&to, depth + 1);
    }
    if code != "200" {
        return Err(format!("{host}: {}", status.trim()));
    }
    let ctype = headers
        .get("content-type")
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    let playlist = ["mpegurl", "scpls", "x-pls"]
        .iter()
        .any(|t| ctype.contains(t))
        || [".pls", ".m3u", ".m3u8"]
            .iter()
            .any(|e| path.to_lowercase().ends_with(e));
    if playlist {
        let mut text = String::new();
        r.take(64 * 1024)
            .read_to_string(&mut text)
            .map_err(|e| e.to_string())?;
        if text.contains("#EXT-X-") {
            return Err("HLS streams are not supported".into());
        }
        // m3u: one URL per line, pls: File1=http://...
        let next = text
            .lines()
            .filter_map(|l| l.find("http").map(|i| l[i..].trim()))
            .next()
            .ok_or("empty playlist")?;
        return open(next, depth + 1);
    }
    Ok((Box::new(r), headers))
}

trait ReadWrite: Read + Write + Send + Sync {}
impl<T: Read + Write + Send + Sync> ReadWrite for T {}

fn tls_stream(
    host: &str,
    tcp: TcpStream,
) -> Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>, String> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let name =
        rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| e.to_string())?;
    let conn = rustls::ClientConnection::new(Arc::new(config), name).map_err(|e| e.to_string())?;
    Ok(rustls::StreamOwned::new(conn, tcp))
}

// Removes the ICY metadata blocks from the audio bytes, reports the song titles and
// copies the audio bytes to the recording file.
struct IcyReader {
    inner: Box<dyn Read + Send + Sync>,
    metaint: usize,
    // audio bytes until the next metadata block
    left: usize,
    sh: Arc<Shared>,
    on_title: Box<dyn Fn(BTreeMap<String, String>) + Send + Sync>,
}

impl Read for IcyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.sh.stop.load(Relaxed) {
            return Ok(0);
        }
        if self.metaint > 0 && self.left == 0 {
            let mut len = [0u8];
            self.inner.read_exact(&mut len)?;
            let mut meta = vec![0u8; len[0] as usize * 16];
            self.inner.read_exact(&mut meta)?;
            // StreamTitle='Artist - Song';StreamUrl='...';
            let text = String::from_utf8(meta.clone())
                .unwrap_or_else(|_| meta.iter().map(|&b| b as char).collect());
            if let Some(start) = text.find("StreamTitle='") {
                let rest = &text[start + 13..];
                let title = &rest[..rest
                    .find("';")
                    .unwrap_or(rest.trim_end_matches(['\0', '\'']).len())];
                (self.on_title)(BTreeMap::from([(
                    "icy-title".to_string(),
                    title.trim().to_string(),
                )]));
            }
            self.left = self.metaint;
        }
        let max = if self.metaint > 0 {
            buf.len().min(self.left)
        } else {
            buf.len()
        };
        let n = self.inner.read(&mut buf[..max])?;
        if self.metaint > 0 {
            self.left -= n;
        }
        if let Some(f) = self.sh.record.lock().unwrap().as_mut() {
            f.write_all(&buf[..n])?;
        }
        Ok(n)
    }
}

// ---------- decoders ----------

// AAC and AAC+ (HE-AAC) in ADTS frames, with Fraunhofer's decoder.
fn decode_aac(mut r: impl Read, out: &mut Output) -> Result<(), String> {
    let mut dec = AacDecoder::new(Transport::Adts);
    let mut input = vec![0u8; 8192];
    // largest frame: 2048 samples x 8 channels
    let mut pcm = vec![0i16; 2048 * 8];
    let mut samples = Vec::new();
    loop {
        let n = r.read(&mut input).map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(());
        }
        let mut data = &input[..n];
        while !data.is_empty() {
            let used = dec.fill(data).map_err(|e| e.message().to_string())?;
            data = &data[used..];
            loop {
                match dec.decode_frame(&mut pcm) {
                    Ok(()) => {
                        let info = dec.stream_info();
                        let len = info.frameSize as usize * info.numChannels as usize;
                        samples.clear();
                        samples.extend(pcm[..len].iter().map(|&s| s as f32 / 32768.0));
                        out.push(
                            &samples,
                            info.sampleRate as u32,
                            info.numChannels as usize,
                            "aac",
                        )?;
                    }
                    Err(DecoderError::NOT_ENOUGH_BITS) => break,
                    // a damaged frame: skip it, the next one resyncs
                    Err(e) if e == DecoderError::TRANSPORT_SYNC_ERROR => break,
                    Err(e) => return Err(format!("aac: {}", e.message())),
                }
            }
        }
    }
}

// MP3, Ogg Vorbis and FLAC.
fn decode_symphonia(
    r: impl Read + Send + Sync + 'static,
    kind: &str,
    out: &mut Output,
) -> Result<(), String> {
    let mut hint = Hint::new();
    for (word, ext) in [
        ("mpeg", "mp3"),
        ("mp3", "mp3"),
        ("ogg", "ogg"),
        ("flac", "flac"),
    ] {
        if kind.contains(word) {
            hint.with_extension(ext);
            break;
        }
    }
    let mss = MediaSourceStream::new(Box::new(ReadOnlySource::new(r)), Default::default());
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("unsupported format: {e}"))?;
    let mut samples = Vec::new();
    let mut decoder = None;
    let mut track_id = 0;
    let mut codec = "audio";
    loop {
        // a new decoder at the start, and when an Ogg stream starts a new song
        if decoder.is_none() {
            let track = format
                .default_track(TrackType::Audio)
                .ok_or("no audio track")?;
            let params = track
                .codec_params
                .as_ref()
                .and_then(|p| p.audio())
                .ok_or("no audio track")?;
            track_id = track.id;
            codec = if kind.contains("ogg") {
                "vorbis"
            } else if kind.contains("flac") {
                "flac"
            } else {
                "mp3"
            };
            decoder = Some(
                symphonia::default::get_codecs()
                    .make_audio_decoder(params, &AudioDecoderOptions::default())
                    .map_err(|e| format!("unsupported codec: {e}"))?,
            );
        }
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => return Ok(()),
            Err(SymError::ResetRequired) => {
                decoder = None;
                continue;
            }
            Err(SymError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.to_string()),
        };
        if packet.track_id != track_id {
            continue;
        }
        match decoder.as_mut().unwrap().decode(&packet) {
            Ok(buf) => {
                let (rate, channels) = (buf.spec().rate(), buf.spec().channels().count());
                samples.resize(buf.samples_interleaved(), 0.0);
                buf.copy_to_slice_interleaved(&mut samples);
                out.push(&samples, rate, channels, codec)?;
            }
            // a damaged frame: skip it
            Err(SymError::DecodeError(_)) => {}
            Err(SymError::ResetRequired) => decoder = None,
            Err(e) => return Err(e.to_string()),
        }
    }
}

// ---------- sound card ----------

struct Output {
    sh: Arc<Shared>,
    // not Send on every platform, so it lives in the stream thread
    stream: Option<cpal::Stream>,
    rate: u32,
    // stall check, from the last time the sound server was on time: time and samples pulled
    check: Option<(Instant, usize)>,
    tx: Sender<Msg>,
    id: u64,
}

impl Output {
    // Queues decoded samples as stereo. Waits while the queue is full.
    fn push(
        &mut self,
        samples: &[f32],
        rate: u32,
        channels: usize,
        codec: &'static str,
    ) -> Result<(), String> {
        if channels == 0 || rate == 0 {
            return Ok(());
        }
        if rate != self.rate {
            self.stream = None;
            self.rate = rate;
            self.sh.rate.store(rate, Relaxed);
        }
        let mut q = self.sh.queue.lock().unwrap();
        while q.len() > rate as usize * 2 * QUEUE_SECONDS {
            if self.sh.stop.load(Relaxed) {
                return Err("stopped".into());
            }
            q = self
                .sh
                .room
                .wait_timeout(q, Duration::from_millis(100))
                .unwrap()
                .0;
        }
        for frame in samples.chunks_exact(channels) {
            let (l, r) = (frame[0], frame[channels.min(2) - 1]);
            q.extend([l, r]);
        }
        let ready = q.len() as f32 >= rate as f32 * 2.0 * PREBUFFER_SECONDS;
        drop(q);
        let pulled = self.sh.pulled.load(Relaxed);
        if let Some((t, p)) = self.check {
            let lag = t.elapsed().as_secs_f32() - (pulled - p) as f32 / (rate as f32 * 2.0);
            if lag > STALL_SECONDS {
                self.stream = None;
            } else if lag <= PERIOD_SECONDS {
                self.check = Some((Instant::now(), pulled));
            }
        }
        if self.stream.is_none() && ready {
            let (stream, fixed) = open_device(&self.sh, rate)?;
            stream.play().map_err(|e| format!("sound device: {e}"))?;
            self.stream = Some(stream);
            // the default buffer can ask for more than STALL_SECONDS at once
            self.check = fixed.then(|| (Instant::now(), self.sh.pulled.load(Relaxed)));
            let _ = self
                .tx
                .send(Msg::Audio(self.id, AudioEvent::Started { codec, rate }));
        }
        Ok(())
    }
}

// Stereo f32 output at the stream's own rate: PulseAudio, WASAPI and CoreAudio convert it.
// true: the device took PERIOD_SECONDS
fn open_device(sh: &Arc<Shared>, rate: u32) -> Result<(cpal::Stream, bool), String> {
    #[cfg(target_os = "linux")]
    let host =
        cpal::host_from_id(cpal::HostId::PulseAudio).unwrap_or_else(|_| cpal::default_host());
    #[cfg(not(target_os = "linux"))]
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("sound device: none found")?;
    let build = |buffer_size| {
        let config = cpal::StreamConfig {
            channels: 2,
            sample_rate: rate,
            buffer_size,
        };
        let sh = sh.clone();
        device.build_output_stream(
            config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| fill(&sh, data),
            |_| {},
            None,
        )
    };
    // a device that refuses this period gets its own default
    build(cpal::BufferSize::Fixed((rate as f32 * PERIOD_SECONDS) as u32))
        .map(|s| (s, true))
        .or_else(|_| build(cpal::BufferSize::Default).map(|s| (s, false)))
        .map_err(|e| format!("sound device: {e}"))
}

// The sound card callback: queue -> volume -> speakers, and a copy for the meters.
fn fill(sh: &Shared, data: &mut [f32]) {
    data.fill(0.0);
    sh.pulled.fetch_add(data.len(), Relaxed);
    // stopped: the queue of the old station must not play over the new one
    if sh.paused.load(Relaxed) || sh.stop.load(Relaxed) {
        return;
    }
    let gain = if sh.muted.load(Relaxed) {
        0.0
    } else {
        f32::from_bits(sh.gain.load(Relaxed))
    };
    let mut q = sh.queue.lock().unwrap();
    // At the start and after an underrun, wait until the queue fills this whole request:
    // a bit of audio, then silence, then a bit of audio sounds chopped. The sound server
    // can ask for 2 s at once, more than the prebuffer.
    if sh.starving.load(Relaxed) {
        let second = sh.rate.load(Relaxed) as usize * 2;
        let need = data
            .len()
            .max((second as f32 * PREBUFFER_SECONDS) as usize)
            .min(second * QUEUE_SECONDS);
        if q.len() < need {
            return;
        }
        sh.starving.store(false, Relaxed);
    }
    let n = data.len().min(q.len());
    let mut scope = sh.scope.lock().unwrap();
    for (out, s) in data.iter_mut().zip(q.drain(..n)) {
        scope.push_back(s);
        *out = s * gain;
    }
    let extra = scope.len().saturating_sub(SCOPE_FRAMES * 2);
    scope.drain(..extra);
    if n < data.len() {
        sh.starving.store(true, Relaxed);
    }
    drop((q, scope));
    sh.room.notify_one();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_waits_for_a_full_request() {
        let sh = Shared {
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            gain: AtomicU32::new(1.0f32.to_bits()),
            starving: AtomicBool::new(true),
            // 1 s = 20 samples, prebuffer 1 s
            rate: AtomicU32::new(10),
            pulled: AtomicUsize::new(0),
            queue: Mutex::new(VecDeque::new()),
            room: Condvar::new(),
            scope: Mutex::new(VecDeque::new()),
            record: Mutex::new(None),
        };
        let mut out = [9.0; 30];
        sh.queue.lock().unwrap().extend([0.5; 25]);
        // starting: 25 samples do not fill a 30 sample request, keep them
        fill(&sh, &mut out);
        assert_eq!(out, [0.0; 30]);
        assert_eq!(sh.queue.lock().unwrap().len(), 25);
        // 40 samples: play 30
        sh.queue.lock().unwrap().extend([0.5; 15]);
        fill(&sh, &mut out);
        assert_eq!(out, [0.5; 30]);
        assert!(!sh.starving.load(Relaxed));
        // playing: 10 samples, then silence, then wait again
        fill(&sh, &mut out);
        assert_eq!(out[..10], [0.5; 10]);
        assert_eq!(out[10..], [0.0; 20]);
        assert!(sh.starving.load(Relaxed));
        // after an underrun: the prebuffer (20) is not enough for a request of 30
        sh.queue.lock().unwrap().extend([0.5; 20]);
        fill(&sh, &mut out);
        assert_eq!(out, [0.0; 30]);
    }
}
