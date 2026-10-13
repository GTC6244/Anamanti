//! Generic append-only **JSONL** log: one serialized `T` per line.
//!
//! Backs both the chat log ([`super::chatlog::ChatLog`]) and the prompt log
//! ([`super::promptlog::PromptLog`]). Writes are serialized by a mutex and flushed
//! per record so a separate reader (the GraphRAG ingester, the debug pages) sees
//! each line promptly. Readers never load the whole file:
//!
//! - [`read_tail`] reads **backwards from EOF** in fixed-size blocks and stops as
//!   soon as it has `limit` records, so the cost is bounded by the request, not by
//!   the lifetime of the log;
//! - [`read_after`] streams forward from a [`LogCursor`] (a byte position plus the
//!   matching line index), consuming only complete (`\n`-terminated) lines, so a
//!   tailing reader picks up exactly the records appended since its last call.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::ids::IdMinter;

/// Block size for the backwards tail scan.
const TAIL_BLOCK: usize = 16 * 1024;

/// Append-only JSONL writer for records of type `T`, safe to share across
/// concurrent turns behind an `Arc`.
pub struct JsonlLog<T> {
    path: PathBuf,
    file: Mutex<File>,
    ids: IdMinter,
    _record: PhantomData<fn(&T)>,
}

impl<T: Serialize> JsonlLog<T> {
    /// Open (creating if needed, along with its parent directory) the log at `path`
    /// for appending.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening log at {}", path.display()))?;
        Ok(Self {
            path,
            file: Mutex::new(file),
            ids: IdMinter::new(),
            _record: PhantomData,
        })
    }

    /// The log's path (readers open it separately).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Mint a fresh record id (`{unix_millis}-{counter}`, see [`crate::ids`]).
    pub fn next_id(&self) -> String {
        self.ids.mint("")
    }

    /// Append one record as a single JSON line, flushing so a reader sees it promptly.
    /// The whole line goes out in one `write_all`, so a reader never observes two
    /// records interleaved.
    pub fn append(&self, record: &T) -> Result<()> {
        let mut line = serde_json::to_string(record).context("serializing log record")?;
        line.push('\n');
        let mut file = self.file.lock().unwrap();
        file.write_all(line.as_bytes())
            .with_context(|| format!("appending to {}", self.path.display()))?;
        file.flush()
            .with_context(|| format!("flushing {}", self.path.display()))?;
        Ok(())
    }
}

/// Parse one raw line; blank lines yield `None` silently, malformed ones are logged.
fn parse_line<T: DeserializeOwned>(raw: &[u8], path: &Path) -> Option<T> {
    if raw.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    match serde_json::from_slice::<T>(raw) {
        Ok(rec) => Some(rec),
        Err(e) => {
            log::warn!("skipping malformed line in {}: {e}", path.display());
            None
        }
    }
}

/// Read up to `limit` most-recent records from `path`, **newest first**, reading
/// backwards from EOF so only the tail of the file is touched. Blank and malformed
/// lines are skipped (and don't count toward `limit`). A missing file yields an
/// empty vec. The same records, in the same order, as parsing the whole file and
/// keeping the last `limit` valid ones.
pub fn read_tail<T: DeserializeOwned>(path: impl AsRef<Path>, limit: usize) -> Result<Vec<T>> {
    let path = path.as_ref();
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    let mut pos = file
        .metadata()
        .with_context(|| format!("reading metadata of {}", path.display()))?
        .len();
    let mut out: Vec<T> = Vec::new();
    // Bytes of the file at [pos, end-of-unconsumed) — the not-yet-split segment.
    let mut pending: Vec<u8> = Vec::new();
    loop {
        // Emit every complete line at the end of `pending` (everything after its
        // last newline), newest first.
        while let Some(nl) = pending.iter().rposition(|&b| b == b'\n') {
            if let Some(rec) = parse_line(&pending[nl + 1..], path) {
                out.push(rec);
                if out.len() == limit {
                    return Ok(out);
                }
            }
            pending.truncate(nl);
        }
        if pos == 0 {
            // What remains is the file's first line.
            if let Some(rec) = parse_line(&pending, path) {
                out.push(rec);
            }
            return Ok(out);
        }
        let n = TAIL_BLOCK.min(pos as usize);
        pos -= n as u64;
        let mut block = vec![0u8; n];
        file.seek(SeekFrom::Start(pos))
            .and_then(|_| file.read_exact(&mut block))
            .with_context(|| format!("reading {}", path.display()))?;
        block.extend_from_slice(&pending);
        pending = block;
    }
}

/// A position in a JSONL log: the byte offset just past the last consumed line and
/// the number of lines consumed so far (the legacy line-index high-water mark the
/// ingester persists).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LogCursor {
    /// Byte offset of the first unconsumed byte.
    pub byte: u64,
    /// Lines consumed before `byte` (blank and malformed lines included).
    pub line: u64,
}

/// Locate the cursor for a legacy **line index** by streaming the file once and
/// counting newlines. A line index past the end of the file (it counted a trailing
/// line with no newline yet) maps to EOF, i.e. exactly what the legacy reader had
/// already consumed. A missing file maps to byte 0.
pub fn cursor_at_line(path: impl AsRef<Path>, line: u64) -> Result<LogCursor> {
    let path = path.as_ref();
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LogCursor { byte: 0, line })
        }
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    let mut reader = BufReader::new(file);
    let mut byte = 0u64;
    let mut seen = 0u64;
    let mut buf = Vec::new();
    while seen < line {
        buf.clear();
        let n = reader
            .read_until(b'\n', &mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break; // EOF before `line` complete lines
        }
        byte += n as u64;
        if buf.last() == Some(&b'\n') {
            seen += 1;
        }
    }
    Ok(LogCursor { byte, line })
}

/// Read every complete record after `from`, returning the advanced cursor and the
/// parsed records in log order. Only `\n`-terminated lines are consumed — a line
/// still being written is left for the next call — and blank or malformed lines
/// advance the cursor but yield no record. A missing file, or a cursor past the end
/// (the file was truncated or replaced), yields nothing and leaves the cursor as is.
pub fn read_after<T: DeserializeOwned>(
    path: impl AsRef<Path>,
    from: LogCursor,
) -> Result<(LogCursor, Vec<T>)> {
    let path = path.as_ref();
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((from, Vec::new())),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    let len = file
        .metadata()
        .with_context(|| format!("reading metadata of {}", path.display()))?
        .len();
    if from.byte > len {
        log::warn!(
            "{} is shorter ({len} bytes) than the read position ({}); was it truncated?",
            path.display(),
            from.byte
        );
        return Ok((from, Vec::new()));
    }
    file.seek(SeekFrom::Start(from.byte))
        .with_context(|| format!("seeking {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut cursor = from;
    let mut out = Vec::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = reader
            .read_until(b'\n', &mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 || buf.last() != Some(&b'\n') {
            break; // EOF, or a partial line still being appended
        }
        cursor.byte += n as u64;
        cursor.line += 1;
        if let Some(rec) = parse_line(&buf, path) {
            out.push(rec);
        }
    }
    Ok((cursor, out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Rec {
        n: u32,
    }

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "anamanti-jsonl-{tag}-{}-{}",
            std::process::id(),
            crate::clock::now_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("log.jsonl")
    }

    fn write_raw(path: &Path, s: &str) {
        std::fs::write(path, s).unwrap();
    }

    #[test]
    fn tail_is_newest_first_capped_and_spans_blocks() {
        let path = tmp("tail");
        let log: JsonlLog<Rec> = JsonlLog::open(&path).unwrap();
        // Enough records to cross several TAIL_BLOCK boundaries.
        for n in 0..5000 {
            log.append(&Rec { n }).unwrap();
        }
        let tail: Vec<Rec> = read_tail(&path, 3).unwrap();
        assert_eq!(
            tail,
            vec![Rec { n: 4999 }, Rec { n: 4998 }, Rec { n: 4997 }]
        );
        let all: Vec<Rec> = read_tail(&path, 10_000).unwrap();
        assert_eq!(all.len(), 5000);
        assert_eq!(all.last(), Some(&Rec { n: 0 }));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn tail_skips_blank_and_malformed_and_reads_unterminated_last_line() {
        let path = tmp("tail-messy");
        write_raw(&path, "{\"n\":1}\n\nnot json\n{\"n\":2}\n{\"n\":3}");
        let tail: Vec<Rec> = read_tail(&path, 10).unwrap();
        assert_eq!(tail, vec![Rec { n: 3 }, Rec { n: 2 }, Rec { n: 1 }]);
        let two: Vec<Rec> = read_tail(&path, 2).unwrap();
        assert_eq!(two, vec![Rec { n: 3 }, Rec { n: 2 }]);
        assert!(read_tail::<Rec>(&path, 0).unwrap().is_empty());
        assert!(read_tail::<Rec>(path.with_extension("missing"), 5)
            .unwrap()
            .is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn read_after_consumes_only_complete_lines() {
        let path = tmp("after");
        write_raw(&path, "{\"n\":1}\n\nbad\n{\"n\":2}\n{\"n\":3");
        let (c, recs): (_, Vec<Rec>) = read_after(&path, LogCursor::default()).unwrap();
        assert_eq!(recs, vec![Rec { n: 1 }, Rec { n: 2 }]);
        // Four complete lines (blank + malformed counted), the partial one left.
        assert_eq!(c.line, 4);
        assert_eq!(c.byte, "{\"n\":1}\n\nbad\n{\"n\":2}\n".len() as u64);

        // The partial line completes later and is picked up whole.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"}\n").unwrap();
        let (c2, recs2): (_, Vec<Rec>) = read_after(&path, c).unwrap();
        assert_eq!(recs2, vec![Rec { n: 3 }]);
        assert_eq!(c2.line, 5);

        // Nothing new → nothing read, cursor unchanged.
        let (c3, recs3): (_, Vec<Rec>) = read_after(&path, c2).unwrap();
        assert!(recs3.is_empty());
        assert_eq!(c3, c2);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn cursor_at_line_matches_streaming_position() {
        let path = tmp("cursor");
        write_raw(&path, "{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n");
        let c = cursor_at_line(&path, 2).unwrap();
        assert_eq!(c.line, 2);
        assert_eq!(c.byte, "{\"n\":1}\n{\"n\":2}\n".len() as u64);
        let (_, rest): (_, Vec<Rec>) = read_after(&path, c).unwrap();
        assert_eq!(rest, vec![Rec { n: 3 }]);
        // Past the end → EOF (everything already consumed).
        let end = cursor_at_line(&path, 99).unwrap();
        assert_eq!(end.byte, std::fs::metadata(&path).unwrap().len());
        // Missing file → byte 0.
        assert_eq!(
            cursor_at_line(path.with_extension("missing"), 3).unwrap(),
            LogCursor { byte: 0, line: 3 }
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
