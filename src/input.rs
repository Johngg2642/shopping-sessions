use std::fs::{self, File};
use std::io;
use std::path::Path;

use memchr::{memchr, memchr_iter};
use memmap2::{Mmap, MmapOptions};

pub use crate::csv::fields;

pub const DAY: i64 = 86_400_000;
pub const WEEK: i64 = 7 * DAY;
const CHUNK_BYTES: usize = 2 * 1024 * 1024;
const OFFSET_BITS: u32 = 40;
const OFFSET_MASK: u64 = (1 << OFFSET_BITS) - 1;

pub fn map_file(path: &Path) -> io::Result<Option<Mmap>> {
    let file = File::open(path)?;
    if file.metadata()?.len() == 0 {
        return Ok(None);
    }
    // Inputs are immutable for the lifetime of the process (read-only scorer mount).
    unsafe { MmapOptions::new().map(&file).map(Some) }
}

pub struct Input {
    files: Vec<Mmap>,
    pub chunks: Vec<Chunk>,
}

pub struct Chunk {
    file: usize,
    start: usize,
    end: usize,
}

impl Input {
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut files = Vec::new();
        let mut chunks = Vec::new();
        for entry in fs::read_dir(path)? {
            let path = entry?.path();
            if path.extension().is_none_or(|ext| ext != "csv") {
                continue;
            }
            let Some(bytes) = map_file(&path)? else {
                continue;
            };
            let file = files.len();
            if bytes.len() as u64 > OFFSET_MASK || file as u64 > (u64::MAX >> OFFSET_BITS) {
                return Err(io::Error::other(format!(
                    "input too large: {}",
                    path.display()
                )));
            }

            let mut start = after_line(&bytes, 0);
            while start < bytes.len() {
                let end = after_line(&bytes, (start + CHUNK_BYTES).min(bytes.len()));
                chunks.push(Chunk { file, start, end });
                start = end;
            }
            files.push(bytes);
        }
        Ok(Self { files, chunks })
    }

    pub fn rows<'a>(&'a self, chunk: &'a Chunk) -> impl Iterator<Item = &'a [u8]> {
        self.files[chunk.file][chunk.start..chunk.end].split_terminator_byte(b'\n')
    }

    pub fn bytes(&self) -> usize {
        self.files.iter().map(|file| file.len()).sum()
    }
}

fn after_line(bytes: &[u8], start: usize) -> usize {
    memchr(b'\n', &bytes[start..]).map_or(bytes.len(), |at| start + at + 1)
}

pub fn data_rows(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes[after_line(bytes, 0)..].split_terminator_byte(b'\n')
}

trait SplitByte {
    fn split_terminator_byte(&self, needle: u8) -> impl Iterator<Item = &[u8]>;
}

impl SplitByte for [u8] {
    fn split_terminator_byte(&self, needle: u8) -> impl Iterator<Item = &[u8]> {
        let ends = memchr_iter(needle, self);
        let mut start = 0;
        ends.chain(std::iter::once(self.len()))
            .filter_map(move |end| {
                let line = self.get(start..end)?;
                start = end + 1;
                (end < self.len() || !line.is_empty()).then_some(line)
            })
    }
}

// A malformed row can add a false positive here. Valid IDs use the same
// unquoted bytes during the full parse, so no actual repeat is missed.
pub fn first_id(line: &[u8]) -> Option<&[u8]> {
    let first = &line[..memchr(b',', line)?];
    let id = if let Some(quoted) = first.strip_prefix(b"\"") {
        quoted.strip_suffix(b"\"")?
    } else {
        first
    };
    (!id.is_empty()).then_some(id)
}

pub fn decimal(mut bytes: &[u8], max: i64) -> Option<i64> {
    if bytes.is_empty() {
        return None;
    }
    while bytes.first() == Some(&b'0') {
        bytes = &bytes[1..];
    }
    if bytes.len() > 19 {
        return None;
    }
    let mut value = 0u64;
    if let Some(prefix) = bytes.first_chunk::<8>() {
        value = decimal_eight(prefix)?;
        bytes = &bytes[8..];
    }
    for &byte in bytes {
        let digit = byte.wrapping_sub(b'0');
        if digit > 9 {
            return None;
        }
        value = value * 10 + u64::from(digit);
    }
    (value <= max as u64).then_some(value as i64)
}

#[inline]
fn decimal_eight(bytes: &[u8; 8]) -> Option<u64> {
    const HIGH: u64 = 0xf0f0_f0f0_f0f0_f0f0;
    const ASCII: u64 = 0x3030_3030_3030_3030;
    let bytes = u64::from_le_bytes(*bytes);
    if bytes & HIGH != ASCII || (bytes + 0x0606_0606_0606_0606) & HIGH != ASCII {
        return None;
    }
    // Reduce eight validated ASCII digits to four pairs, two quads, then one value.
    let digits = bytes & !HIGH;
    let pairs = (digits.wrapping_mul(10) + (digits >> 8)) & 0x00ff_00ff_00ff_00ff;
    let quads = (pairs.wrapping_mul(100) + (pairs >> 16)) & 0x0000_ffff_0000_ffff;
    Some((quads.wrapping_mul(10_000) + (quads >> 32)) & 0xffff_ffff)
}

fn pair(bytes: &[u8], start: usize) -> Option<i64> {
    let a = bytes[start].wrapping_sub(b'0');
    let b = bytes[start + 1].wrapping_sub(b'0');
    (a <= 9 && b <= 9).then_some(i64::from(a) * 10 + i64::from(b))
}

pub fn timestamp(bytes: &[u8]) -> Option<i64> {
    // Inspect the date separator first. Epoch milliseconds can have any length.
    if bytes.get(4) != Some(&b'-') {
        return decimal(bytes, i64::MAX);
    }
    let offset_minutes = match bytes.len() {
        20 if bytes[19] == b'Z' => 0,
        25 if bytes[22] == b':' => {
            let sign = match bytes[19] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (hours, minutes) = (pair(bytes, 20)?, pair(bytes, 23)?);
            if hours > 23 || minutes > 59 {
                return None;
            }
            sign * (hours * 60 + minutes)
        }
        _ => return None,
    };
    if bytes[7] != b'-' || bytes[10] != b'T' || bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let year = pair(bytes, 0)? * 100 + pair(bytes, 2)?;
    let (month, day) = (pair(bytes, 5)?, pair(bytes, 8)?);
    let (hour, minute, second) = (pair(bytes, 11)?, pair(bytes, 14)?, pair(bytes, 17)?);
    if !(1..=12).contains(&month) || day == 0 || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let entry = calendar_month(year, month);
    if day > i64::from(entry & 31) {
        return None;
    }
    let days = i64::from(entry >> 5) + day - 1;
    Some(days * DAY + ((hour * 60 + minute - offset_minutes) * 60 + second) * 1_000)
}

pub fn ingest_millis(bytes: &[u8]) -> i64 {
    // The spec guarantees every ingest field is a valid fixed-width UTC timestamp.
    let bytes: &[u8; 20] = bytes.try_into().expect("fixed-width ingest timestamp");
    let pair = |i: usize| i64::from(bytes[i] - b'0') * 10 + i64::from(bytes[i + 1] - b'0');
    let month = calendar_month(pair(0) * 100 + pair(2), pair(5));
    let days = i64::from(month >> 5) + pair(8) - 1;
    days * DAY + ((pair(11) * 60 + pair(14)) * 60 + pair(17)) * 1_000
}

const FIRST_CACHED_YEAR: i64 = 1970;
const CACHED_YEARS: usize = 130;
const CACHED_MONTHS: usize = CACHED_YEARS * 12;

static MONTHS: [i32; CACHED_MONTHS] = {
    let mut months = [0; CACHED_MONTHS];
    let mut index = 0;
    while index < months.len() {
        months[index] = build_month(
            FIRST_CACHED_YEAR + (index / 12) as i64,
            (index % 12 + 1) as i64,
        );
        index += 1;
    }
    months
};

fn calendar_month(year: i64, month: i64) -> i32 {
    let index = ((year - FIRST_CACHED_YEAR) * 12 + month - 1) as usize;
    MONTHS
        .get(index)
        .copied()
        .unwrap_or_else(|| build_month(year, month))
}

const fn build_month(year: i64, month: i64) -> i32 {
    let first = civil_day(year, month, 1);
    let next = if month == 12 {
        civil_day(year + 1, 1, 1)
    } else {
        civil_day(year, month + 1, 1)
    };
    (first as i32) << 5 | (next - first) as i32
}

const fn civil_day(year: i64, month: i64, day: i64) -> i64 {
    // Proleptic Gregorian days since 1970-01-01, with the year starting in March.
    let march_year = year - (month <= 2) as i64;
    let era = march_year.div_euclid(400);
    let year_of_era = march_year - era * 400;
    let march_month = if month > 2 { month - 3 } else { month + 9 };
    era * 146_097 + year_of_era * 365 + year_of_era / 4 - year_of_era / 100
        + (153 * march_month + 2) / 5
        + day
        - 1
        - 719_468
}
