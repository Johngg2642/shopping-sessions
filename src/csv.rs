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
        *field = if start < end && (quotes >> start) & 1 != 0 {
            if end - start < 2 {
                return None;
            }
            quotes ^= (1 << start) | (1 << (end - 1));
            &line[start + 1..end - 1]
        } else {
            &line[start..end]
        };
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
}
