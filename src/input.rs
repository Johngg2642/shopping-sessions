use std::fs::{self, File};
use std::io;
use std::path::Path;

use memchr::{memchr, memchr_iter};
use memmap2::{Mmap, MmapOptions};

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

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position(u64);

impl Position {
    fn new(file: usize, offset: usize) -> Self {
        Self(((file as u64) << OFFSET_BITS) | offset as u64)
    }
}

impl Chunk {
    pub fn rejected<'a>(&self, rejected: &'a [Position]) -> &'a [Position] {
        let start = Position::new(self.file, self.start);
        let end = Position::new(self.file, self.end);
        let first = rejected.partition_point(|&position| position < start);
        let count = rejected[first..].partition_point(|&position| position < end);
        &rejected[first..first + count]
    }
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

    pub fn rows<'a>(&'a self, chunk: &'a Chunk) -> impl Iterator<Item = (Position, &'a [u8])> {
        let mut offset = chunk.start;
        self.files[chunk.file][chunk.start..chunk.end]
            .split_terminator_byte(b'\n')
            .map(move |line| {
                let position = Position::new(chunk.file, offset);
                offset += line.len() + 1;
                (position, line)
            })
    }

    pub fn row(&self, position: Position) -> &[u8] {
        let file = (position.0 >> OFFSET_BITS) as usize;
        let offset = (position.0 & OFFSET_MASK) as usize;
        let bytes = &self.files[file][offset..];
        &bytes[..memchr(b'\n', bytes).unwrap_or(bytes.len())]
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

pub fn fields<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    let mut output = [b"".as_slice(); N];
    let mut start = 0;
    let mut commas = memchr_iter(b',', line);
    for (index, field) in output.iter_mut().enumerate() {
        let end = if index + 1 == N {
            if commas.next().is_some() {
                return None;
            }
            line.len()
        } else {
            commas.next()?
        };
        let mut value = &line[start..end];
        if value.first() == Some(&b'"') {
            value = value.get(1..)?.strip_suffix(b"\"")?;
        }
        if memchr(b'"', value).is_some() {
            return None;
        }
        *field = value;
        start = end + 1;
    }
    Some(output)
}

// Call only for previously validated rows.
pub fn id(line: &[u8]) -> &[u8] {
    let first = &line[..memchr(b',', line).expect("validated CSV")];
    if first.first() == Some(&b'"') {
        &first[1..first.len() - 1]
    } else {
        first
    }
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
    let mut value = 0i64;
    for &byte in bytes {
        let digit = byte.wrapping_sub(b'0');
        if digit > 9 {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(i64::from(digit))?;
    }
    (value <= max).then_some(value)
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
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => 28 + i64::from(year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)),
        _ => return None,
    };
    if day == 0 || day > days_in_month || hour > 23 || minute > 59 || second > 59 {
        return None;
    }

    // Proleptic Gregorian days since 1970-01-01, with the year starting in March.
    let march_year = year - i64::from(month <= 2);
    let era = march_year.div_euclid(400);
    let year_of_era = march_year - era * 400;
    let march_month = if month > 2 { month - 3 } else { month + 9 };
    let days = era * 146_097 + year_of_era * 365 + year_of_era / 4 - year_of_era / 100
        + (153 * march_month + 2) / 5
        + day
        - 1
        - 719_468;
    Some(days * DAY + ((hour * 60 + minute - offset_minutes) * 60 + second) * 1_000)
}
