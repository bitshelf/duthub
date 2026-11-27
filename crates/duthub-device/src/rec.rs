//! Structured record log (`.rec`): the third session-log stream.
//!
//! Fixed-layout, little-endian, self-validating record framing. Headers are
//! written via zerocopy derives (no manual byte packing) and parsed back
//! zero-copy (`FromBytes::ref_from_prefix` over borrowed buffers — no
//! per-record allocation until the caller materializes display text). Format
//! spec: `docs/reference/REC_LOG_FORMAT.md`.
//!
//! Losslessness is inherited from the session-log pipeline: records are
//! written by the same writer thread as `.log`/`.bin` with per-entry flush
//! (AGENTS.md pit #47). `.bin` remains the verbatim-byte authority; `.rec`
//! is the typed, timestamped, indexable companion that also feeds the
//! desktop terminal log view.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use zerocopy::byteorder::little_endian::{U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

/// Magic at file start (`FileHeader`).
pub const FILE_MAGIC: [u8; 8] = *b"DUTHUBR1";
/// Magic at each record start (`RecordHeader`) — lets a reader re-sync after
/// a torn write and reject garbage before trusting `payload_len`.
pub const RECORD_MAGIC: [u8; 4] = *b"REC1";
pub const FORMAT_VERSION: u32 = 1;
/// `.rec` rotation threshold mirrors the raw stream (8 MiB).
pub const REC_ROTATE_BYTES: u64 = 8 * 1024 * 1024;
/// Defensive cap for one record's payload. Serial chunks are <= 4 KiB and
/// keystroke bursts <= 256 B, so the cap only guards adversarial writes;
/// beyond it the payload is truncated for `.rec` only — `.bin` stays complete.
pub const MAX_RECORD_PAYLOAD: usize = 16 * 1024 * 1024;

/// Record payload kinds. Values are part of the on-disk format (little-endian
/// `u32` in `RecordHeader::kind`); 0 is invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecKind {
    /// RX wire bytes (same payload as the `.bin` chunk).
    Raw = 1,
    /// TX wire bytes (same payload as the `.bin` chunk; display adds `[TX]`).
    Tx = 2,
    /// Keystroke audit bytes (`.bin`-only stream, now typed and timestamped).
    Keystrokes = 3,
    /// Annotation line (`[note] …` text without the prefix).
    Note = 4,
    /// Split marker line (`[mark] …` text without the prefix).
    Mark = 5,
}

impl RecKind {
    #[must_use]
    pub fn from_u32(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::Raw),
            2 => Some(Self::Tx),
            3 => Some(Self::Keystrokes),
            4 => Some(Self::Note),
            5 => Some(Self::Mark),
            _ => None,
        }
    }

    /// Short Chinese display tag used by the terminal log view.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Raw => "RX",
            Self::Tx => "TX",
            Self::Keystrokes => "KEY",
            Self::Note => "NOTE",
            Self::Mark => "MARK",
        }
    }
}

/// File preamble: magic + format version, written once when a segment file is
/// created (only when the file is empty, so re-opening an existing segment
/// never duplicates it). `header_len` lets future versions extend the preamble
/// without breaking v1 readers.
#[derive(Debug, FromBytes, IntoBytes, KnownLayout, Unaligned, Immutable)]
#[repr(C)]
pub struct FileHeader {
    pub magic: [u8; 8],
    pub version: U32,
    pub header_len: U32,
    pub created_unix_ns: U64,
}

impl FileHeader {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    #[must_use]
    pub fn new() -> Self {
        Self {
            magic: FILE_MAGIC,
            version: U32::new(FORMAT_VERSION),
            header_len: U32::new(u32::try_from(Self::SIZE).expect("FileHeader size fits in u32")),
            created_unix_ns: U64::new(now_unix_ns()),
        }
    }
}

impl Default for FileHeader {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-record fixed header (24 bytes, unaligned, little-endian). The CRC
/// covers the payload only; the header is guarded by its magic and the
/// `payload_len` sanity bound.
#[derive(Debug, FromBytes, IntoBytes, KnownLayout, Unaligned, Immutable)]
#[repr(C)]
pub struct RecordHeader {
    pub magic: [u8; 4],
    pub kind: U32,
    pub ts_unix_ns: U64,
    pub payload_len: U32,
    pub crc32: U32,
}

impl RecordHeader {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    fn new(kind: RecKind, ts_unix_ns: u64, payload_len: usize, crc32: u32) -> Self {
        Self {
            magic: RECORD_MAGIC,
            kind: U32::new(kind as u32),
            ts_unix_ns: U64::new(ts_unix_ns),
            payload_len: U32::new(
                u32::try_from(payload_len).expect("payload_len bounded by MAX_RECORD_PAYLOAD"),
            ),
            crc32: U32::new(crc32),
        }
    }
}

/// Wall-clock nanoseconds since the Unix epoch (0 on clock failure).
#[must_use]
pub fn now_unix_ns() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    )
    .unwrap_or(u64::MAX)
}

const fn build_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        #[expect(clippy::cast_possible_truncation, reason = "loop index is in 0..256")]
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = build_crc_table();

/// Standard CRC-32 (IEEE 802.3, reflected poly 0xEDB88320; matches zlib /
/// crc32fast). Guard against torn or corrupt record tails on parse.
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc = CRC_TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// Encode one record into `out` (cleared first): header + payload, ready for
/// a single `write_all` (one write per record keeps torn records to OS-crash
/// windows, mirroring the per-entry flush contract).
pub fn encode_record(kind: RecKind, ts_unix_ns: u64, payload: &[u8], out: &mut Vec<u8>) {
    let payload = if payload.len() > MAX_RECORD_PAYLOAD {
        &payload[..MAX_RECORD_PAYLOAD]
    } else {
        payload
    };
    out.clear();
    out.reserve(RecordHeader::SIZE + payload.len());
    let header = RecordHeader::new(kind, ts_unix_ns, payload.len(), crc32(payload));
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(payload);
}

/// One parsed record; `payload` borrows the source buffer (zero-copy read).
#[derive(Debug)]
pub struct RecRecord<'a> {
    pub kind: RecKind,
    pub ts_unix_ns: u64,
    pub payload: &'a [u8],
}

/// Owned variant of [`RecRecord`] for handing records across threads/lifetimes.
#[derive(Debug, Clone)]
pub struct OwnedRecRecord {
    pub kind: RecKind,
    pub ts_unix_ns: u64,
    pub payload: Vec<u8>,
}

impl From<&RecRecord<'_>> for OwnedRecRecord {
    fn from(record: &RecRecord<'_>) -> Self {
        Self {
            kind: record.kind,
            ts_unix_ns: record.ts_unix_ns,
            payload: record.payload.to_vec(),
        }
    }
}

/// Parse result: valid records cover the first `valid_bytes` of `bytes`.
/// A torn or corrupt tail stops parsing there — earlier records are kept,
/// later bytes are ignored (never a panic on truncated input).
#[derive(Debug)]
pub struct RecBatch<'a> {
    pub records: Vec<RecRecord<'a>>,
    pub valid_bytes: usize,
}

/// Parse a buffer of `.rec` bytes. Accepts buffers with or without the file
/// preamble: if the preamble magic matches it is skipped, otherwise parsing
/// starts at byte 0 (a continuation buffer or a preamble-less segment file).
#[must_use]
pub fn parse_records(bytes: &[u8]) -> RecBatch<'_> {
    let mut pos = 0usize;
    if bytes.len() >= FileHeader::SIZE
        && let Ok((header, _)) = FileHeader::ref_from_prefix(bytes)
        && header.magic == FILE_MAGIC
    {
        pos = FileHeader::SIZE;
    }
    let mut records = Vec::new();
    while pos + RecordHeader::SIZE <= bytes.len() {
        let Ok((header, rest)) = RecordHeader::ref_from_prefix(&bytes[pos..]) else {
            break;
        };
        if header.magic != RECORD_MAGIC {
            break;
        }
        let Some(kind) = RecKind::from_u32(header.kind.get()) else {
            break;
        };
        let payload_len = header.payload_len.get() as usize;
        if payload_len > MAX_RECORD_PAYLOAD || rest.len() < payload_len {
            break;
        }
        let payload = &rest[..payload_len];
        if crc32(payload) != header.crc32.get() {
            break;
        }
        records.push(RecRecord {
            kind,
            ts_unix_ns: header.ts_unix_ns.get(),
            payload,
        });
        pos += RecordHeader::SIZE + payload_len;
    }
    RecBatch {
        records,
        valid_bytes: pos,
    }
}

/// Write the file preamble to a freshly created segment writer.
pub fn write_preamble_new(writer: &mut impl std::io::Write) -> std::io::Result<()> {
    writer
        .write_all(FileHeader::new().as_bytes())
        .and_then(|()| writer.flush())
}

/// Write the file preamble to a freshly opened segment file. Callers must
/// invoke this right after opening the file while its length is still 0 —
/// the preamble is never duplicated into an existing (re-opened) segment.
pub fn write_preamble(file: &mut File) -> std::io::Result<()> {
    if file.metadata()?.len() == 0 {
        write_preamble_new(file)?;
    }
    Ok(())
}

/// Incremental reader over one `.rec` segment file. `poll` returns every
/// record completed since the last call; an in-progress (torn) record stays
/// buffered until its remaining bytes arrive on a later poll.
#[derive(Debug)]
pub struct RecTailReader {
    path: PathBuf,
    /// Bytes consumed so far (complete records + preamble).
    offset: u64,
    /// Bytes read from the file after `offset` that do not yet form a
    /// complete record.
    pending: Vec<u8>,
}

impl RecTailReader {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            offset: 0,
            pending: Vec::new(),
        }
    }

    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Read newly appended bytes and return all newly completed records.
    pub fn poll(&mut self) -> std::io::Result<Vec<OwnedRecRecord>> {
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(self.offset + self.pending.len() as u64))?;
        let mut fresh = Vec::new();
        file.read_to_end(&mut fresh)?;
        self.pending.extend_from_slice(&fresh);

        let batch = parse_records(&self.pending);
        let records: Vec<OwnedRecRecord> = batch.records.iter().map(OwnedRecRecord::from).collect();
        self.offset += batch.valid_bytes as u64;
        self.pending.drain(..batch.valid_bytes);
        Ok(records)
    }
}

/// List the `.rec` segments of a session in chronological order (main segment
/// first, then rotation `.1`, `.2`, … by numeric suffix). Mirrors
/// `logs::list_segments` but filters strictly on the `.rec` family so the
/// log/bin listing and its tests are untouched.
#[must_use]
pub fn list_rec_segments(dir: &Path, session_id: &str) -> Vec<PathBuf> {
    let prefix = format!("{session_id}.rec");
    let mut found: Vec<(u32, PathBuf)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(suffix) = name.strip_prefix(&prefix) else {
            continue;
        };
        let rotation = if suffix.is_empty() {
            0
        } else {
            match suffix.strip_prefix('.') {
                Some(number)
                    if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    number.parse().unwrap_or(0)
                }
                _ => continue,
            }
        };
        found.push((rotation, entry.path()));
    }
    found.sort_by_key(|(rotation, _)| *rotation);
    found.into_iter().map(|(_, path)| path).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Test temp dirs live under project target/ (data stays in the project).
    fn test_dir(tag: &str) -> PathBuf {
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/test-tmp/rec")
            .join(format!("{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Encode a batch of records into one buffer (with preamble).
    fn encode_all(records: &[(RecKind, &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(FileHeader::new().as_bytes());
        let mut scratch = Vec::new();
        for (kind, text) in records {
            encode_record(*kind, now_unix_ns(), text.as_bytes(), &mut scratch);
            out.extend_from_slice(&scratch);
        }
        out
    }

    #[test]
    fn crc32_matches_standard_vectors() {
        // zlib reference: crc32(b"123456789") == 0xCBF43926
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn header_layout_is_fixed_and_little_endian() {
        assert_eq!(FileHeader::SIZE, 24);
        assert_eq!(RecordHeader::SIZE, 24);
        let bytes = FileHeader::new().as_bytes().to_vec();
        assert_eq!(&bytes[..8], b"DUTHUBR1");
        // version 1 as LE u32 at offset 8
        assert_eq!(&bytes[8..12], &[1, 0, 0, 0]);
        let mut record = Vec::new();
        encode_record(RecKind::Raw, 0x0102_0304_0506_0708, b"ab", &mut record);
        assert_eq!(&record[..4], b"REC1");
        // kind = Raw(1) LE at offset 4
        assert_eq!(&record[4..8], &[1, 0, 0, 0]);
        // ts LE at offset 8..16
        assert_eq!(
            &record[8..16],
            &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
        );
        // payload_len = 2 LE at offset 16
        assert_eq!(&record[16..20], &[2, 0, 0, 0]);
    }

    #[test]
    fn roundtrip_multibyte_payloads() {
        let cases = [
            (
                RecKind::Raw,
                ".boot 洪流\r\nLinux version 6.1 (builder@lab)",
            ),
            (RecKind::Tx, "[TX] uname -a\r"),
            (RecKind::Keystrokes, "\x1b[A\x7f"),
            (RecKind::Note, "[note] 波特率切换 115200→921600 中文注记"),
            (RecKind::Mark, "供电边界 Power ON"),
        ];
        let encoded = encode_all(&cases);
        let batch = parse_records(&encoded);
        assert_eq!(batch.records.len(), cases.len());
        assert_eq!(batch.valid_bytes, encoded.len());
        for (record, (kind, text)) in batch.records.iter().zip(cases.iter()) {
            assert_eq!(record.kind, *kind);
            assert_eq!(record.payload, text.as_bytes());
            assert!(record.ts_unix_ns > 0);
        }
    }

    #[test]
    fn torn_tail_stops_at_last_complete_record() {
        let mut encoded = encode_all(&[(RecKind::Raw, "first"), (RecKind::Tx, "second")]);
        // Truncate mid-payload of the second record
        encoded.truncate(encoded.len() - 2);
        let batch = parse_records(&encoded);
        assert_eq!(batch.records.len(), 1);
        assert_eq!(batch.records[0].payload, b"first");
        // valid_bytes stops before the torn record
        assert!(batch.valid_bytes < encoded.len());
    }

    #[test]
    fn corrupt_record_is_rejected_without_panic() {
        let mut encoded = encode_all(&[(RecKind::Raw, "good"), (RecKind::Note, "next")]);
        // Flip a payload byte of the second record (inside its payload region)
        let len = encoded.len();
        encoded[len - 1] ^= 0xFF;
        let batch = parse_records(&encoded);
        assert_eq!(batch.records.len(), 1);
        assert_eq!(batch.records[0].payload, b"good");
    }

    #[test]
    fn empty_and_garbage_buffers_are_safe() {
        let empty = parse_records(&[]);
        assert!(empty.records.is_empty());
        assert_eq!(empty.valid_bytes, 0);
        let garbage = parse_records(b"not a rec file at all ..........");
        assert!(garbage.records.is_empty());
        // Zeroed bytes: invalid magic both as preamble and as record
        let zeros = parse_records(&[0u8; 128]);
        assert!(zeros.records.is_empty());
    }

    #[test]
    fn continuation_buffer_without_preamble_parses() {
        // Records only (as produced by a mid-stream continuation read)
        let mut scratch = Vec::new();
        encode_record(RecKind::Mark, 42, b"bound", &mut scratch);
        let batch = parse_records(&scratch);
        assert_eq!(batch.records.len(), 1);
        assert_eq!(batch.records[0].kind, RecKind::Mark);
        assert_eq!(batch.records[0].ts_unix_ns, 42);
    }

    #[test]
    fn tail_reader_follows_appends_across_polls() {
        let dir = test_dir("tail");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sess-t.rec");
        {
            let mut file = File::create(&path).unwrap();
            write_preamble(&mut file).unwrap();
            let mut scratch = Vec::new();
            encode_record(RecKind::Raw, 1, "hello 中文".as_bytes(), &mut scratch);
            file.write_all(&scratch).unwrap();
            file.flush().unwrap();
        }
        let mut reader = RecTailReader::new(&path);
        let first = reader.poll().unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].payload, "hello 中文".as_bytes());

        // Append a second record; poll sees only the new one
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            let mut scratch = Vec::new();
            encode_record(RecKind::Tx, 2, b"world", &mut scratch);
            file.write_all(&scratch).unwrap();
            file.flush().unwrap();
        }
        let second = reader.poll().unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].kind, RecKind::Tx);
        assert_eq!(second[0].payload, b"world");
        assert_eq!(reader.offset(), path.metadata().unwrap().len());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_rec_segments_orders_numerically() {
        let dir = test_dir("list");
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "sess-a.rec.2",
            "sess-a.rec.10",
            "sess-a.rec.1",
            "sess-a.rec",
        ] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        // Same prefix but a different stream family must be excluded
        std::fs::write(dir.join("sess-a.log"), b"").unwrap();
        std::fs::write(dir.join("other.rec"), b"").unwrap();
        let segments = list_rec_segments(&dir, "sess-a");
        let names: Vec<String> = segments
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            names,
            [
                "sess-a.rec",
                "sess-a.rec.1",
                "sess-a.rec.2",
                "sess-a.rec.10"
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
