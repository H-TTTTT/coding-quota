//! Read-only Desktop Antigravity App generation accounting (`~/.gemini/antigravity/conversations/*.db`).
//!
//! `CortexStepGeneratorMetadata`: 1 = chat_model, 2 = repeated step_indices.
//! `ChatModelMetadata`: 4 = usage, 9 = chat_start_metadata, 19 = response_model,
//! 21 = model_display_name, 22 = response_model_full. ChatStartMetadata.4 is
//! created_at, but current databases omit it; each referenced step's
//! CortexStepMetadata.1 (created_at) supplies that generation's real timestamp.
//! No conversation-wide model or filesystem timestamp is used for attribution.

use super::UsageEvent;
use crate::model::ProviderId;
use rusqlite::{types::ValueRef, Connection, OpenFlags};
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

const DATABASE_SETTLE_TIME: Duration = Duration::from_secs(3);

pub(super) fn read_database(path: &Path) -> Option<Vec<UsageEvent>> {
    read_database_at(path, SystemTime::now())
}

fn read_database_at(path: &Path, now: SystemTime) -> Option<Vec<UsageEvent>> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    // On filesystems without birth time, modification time is only a conservative
    // readiness gate. It never becomes an event's timestamp.
    let created = metadata.created().or_else(|_| metadata.modified()).ok()?;
    if now.duration_since(created).ok()? < DATABASE_SETTLE_TIME {
        return None;
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    connection.busy_timeout(Duration::from_millis(50)).ok()?;
    // One short read snapshot covers both tables and includes committed WAL data.
    // In particular, do not use immutable=1, which can silently ignore the WAL.
    let transaction = connection.unchecked_transaction().ok()?;
    let mut step_times = HashMap::new();
    {
        let mut statement = transaction
            .prepare("SELECT idx, metadata FROM steps")
            .ok()?;
        let mut rows = statement.query([]).ok()?;
        while let Some(row) = rows.next().ok()? {
            let (Ok(index), Ok(ValueRef::Blob(data))) = (row.get::<_, i64>(0), row.get_ref(1))
            else {
                continue;
            };
            if let Some(timestamp) = step_created_at(data) {
                step_times.insert(index, timestamp);
            }
        }
    }
    let mut events = Vec::new();
    let mut statement = transaction
        .prepare("SELECT data FROM gen_metadata ORDER BY idx")
        .ok()?;
    let mut rows = statement.query([]).ok()?;
    while let Some(row) = rows.next().ok()? {
        let Ok(ValueRef::Blob(data)) = row.get_ref(0) else {
            continue;
        };
        if let Some(event) = read_call(data, &step_times) {
            events.push(event);
        }
    }
    Some(events)
}

fn step_created_at(data: &[u8]) -> Option<i64> {
    let mut created = None;
    for field in Fields::new(data) {
        let field = field.ok()?;
        if field.number == 1 {
            created = Some(timestamp(field.value.bytes()?)?);
        }
    }
    created
}

fn timestamp(data: &[u8]) -> Option<i64> {
    let mut seconds = 0_i64;
    let mut nanos = 0;
    for field in Fields::new(data) {
        let field = field.ok()?;
        match field.number {
            1 => seconds = field.value.varint()? as i64,
            2 => nanos = field.value.varint()?,
            _ => {}
        }
    }
    // google.protobuf.Timestamp's specified range; reject damaged timestamps.
    if !(-62_135_596_800..=253_402_300_799).contains(&seconds) || nanos >= 1_000_000_000 {
        return None;
    }
    Some(seconds)
}

fn read_call(data: &[u8], step_times: &HashMap<i64, i64>) -> Option<UsageEvent> {
    let mut chat = None;
    let mut step_time = None;
    for field in Fields::new(data) {
        let field = field.ok()?;
        match field.number {
            1 => chat = Some(field.value.bytes()?),
            2 => match field.value {
                WireValue::Varint(index) => record_step_time(index, step_times, &mut step_time),
                WireValue::Bytes(mut packed) => {
                    while !packed.is_empty() {
                        record_step_time(take_varint(&mut packed)?, step_times, &mut step_time);
                    }
                }
                WireValue::Fixed => return None,
            },
            _ => {}
        }
    }
    let mut usage = None;
    let mut model = None;
    let mut full_model = None;
    let mut display_model = None;
    let mut call_time = None;
    for field in Fields::new(chat?) {
        let field = field.ok()?;
        match field.number {
            4 => usage = Some(field.value.bytes()?),
            9 => call_time = step_created_at_field(field.value.bytes()?, 4)?,
            19 => model = Some(field.value.bytes()?),
            21 => display_model = Some(field.value.bytes()?),
            22 => full_model = Some(field.value.bytes()?),
            _ => {}
        }
    }
    let total_tokens = usage_total(usage?)?;
    if total_tokens == 0 {
        return None;
    }
    let model = [model, full_model, display_model]
        .into_iter()
        .flatten()
        .filter_map(|value| std::str::from_utf8(value).ok())
        .find(|value| !value.trim().is_empty())?;
    Some(UsageEvent {
        provider: ProviderId::Antigravity,
        timestamp: call_time.or(step_time)?,
        model: model.to_owned(),
        total_tokens,
    })
}

fn record_step_time(index: u64, times: &HashMap<i64, i64>, earliest: &mut Option<i64>) {
    let Ok(index) = i64::try_from(index) else {
        return;
    };
    if let Some(&timestamp) = times.get(&index) {
        // A single model call can generate several steps; count the call once at
        // its earliest generated step, not once per step or at the session end.
        *earliest = Some(earliest.map_or(timestamp, |old| old.min(timestamp)));
    }
}

fn step_created_at_field(data: &[u8], number: u32) -> Option<Option<i64>> {
    let mut created = None;
    for field in Fields::new(data) {
        let field = field.ok()?;
        if field.number == number {
            created = Some(timestamp(field.value.bytes()?)?);
        }
    }
    Some(created)
}

fn usage_total(data: &[u8]) -> Option<u64> {
    let (mut input, mut output, mut cache_read) = (0_u64, 0_u64, 0_u64);
    for field in Fields::new(data) {
        let field = field.ok()?;
        match field.number {
            2 => input = field.value.varint()?,
            3 => output = field.value.varint()?,
            5 => cache_read = field.value.varint()?,
            _ => {}
        }
    }
    // Native input excludes cache hits (observed cache_read > input); output
    // already equals thinking_output_tokens(9) + response_output_tokens(10).
    input.checked_add(cache_read)?.checked_add(output)
}

/// Borrowed, non-recursive wire decoding, reusable for another native store.
/// Every malformed/truncated field invalidates the message instead of retaining
/// a partially decoded usage count. Unknown length-delimited values stay opaque.
pub(super) struct Fields<'a> {
    remaining: &'a [u8],
}

pub(super) struct Field<'a> {
    pub(super) number: u32,
    pub(super) value: WireValue<'a>,
}

pub(super) enum WireValue<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

impl<'a> WireValue<'a> {
    pub(super) fn bytes(self) -> Option<&'a [u8]> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }

    pub(super) fn varint(self) -> Option<u64> {
        match self {
            Self::Varint(value) => Some(value),
            _ => None,
        }
    }
}

impl<'a> Fields<'a> {
    pub(super) fn new(data: &'a [u8]) -> Self {
        Self { remaining: data }
    }

    fn next_field(&mut self) -> Option<Field<'a>> {
        let tag = take_varint(&mut self.remaining)?;
        let number = u32::try_from(tag >> 3).ok()?;
        if number == 0 || number > 0x1fff_ffff {
            return None;
        }
        let value = match tag & 7 {
            0 => WireValue::Varint(take_varint(&mut self.remaining)?),
            2 => {
                let length = usize::try_from(take_varint(&mut self.remaining)?).ok()?;
                let bytes = self.remaining.get(..length)?;
                self.remaining = &self.remaining[length..];
                WireValue::Bytes(bytes)
            }
            1 | 5 => {
                let length = if tag & 7 == 1 { 8 } else { 4 };
                self.remaining = self.remaining.get(length..)?;
                WireValue::Fixed
            }
            _ => return None,
        };
        Some(Field { number, value })
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = Result<Field<'a>, ()>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        match self.next_field() {
            Some(field) => Some(Ok(field)),
            None => {
                self.remaining = &[];
                Some(Err(()))
            }
        }
    }
}

pub(super) fn take_varint(data: &mut &[u8]) -> Option<u64> {
    let mut value = 0_u64;
    for shift in (0..70).step_by(7) {
        let (&byte, rest) = data.split_first()?;
        *data = rest;
        if shift == 63 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Database {
        directory: std::path::PathBuf,
        path: std::path::PathBuf,
        writer: Option<Connection>,
    }

    impl Database {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let directory = std::env::temp_dir().join(format!(
                "coding-quota-agy-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).unwrap();
            let path = directory.join("conversation.db");
            let writer = Connection::open(&path).unwrap();
            writer
                .execute_batch(
                    "PRAGMA journal_mode=WAL;
                 CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);
                 CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);",
                )
                .unwrap();
            Self {
                directory,
                path,
                writer: Some(writer),
            }
        }

        fn connection(&self) -> &Connection {
            self.writer.as_ref().unwrap()
        }

        fn step(&self, index: i64, seconds: u64) {
            self.connection()
                .execute(
                    "INSERT INTO steps VALUES (?1, ?2)",
                    rusqlite::params![index, bytes(1, &integer(1, seconds))],
                )
                .unwrap();
        }

        fn call(&self, index: i64, data: &[u8]) {
            self.connection()
                .execute(
                    "INSERT INTO gen_metadata VALUES (?1, ?2)",
                    rusqlite::params![index, data],
                )
                .unwrap();
        }

        fn read(&self) -> Option<Vec<UsageEvent>> {
            read_database_at(&self.path, SystemTime::now() + Duration::from_secs(60))
        }
    }

    impl Drop for Database {
        fn drop(&mut self) {
            drop(self.writer.take());
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    fn varint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        while value >= 128 {
            out.push(value as u8 | 128);
            value >>= 7;
        }
        out.push(value as u8);
        out
    }

    fn integer(number: u64, value: u64) -> Vec<u8> {
        [varint(number << 3), varint(value)].concat()
    }

    fn bytes(number: u64, value: &[u8]) -> Vec<u8> {
        [
            varint(number << 3 | 2),
            varint(value.len() as u64),
            value.to_vec(),
        ]
        .concat()
    }

    fn call(indices: &[u64], model: &str, input: u64, cached: u64, output: u64) -> Vec<u8> {
        let usage = [
            integer(2, input),
            integer(3, output),
            integer(5, cached),
            integer(9, output / 2),
            integer(10, output - output / 2),
        ]
        .concat();
        let chat = [bytes(4, &usage), bytes(19, model.as_bytes())].concat();
        let packed: Vec<u8> = indices.iter().flat_map(|index| varint(*index)).collect();
        [bytes(1, &chat), bytes(2, &packed)].concat()
    }

    #[test]
    fn agy_preserves_each_call_model_time_and_nonduplicated_usage() {
        let database = Database::new();
        database.step(1, 1_794_273_599); // Before Beijing midnight.
        database.step(2, 1_794_359_360);
        database.step(3, 1_794_359_365); // Another step from the same call.
        database.call(0, &call(&[1], "claude-sonnet-4-5", 10, 100, 20));
        database.call(1, &call(&[3, 2], "gemini-4-carbon", 2_486, 100_558, 991));
        let events = database.read().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].provider, ProviderId::Antigravity);
        assert_eq!(events[0].model, "claude-sonnet-4-5");
        assert_eq!(events[0].timestamp, 1_794_273_599);
        assert_eq!(events[0].total_tokens, 130);
        assert_eq!(events[1].model, "gemini-4-carbon");
        assert_eq!(events[1].timestamp, 1_794_359_360);
        assert_eq!(events[1].total_tokens, 104_035);
        let rows = super::super::aggregate_at(events, 1_794_359_400);
        assert_eq!(
            rows.iter().find(|row| row.id == "1d").unwrap().total_tokens,
            104_035
        );
        assert_eq!(
            rows.iter()
                .find(|row| row.id == "all")
                .unwrap()
                .total_tokens,
            104_165
        );
    }

    #[test]
    fn agy_includes_new_committed_wal_rows_and_excludes_uncommitted_rows() {
        let database = Database::new();
        database.step(1, 1_794_359_360);
        database.call(0, &call(&[1], "gemini-4-carbon", 10, 20, 30));
        database
            .connection()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        assert_eq!(
            database
                .read()
                .unwrap()
                .iter()
                .map(|event| event.total_tokens)
                .sum::<u64>(),
            60,
        );
        database
            .connection()
            .execute_batch("BEGIN IMMEDIATE")
            .unwrap();
        database.step(2, 1_794_359_400);
        database.call(1, &call(&[2], "claude-sonnet-4-5", 100, 200, 300));
        assert_eq!(
            database
                .read()
                .unwrap()
                .iter()
                .map(|event| event.total_tokens)
                .sum::<u64>(),
            60,
            "Uncommitted usage must not enter the displayed total",
        );
        database.connection().execute_batch("COMMIT").unwrap();
        assert!(
            std::fs::metadata(database.path.with_extension("db-wal"))
                .unwrap()
                .len()
                > 0
        );
        let events = database.read().unwrap();
        assert_eq!(
            events.iter().map(|event| event.total_tokens).sum::<u64>(),
            660
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.model.starts_with("claude"))
                .map(|event| event.total_tokens)
                .sum::<u64>(),
            600,
        );
    }

    #[test]
    fn agy_skips_zero_missing_time_and_damaged_records() {
        let database = Database::new();
        database.step(1, 1_794_359_360);
        database.call(0, &call(&[1], "gemini-4-carbon", 0, 0, 0));
        database.call(1, &call(&[999], "gemini-4-carbon", 1, 2, 3));
        let mut truncated = call(&[1], "gemini-4-carbon", 1, 2, 3);
        truncated.extend_from_slice(&[0x7a, 0x80]);
        database.call(2, &truncated);
        database.call(3, &call(&[1], "gemini-4-carbon", u64::MAX, 1, 0));
        database.call(4, &call(&[1], "gemini-4-carbon", 1, 2, 3));
        let events = database.read().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].total_tokens, 6);
    }

    #[test]
    fn agy_distinguishes_unready_database_from_successful_empty_database() {
        let database = Database::new();
        let created = std::fs::metadata(&database.path)
            .unwrap()
            .created()
            .unwrap_or_else(|_| {
                std::fs::metadata(&database.path)
                    .unwrap()
                    .modified()
                    .unwrap()
            });
        assert!(read_database_at(&database.path, created + Duration::from_secs(2)).is_none());
        assert!(database.read().unwrap().is_empty());
        assert!(
            read_database_at(&database.directory.join("missing.db"), SystemTime::now()).is_none()
        );
    }

    #[test]
    fn agy_wire_decoder_rejects_truncation_overflow_and_invalid_tags() {
        for invalid in [
            &[0][..],
            &[0x0a, 5, 1][..],
            &[0x09, 1][..],
            &[0x0d, 1][..],
            &[
                0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 2,
            ][..],
        ] {
            assert!(Fields::new(invalid).any(|field| field.is_err()));
        }
    }
}
