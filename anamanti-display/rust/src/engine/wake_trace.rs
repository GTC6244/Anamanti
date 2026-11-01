//! Always-on, on-device wake-word trace.
//!
//! A missed wake word leaves no event: nothing fires, no turn starts, and the Core
//! never hears about it. So the device keeps its own record — one summary line a
//! second from the engine loop, plus events (detections, touches, screen and activity
//! changes, mic state). The moments before a touch are where a miss shows up, since a
//! touch is what people reach for after one.
//!
//! **Numbers only.** No audio and no transcripts ever reach this file.
//!
//! Two files rotate at [`MAX_FILE_BYTES`], so the trace never holds more than about a
//! day. Callers only ever build a line and `try_send` it to a dedicated writer thread:
//! a slow disk drops lines (counted in the next line's `dropped=`) rather than stalling
//! the wake-word loop. Buffered lines reach the file every [`FLUSH_EVERY`].

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TRACE_FILE: &str = "wake_trace.log";
const PREVIOUS_FILE: &str = "wake_trace.1.log";

/// Rotate at this size. At ~120 bytes a second, two files hold about a day.
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;

/// Lines buffered for the writer thread before new ones are dropped.
const CHANNEL_DEPTH: usize = 256;

/// How often buffered lines are pushed to the file.
const FLUSH_EVERY: Duration = Duration::from_secs(30);

/// How long [`tail`] waits for the writer to flush before reading what's there.
const TAIL_FLUSH_WAIT: Duration = Duration::from_secs(2);

/// Bytes read from the end of a file by [`tail`]; ample for the viewer's line count.
const TAIL_BYTES: u64 = 128 * 1024;

/// Summary lines are written at this cadence.
const WINDOW: Duration = Duration::from_secs(1);

/// A window whose peak score reaches this without firing is marked `near`.
const NEAR_MISS_SCORE: f32 = 0.3;

/// OS-side events reported from Kotlin (`MicBridge.nativeTraceEvent`). Integer codes
/// keep the JNI call free of string handling and free text out of the file. Keep in
/// step with `MicBridge.kt`.
fn event_name(code: i32) -> &'static str {
    match code {
        1 => "touch",
        2 => "screen_on",
        3 => "screen_off",
        4 => "activity_start",
        5 => "activity_resume",
        6 => "activity_pause",
        7 => "activity_stop",
        8 => "window_focus",
        9 => "mic_started",
        10 => "mic_stopped",
        11 => "mic_read_error",
        12 => "mic_silenced",
        _ => "unknown_event",
    }
}

enum Msg {
    Line(String),
    Flush(SyncSender<()>),
}

static TX: Mutex<Option<SyncSender<Msg>>> = Mutex::new(None);
static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
static DROPPED: AtomicU64 = AtomicU64::new(0);
static UTC_OFFSET_MINS: AtomicI32 = AtomicI32::new(0);
static STARTED: OnceLock<Instant> = OnceLock::new();

/// Start the writer, appending to any trace already in `dir`. Later calls do nothing,
/// so a restarted engine or a hot restart keeps the one writer.
pub fn start(dir: &str, utc_offset_mins: i32) -> anyhow::Result<()> {
    UTC_OFFSET_MINS.store(utc_offset_mins, Ordering::Relaxed);
    let mut tx_slot = TX.lock().unwrap();
    if tx_slot.is_some() {
        return Ok(());
    }
    let dir = PathBuf::from(dir);
    fs::create_dir_all(&dir)?;
    let mut trace = TraceFile::open(&dir, MAX_FILE_BYTES)?;
    let (tx, rx) = mpsc::sync_channel::<Msg>(CHANNEL_DEPTH);
    std::thread::Builder::new()
        .name("wake-trace".to_string())
        .spawn(move || loop {
            match rx.recv_timeout(FLUSH_EVERY) {
                Ok(Msg::Line(line)) => {
                    if let Err(e) = trace.write_line(&line) {
                        log::warn!("wake trace write failed: {e}");
                    }
                }
                Ok(Msg::Flush(done)) => {
                    let _ = trace.flush();
                    let _ = done.try_send(());
                }
                Err(RecvTimeoutError::Timeout) => {
                    let _ = trace.flush();
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = trace.flush();
                    return;
                }
            }
        })?;
    *tx_slot = Some(tx);
    drop(tx_slot);
    *DIR.lock().unwrap() = Some(dir);
    note(format!("trace_start utc_offset_mins={utc_offset_mins}"));
    Ok(())
}

/// Update the local-time offset used for timestamps (it changes at daylight saving).
pub fn set_utc_offset_mins(mins: i32) {
    UTC_OFFSET_MINS.store(mins, Ordering::Relaxed);
}

/// Append one timestamped line. Never blocks; a no-op until [`start`] has run.
pub fn note(message: impl AsRef<str>) {
    // Not `unwrap`: this runs inside JNI calls and the panic hook, where a poisoned
    // lock must cost a log line, never the process.
    let Ok(tx) = TX.lock() else {
        return;
    };
    let Some(tx) = tx.as_ref() else {
        return;
    };
    let dropped = DROPPED.swap(0, Ordering::Relaxed);
    let mut line = format!("{} {}", stamp(), message.as_ref());
    if dropped > 0 {
        line.push_str(&format!(" dropped={dropped}"));
    }
    line.push('\n');
    if tx.try_send(Msg::Line(line)).is_err() {
        DROPPED.fetch_add(dropped + 1, Ordering::Relaxed);
    }
}

/// Record an OS-side event reported from Kotlin. `value` is event-specific (the mic's
/// sample rate, a read error code, 1/0 for focus or silenced).
pub fn note_event(code: i32, value: i32) {
    note(format!("{} value={value}", event_name(code)));
}

/// The newest `max_lines` lines, oldest first, after flushing what's buffered.
pub fn tail(max_lines: usize) -> String {
    let tx = TX.lock().unwrap().clone();
    if let Some(tx) = tx {
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        if tx.try_send(Msg::Flush(done_tx)).is_ok() {
            let _ = done_rx.recv_timeout(TAIL_FLUSH_WAIT);
        }
    }
    let dir = DIR.lock().unwrap().clone();
    dir.map_or_else(String::new, |d| tail_lines(&d, max_lines))
}

/// The rotating pair of trace files in one directory.
struct TraceFile {
    dir: PathBuf,
    file: BufWriter<File>,
    written: u64,
    max_bytes: u64,
}

impl TraceFile {
    fn open(dir: &Path, max_bytes: u64) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(TRACE_FILE))?;
        let written = file.metadata()?.len();
        Ok(Self {
            dir: dir.to_path_buf(),
            file: BufWriter::new(file),
            written,
            max_bytes,
        })
    }

    fn write_line(&mut self, line: &str) -> io::Result<()> {
        if self.written + line.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        self.file.write_all(line.as_bytes())?;
        self.written += line.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        fs::rename(self.dir.join(TRACE_FILE), self.dir.join(PREVIOUS_FILE))?;
        *self = Self::open(&self.dir, self.max_bytes)?;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// The last `max_lines` lines across the current and previous files, oldest first.
fn tail_lines(dir: &Path, max_lines: usize) -> String {
    let mut lines = read_end(&dir.join(TRACE_FILE));
    if lines.len() < max_lines {
        let mut older = read_end(&dir.join(PREVIOUS_FILE));
        older.append(&mut lines);
        lines = older;
    }
    let skip = lines.len().saturating_sub(max_lines);
    lines[skip..].join("\n")
}

/// The whole lines in the last [`TAIL_BYTES`] of `path`; empty if it can't be read.
fn read_end(path: &Path) -> Vec<String> {
    let read = || -> io::Result<String> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        let start = len.saturating_sub(TAIL_BYTES);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let text = String::from_utf8_lossy(&bytes);
        // A read that starts mid-file starts mid-line.
        let whole = if start > 0 {
            text.split_once('\n').map_or("", |(_, rest)| rest)
        } else {
            &text
        };
        Ok(whole.to_string())
    };
    read()
        .map(|t| t.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Local wall-clock time plus seconds on the monotonic clock. That clock stops while
/// the CPU is suspended, so a jump in the wall clock with no matching jump in `up=`
/// means the device was suspended, while a jump in both means this process was starved.
fn stamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let offset = i64::from(UTC_OFFSET_MINS.load(Ordering::Relaxed)) * 60;
    let up = STARTED.get_or_init(Instant::now).elapsed().as_secs_f32();
    format!(
        "{} up={up:.1}",
        format_local(now.as_secs() as i64 + offset, now.subsec_millis())
    )
}

/// `YYYY-MM-DD HH:MM:SS.mmm` for seconds since the epoch, already offset to local time.
fn format_local(secs: i64, millis: u32) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{millis:03}",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

/// Gregorian (year, month, day) for days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// One second of what the engine loop saw, summarized into a line by [`take_line`].
///
/// [`take_line`]: TraceWindow::take_line
pub struct TraceWindow {
    started: Instant,
    samples: u64,
    zero_samples: u64,
    sum_sq: f64,
    block_rms_max: f32,
    scores: u32,
    peak: f32,
    avg_peak: f32,
    threshold: f32,
    longest_pass: Duration,
    fired: u32,
}

impl TraceWindow {
    pub fn new(now: Instant) -> Self {
        Self {
            started: now,
            samples: 0,
            zero_samples: 0,
            sum_sq: 0.0,
            block_rms_max: 0.0,
            scores: 0,
            peak: 0.0,
            avg_peak: 0.0,
            threshold: 0.0,
            longest_pass: Duration::ZERO,
            fired: 0,
        }
    }

    /// A block of raw mic samples, before gain. Android delivers exact zeros to a
    /// silenced client, so `zero=` separates a blocked mic from a quiet room.
    pub fn on_mic(&mut self, samples: &[i16]) {
        let mut block_sq = 0.0_f64;
        for &s in samples {
            self.zero_samples += u64::from(s == 0);
            block_sq += f64::from(s) * f64::from(s);
        }
        self.samples += samples.len() as u64;
        self.sum_sq += block_sq;
        if !samples.is_empty() {
            let rms = (block_sq / samples.len() as f64).sqrt() / f64::from(i16::MAX);
            self.block_rms_max = self.block_rms_max.max(rms as f32);
        }
    }

    /// How long one pass through the detector took.
    pub fn on_pass(&mut self, elapsed: Duration) {
        self.longest_pass = self.longest_pass.max(elapsed);
    }

    /// A wake-word score, with the smoothed value the gate tested and the threshold.
    pub fn on_score(&mut self, score: f32, avg: f32, threshold: f32) {
        self.scores += 1;
        self.peak = self.peak.max(score);
        self.avg_peak = self.avg_peak.max(avg);
        self.threshold = threshold;
    }

    pub fn on_fire(&mut self) {
        self.fired += 1;
    }

    /// Once a window has elapsed, the summary line, and start the next window.
    /// `backlog` is samples waiting in the capture ring.
    pub fn take_line(&mut self, now: Instant, backlog: usize, turn_active: bool) -> Option<String> {
        let dt = now.duration_since(self.started);
        if dt < WINDOW {
            return None;
        }
        let rms = if self.samples > 0 {
            (self.sum_sq / self.samples as f64).sqrt() / f64::from(i16::MAX)
        } else {
            0.0
        };
        let near = if self.fired == 0 && self.peak >= NEAR_MISS_SCORE {
            " near"
        } else {
            ""
        };
        let line = format!(
            "s dt={} n={} zero={} rms={rms:.4} rms_max={:.4} scores={} peak={:.3} avg={:.3} \
             thr={:.2} pass_ms={} backlog={backlog} turn={} fired={}{near}",
            dt.as_millis(),
            self.samples,
            self.zero_samples,
            self.block_rms_max,
            self.scores,
            self.peak,
            self.avg_peak,
            self.threshold,
            self.longest_pass.as_millis(),
            u8::from(turn_active),
            self.fired,
        );
        *self = Self::new(now);
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory under the system temp dir, unique per test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wake-trace-{name}-{}-{:?}",
            std::process::id(),
            Instant::now()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn formats_local_time() {
        assert_eq!(format_local(0, 0), "1970-01-01 00:00:00.000");
        // 2026-11-01 17:37:05 UTC.
        assert_eq!(format_local(1_793_554_625, 541), "2026-11-01 17:37:05.541");
        // A leap day, then the second before the epoch.
        assert_eq!(format_local(1_709_164_800, 7), "2024-02-29 00:00:00.007");
        assert_eq!(format_local(-1, 0), "1969-12-31 23:59:59.000");
    }

    #[test]
    fn rotates_into_the_previous_file_and_keeps_both() {
        let dir = temp_dir("rotate");
        let mut trace = TraceFile::open(&dir, 20).unwrap();
        trace.write_line("first-line-000\n").unwrap();
        trace.write_line("second-line-00\n").unwrap();
        trace.write_line("third-line-000\n").unwrap();
        trace.flush().unwrap();

        // Only the last rotation survives, so the pair stays bounded.
        assert_eq!(
            fs::read_to_string(dir.join(PREVIOUS_FILE)).unwrap(),
            "second-line-00\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join(TRACE_FILE)).unwrap(),
            "third-line-000\n"
        );
    }

    #[test]
    fn reopening_appends_and_counts_what_is_already_there() {
        let dir = temp_dir("reopen");
        let mut trace = TraceFile::open(&dir, 1000).unwrap();
        trace.write_line("before restart\n").unwrap();
        trace.flush().unwrap();
        drop(trace);

        let mut trace = TraceFile::open(&dir, 1000).unwrap();
        assert_eq!(trace.written, 15);
        trace.write_line("after restart\n").unwrap();
        trace.flush().unwrap();
        assert_eq!(
            fs::read_to_string(dir.join(TRACE_FILE)).unwrap(),
            "before restart\nafter restart\n"
        );
    }

    #[test]
    fn tail_spans_the_rotation_oldest_first() {
        let dir = temp_dir("tail");
        fs::write(dir.join(PREVIOUS_FILE), "a\nb\nc\n").unwrap();
        fs::write(dir.join(TRACE_FILE), "d\ne\n").unwrap();

        assert_eq!(tail_lines(&dir, 3), "c\nd\ne");
        assert_eq!(tail_lines(&dir, 2), "d\ne");
        assert_eq!(tail_lines(&dir, 100), "a\nb\nc\nd\ne");
    }

    #[test]
    fn tail_of_nothing_is_empty() {
        assert_eq!(tail_lines(&temp_dir("empty"), 10), "");
    }

    #[test]
    fn tail_never_returns_a_cut_off_first_line() {
        let dir = temp_dir("cut");
        let line = "x".repeat(99);
        let text = format!("{line}\n").repeat(2000); // 200 KB, more than TAIL_BYTES.
        fs::write(dir.join(TRACE_FILE), text).unwrap();

        let tail = tail_lines(&dir, 5000);
        assert!(tail.lines().count() > 1000);
        assert!(tail.lines().all(|l| l == line));
    }

    #[test]
    fn a_window_summarizes_then_resets() {
        let t0 = Instant::now();
        let mut w = TraceWindow::new(t0);
        w.on_mic(&[0, 0, 1000, -1000]);
        w.on_pass(Duration::from_millis(12));
        w.on_pass(Duration::from_millis(40));
        w.on_score(0.1, 0.05, 0.5);
        w.on_score(0.2, 0.15, 0.5);

        assert!(w
            .take_line(t0 + Duration::from_millis(999), 0, false)
            .is_none());
        let line = w
            .take_line(t0 + Duration::from_millis(1000), 320, true)
            .unwrap();
        assert_eq!(
            line,
            "s dt=1000 n=4 zero=2 rms=0.0216 rms_max=0.0216 scores=2 peak=0.200 avg=0.150 \
             thr=0.50 pass_ms=40 backlog=320 turn=1 fired=0"
        );

        let next = w
            .take_line(t0 + Duration::from_millis(2000), 0, false)
            .unwrap();
        assert!(next.starts_with("s dt=1000 n=0 zero=0 rms=0.0000"));
    }

    #[test]
    fn a_silenced_mic_reads_as_all_zeros() {
        let t0 = Instant::now();
        let mut w = TraceWindow::new(t0);
        w.on_mic(&[0; 160]);
        let line = w.take_line(t0 + WINDOW, 0, false).unwrap();
        assert!(line.contains(" n=160 zero=160 rms=0.0000"), "{line}");
    }

    #[test]
    fn a_high_score_is_near_only_if_it_did_not_fire() {
        let t0 = Instant::now();
        let mut w = TraceWindow::new(t0);
        w.on_score(0.45, 0.2, 0.5);
        assert!(w
            .take_line(t0 + WINDOW, 0, false)
            .unwrap()
            .ends_with(" near"));

        let t1 = t0 + WINDOW;
        w.on_score(0.9, 0.8, 0.5);
        w.on_fire();
        let fired = w.take_line(t1 + WINDOW, 0, false).unwrap();
        assert!(fired.ends_with("fired=1"), "{fired}");
    }

    #[test]
    fn unknown_event_codes_stay_harmless() {
        assert_eq!(event_name(1), "touch");
        assert_eq!(event_name(12), "mic_silenced");
        assert_eq!(event_name(999), "unknown_event");
    }
}
