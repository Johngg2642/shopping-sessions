// Keep the mask path limited to short records so field positions fit in a u128.
pub fn fields<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    #[cfg(any(
        target_arch = "aarch64",
        all(target_arch = "x86_64", target_feature = "avx2")
    ))]
    if (WIDTH..=128).contains(&line.len()) {
        return fields_from_masks(line);
    }
    fields_scalar(line)
}

fn fields_scalar<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    let mut output = [b"".as_slice(); N];
    let mut start = 0;
    let mut commas = memchr::memchr_iter(b',', line);
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
        if let Some(quoted) = value.strip_prefix(b"\"") {
            value = quoted.strip_suffix(b"\"")?;
        }
        if memchr::memchr(b'"', value).is_some() {
            return None;
        }
        *field = value;
        start = end + 1;
    }
    Some(output)
}

#[cfg(any(
    target_arch = "aarch64",
    all(target_arch = "x86_64", target_feature = "avx2")
))]
fn fields_from_masks<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    let (chunks, tail) = line.as_chunks::<WIDTH>();
    let mut commas = 0u128;
    let mut quotes = 0u128;
    for (index, chunk) in chunks.iter().enumerate() {
        let (separators, marks) = masks(chunk);
        commas |= u128::from(separators) << (index * WIDTH);
        quotes |= u128::from(marks) << (index * WIDTH);
    }
    if !tail.is_empty() {
        // Read an overlapping full vector ending at the last byte of the row.
        let offset = line.len() - WIDTH;
        let (separators, marks) = masks(line.last_chunk().expect("one full vector"));
        commas |= u128::from(separators) << offset;
        quotes |= u128::from(marks) << offset;
    }
    parse_masks(line, commas, quotes)
}

fn parse_masks<const N: usize>(
    line: &[u8],
    mut commas: u128,
    mut quotes: u128,
) -> Option<[&[u8]; N]> {
    if commas.count_ones() as usize + 1 != N {
        return None;
    }

    let mut output = [b"".as_slice(); N];
    let mut start = 0;
    for (index, field) in output.iter_mut().enumerate() {
        let end = if index + 1 < N {
            let comma = commas.trailing_zeros() as usize;
            commas &= commas - 1;
            comma
        } else {
            line.len()
        };
        let opening = 1u128.wrapping_shl(start as u32);
        let closing = 1u128.wrapping_shl(end.wrapping_sub(1) as u32);
        let quoted = ((start < 128) & (quotes & opening != 0)) as usize;
        quotes ^= (opening | closing) * quoted as u128;
        *field = line.get(start + quoted..end.wrapping_sub(quoted))?;
        start = end + 1;
    }
    // Every quote must be part of a matched opening and closing pair.
    (quotes == 0).then_some(output)
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
const WIDTH: usize = 32;

#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline]
fn masks(chunk: &[u8; WIDTH]) -> (u32, u32) {
    use std::arch::x86_64::*;
    // The slice is exactly one vector wide and loadu does not require alignment.
    unsafe {
        let bytes = _mm256_loadu_si256(chunk.as_ptr().cast());
        let mask =
            |byte| _mm256_movemask_epi8(_mm256_cmpeq_epi8(bytes, _mm256_set1_epi8(byte))) as u32;
        (mask(b',' as i8), mask(b'"' as i8))
    }
}

#[cfg(target_arch = "aarch64")]
const WIDTH: usize = 16;

#[cfg(target_arch = "aarch64")]
#[inline]
fn masks(chunk: &[u8; WIDTH]) -> (u32, u32) {
    use std::arch::aarch64::*;
    const WEIGHTS: [u8; WIDTH] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
    // AArch64 has NEON and both vectors are exactly one load wide.
    unsafe {
        let bytes = vld1q_u8(chunk.as_ptr());
        let weights = vld1q_u8(WEIGHTS.as_ptr());
        let mask = |byte| {
            let weighted = vandq_u8(vceqq_u8(bytes, vdupq_n_u8(byte)), weights);
            u32::from(vaddv_u8(vget_low_u8(weighted)))
                | (u32::from(vaddv_u8(vget_high_u8(weighted))) << 8)
        };
        (mask(b','), mask(b'"'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_tail_and_quote_rules_match_scalar_parser() {
        for len in 0..140 {
            let padding = "x".repeat(len);
            for row in [
                format!("\"{padding}\",,\"ok\""),
                format!("\"{padding}\",,\"unclosed"),
                format!("{padding}\"stray\",,\"ok\""),
                format!("{padding},,\"end\"garbage"),
                format!("{padding},x,,extra"),
                format!("{padding},,\"interior\"quote\""),
                format!("{padding},,\"cr\"\r"),
                format!("{padding},,\r"),
                format!("{padding},,"),
            ] {
                assert_eq!(
                    fields::<3>(row.as_bytes()),
                    fields_scalar::<3>(row.as_bytes()),
                    "{row:?}"
                );
            }
        }
    }

    #[test]
    fn chunk_scanner_matches_scalar_across_block_boundaries() {
        let mut bytes = Vec::new();
        for len in 0..150 {
            let value = "x".repeat(len);
            bytes.extend_from_slice(format!("\"{value}\",,ok\n{value},x,,extra\n").as_bytes());
            bytes.extend_from_slice(format!("{value},,\"bad\"\r\n{value},x,\n").as_bytes());
        }
        bytes.extend_from_slice(b"last,unfinished,row");

        let expected: Vec<_> = bytes
            .split(|&byte| byte == b'\n')
            .filter_map(fields_scalar::<3>)
            .collect();
        let mut actual = Vec::new();
        records::<3>(&bytes, |row| actual.push(row));
        assert_eq!(actual, expected);
    }
}

#[derive(Clone, Copy, Default)]
struct Block {
    newlines: u64,
    commas: u64,
    quotes: u64,
}

pub fn records<'a, const N: usize>(bytes: &'a [u8], mut visit: impl FnMut([&'a [u8]; N])) {
    #[cfg(target_arch = "x86_64")]
    {
        static AVX512: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
            std::env::var_os("CSV_SIMD").as_deref() != Some(std::ffi::OsStr::new("avx2"))
                && std::arch::is_x86_feature_detected!("avx512bw")
        });
        if *AVX512 {
            // The feature check covers the entire scan of this slice.
            return unsafe { records_avx512(bytes, visit) };
        }
    }
    #[cfg(any(
        target_arch = "aarch64",
        all(target_arch = "x86_64", target_feature = "avx2")
    ))]
    if bytes.len() >= 256 {
        return records_block(bytes, block_native, visit);
    }
    for line in bytes.split(|&b| b == b'\n') {
        if let Some(values) = fields(line) {
            visit(values);
        }
    }
}

#[cfg_attr(target_arch = "x86_64", inline(always))]
#[cfg_attr(not(target_arch = "x86_64"), inline)]
fn records_block<'a, const N: usize>(
    bytes: &'a [u8],
    classify: impl Fn(&[u8; 64]) -> Block,
    mut visit: impl FnMut([&'a [u8]; N]),
) {
    let mut start = 0;
    let mut commas = 0;
    let mut quotes = 0;
    for (index, block) in bytes.as_chunks::<64>().0.iter().enumerate() {
        let base = index * 64;
        let end_block = base + 64;
        let Block {
            mut newlines,
            commas: sep,
            quotes: mark,
        } = classify(block);
        while newlines != 0 {
            let end = base + newlines.trailing_zeros() as usize;
            newlines &= newlines - 1;
            let line = &bytes[start..end];
            if line.len() <= 128 && !line.is_empty() {
                let mask = u128::MAX >> (128 - line.len());
                let offset = base.saturating_sub(start);
                if offset < 128 {
                    let skip = start.saturating_sub(base);
                    commas |= u128::from(sep >> skip) << offset;
                    quotes |= u128::from(mark >> skip) << offset;
                }
                if let Some(values) = parse_masks(line, commas & mask, quotes & mask) {
                    visit(values);
                }
            } else if line.len() > 128 {
                if let Some(values) = fields_scalar(line) {
                    visit(values);
                }
            }
            start = end + 1;
            commas = 0;
            quotes = 0;
        }
        if start < end_block && end_block - start <= 128 {
            let offset = base.saturating_sub(start);
            let skip = start.saturating_sub(base);
            commas |= u128::from(sep >> skip) << offset;
            quotes |= u128::from(mark >> skip) << offset;
        }
    }
    for line in bytes[start..].split(|&b| b == b'\n') {
        if let Some(values) = fields(line) {
            visit(values);
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512bw")]
fn records_avx512<'a, const N: usize>(bytes: &'a [u8], visit: impl FnMut([&'a [u8]; N])) {
    use std::arch::x86_64::*;
    let newline = _mm512_set1_epi8(b'\n' as i8);
    let comma = _mm512_set1_epi8(b',' as i8);
    let quote = _mm512_set1_epi8(b'"' as i8);
    records_block(
        bytes,
        |block| {
            // The full block is readable and the load allows an unaligned address.
            let data = unsafe { _mm512_loadu_si512(block.as_ptr().cast()) };
            Block {
                newlines: _mm512_cmpeq_epi8_mask(data, newline),
                commas: _mm512_cmpeq_epi8_mask(data, comma),
                quotes: _mm512_cmpeq_epi8_mask(data, quote),
            }
        },
        visit,
    );
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline]
fn block_native(bytes: &[u8; 64]) -> Block {
    use std::arch::x86_64::*;
    // Both loads stay within the full block and allow unaligned addresses.
    unsafe {
        let lo = _mm256_loadu_si256(bytes.as_ptr().cast());
        let hi = _mm256_loadu_si256(bytes.as_ptr().add(32).cast());
        let mask = |byte| {
            let byte = _mm256_set1_epi8(byte);
            let lo = _mm256_movemask_epi8(_mm256_cmpeq_epi8(lo, byte)) as u32;
            let hi = _mm256_movemask_epi8(_mm256_cmpeq_epi8(hi, byte)) as u32;
            u64::from(lo) | (u64::from(hi) << 32)
        };
        Block {
            newlines: mask(b'\n' as i8),
            commas: mask(b',' as i8),
            quotes: mask(b'"' as i8),
        }
    }
}

#[cfg(target_arch = "aarch64")]
#[inline]
fn block_native(bytes: &[u8; 64]) -> Block {
    use std::arch::aarch64::*;
    const WEIGHTS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
    let mut masks = Block::default();
    // All vectors are fully inside their arrays and NEON is baseline on AArch64.
    unsafe {
        let weights = vld1q_u8(WEIGHTS.as_ptr());
        for (i, vector) in bytes.as_chunks::<16>().0.iter().enumerate() {
            let value = vld1q_u8(vector.as_ptr());
            let mask = |byte| {
                let bits = vandq_u8(vceqq_u8(value, vdupq_n_u8(byte)), weights);
                let lo = vaddv_u8(vget_low_u8(bits));
                let hi = vaddv_u8(vget_high_u8(bits));
                (u64::from(lo) | (u64::from(hi) << 8)) << (i * 16)
            };
            masks.newlines |= mask(b'\n');
            masks.commas |= mask(b',');
            masks.quotes |= mask(b'"');
        }
    }
    masks
}
