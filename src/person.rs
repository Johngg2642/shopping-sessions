use std::str::Utf8Error;
use std::sync::Mutex;

use crate::dimensions::Map;

const INTERNED: u64 = 1 << 63;

pub fn inline_ascii(name: &[u8]) -> Option<u64> {
    if name.len() > 9 {
        return None;
    }
    let mut bits = 0;
    for &byte in name {
        if byte == 0 || byte >= 128 {
            return None;
        }
        bits = (bits << 7) | u64::from(byte);
    }
    Some(bits)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PersonKey(u64);

#[derive(Default)]
pub struct People<'a>(Mutex<Interned<'a>>);

#[derive(Default)]
struct Interned<'a> {
    codes: Map<&'a [u8], PersonKey>,
    names: Vec<&'a [u8]>,
}

impl<'a> People<'a> {
    pub fn key(&self, name: &'a [u8]) -> PersonKey {
        // Nine nonzero ASCII bytes fit in 63 bits. Encode them without a
        // string lookup during sort, and intern all other UTF-8 names exactly.
        inline_ascii(name).map_or_else(|| self.intern(name), PersonKey)
    }

    fn intern(&self, name: &'a [u8]) -> PersonKey {
        let mut storage = self.0.lock().expect("person dictionary lock");
        let Interned { codes, names } = &mut *storage;
        *codes.entry(name).or_insert_with(|| {
            let key = PersonKey(INTERNED | names.len() as u64);
            names.push(name);
            key
        })
    }

    pub fn into_names(self) -> Vec<&'a [u8]> {
        self.0.into_inner().expect("person dictionary lock").names
    }
}

impl PersonKey {
    pub fn name<'a>(
        self,
        interned: &'a [&'a [u8]],
        buffer: &'a mut [u8; 9],
    ) -> Result<&'a str, Utf8Error> {
        if self.0 & INTERNED != 0 {
            return std::str::from_utf8(interned[(self.0 ^ INTERNED) as usize]);
        }
        let mut bits = self.0;
        let mut start = buffer.len();
        while bits != 0 {
            start -= 1;
            buffer[start] = (bits & 127) as u8;
            bits >>= 7;
        }
        std::str::from_utf8(&buffer[start..])
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::People;

    #[test]
    fn short_and_interned_names_round_trip_without_aliasing() {
        let people = People::default();
        let names: &[&[u8]] = &[
            b"ab",
            b"a\0b",
            b"abcdefghi",
            b"abcdefghj",
            b"abcdefghij",
            b"abcdefghik",
            "aé".as_bytes(),
        ];
        let keys: Vec<_> = names.iter().map(|&name| people.key(name)).collect();
        assert_eq!(
            keys.iter().copied().collect::<HashSet<_>>().len(),
            names.len()
        );
        assert!(keys[4] == people.key(b"abcdefghij"));
        let interned = people.into_names();
        let mut buffer = [0; 9];
        for (&key, &name) in keys.iter().zip(names) {
            assert_eq!(key.name(&interned, &mut buffer).unwrap().as_bytes(), name);
        }
    }
}
