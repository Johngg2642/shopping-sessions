use std::collections::HashMap;
use std::io;

use ahash::RandomState;

use crate::input::{Input, data_rows, fields};
use crate::person::{People, PersonKey};

pub type Map<K, V> = HashMap<K, V, RandomState>;

pub struct LinkedIds {
    representatives: Map<PersonKey, PersonKey>,
}

struct Node<'a> {
    // Roots store the negative size of their component. Other nodes store a parent index.
    parent_or_size: isize,
    minimum: (&'a [u8], PersonKey),
}

fn root(nodes: &mut [Node<'_>], mut index: usize) -> usize {
    while nodes[index].parent_or_size >= 0 {
        let parent = nodes[index].parent_or_size as usize;
        let ancestor = nodes[parent].parent_or_size;
        if ancestor < 0 {
            return parent;
        }
        nodes[index].parent_or_size = ancestor;
        index = ancestor as usize;
    }
    index
}

impl LinkedIds {
    pub fn new<'a>(input: &'a Input, people: &People<'a>) -> Self {
        let mut ids = Map::with_capacity_and_hasher(input.bytes() / 24, RandomState::new());
        let mut nodes = Vec::new();
        for chunk in &input.chunks {
            for row in input.rows(chunk) {
                let Some([left, right]) = fields::<2>(row) else {
                    continue;
                };
                if left.is_empty() || right.is_empty() {
                    continue;
                }
                let [mut a, mut b] = [left, right].map(|id| {
                    let key = people.key(id);
                    let index = *ids.entry(key).or_insert_with(|| {
                        let index = nodes.len();
                        nodes.push(Node {
                            parent_or_size: -1,
                            minimum: (id, key),
                        });
                        index
                    });
                    root(&mut nodes, index)
                });
                if a == b {
                    continue;
                }
                if nodes[a].parent_or_size > nodes[b].parent_or_size {
                    std::mem::swap(&mut a, &mut b);
                }
                nodes[a].parent_or_size += nodes[b].parent_or_size;
                nodes[b].parent_or_size = a as isize;
                nodes[a].minimum = nodes[a].minimum.min(nodes[b].minimum);
            }
        }

        let canonical: Vec<_> = (0..nodes.len())
            .map(|index| {
                let index = root(&mut nodes, index);
                nodes[index].minimum.1
            })
            .collect();
        let mut representatives = Map::with_capacity_and_hasher(ids.len(), RandomState::new());
        representatives.extend(ids.into_iter().filter_map(|(key, index)| {
            let person = canonical[index];
            (key != person).then_some((key, person))
        }));
        Self { representatives }
    }

    #[inline]
    pub fn person(&self, key: PersonKey) -> PersonKey {
        self.representatives.get(&key).copied().unwrap_or(key)
    }
}

pub struct Products<'a> {
    codes: Map<&'a [u8], u32>,
    pub categories: Vec<&'a str>,
    unknown: u32,
}

fn dimension_rows(bytes: &[u8]) -> io::Result<Vec<[&[u8]; 2]>> {
    data_rows(bytes)
        .map(|row| fields(row).ok_or_else(|| io::Error::other("malformed dimension row")))
        .collect()
}

impl<'a> Products<'a> {
    pub fn new(bytes: &'a [u8]) -> io::Result<Self> {
        let rows = dimension_rows(bytes)?;
        let mut categories: Vec<_> = rows
            .iter()
            .map(|row| std::str::from_utf8(row[1]))
            .collect::<Result<_, _>>()
            .map_err(io::Error::other)?;
        categories.push("unknown");
        categories.sort_unstable();
        categories.dedup();
        let lookup = |category: &[u8]| {
            categories
                .binary_search_by(|name| name.as_bytes().cmp(category))
                .expect("category indexed") as u32
        };
        let unknown = lookup(b"unknown");
        let codes = rows
            .into_iter()
            .map(|[id, category]| (id, lookup(category)))
            .collect();
        Ok(Self {
            codes,
            categories,
            unknown,
        })
    }

    pub fn category(&self, id: &[u8]) -> u32 {
        self.codes.get(id).copied().unwrap_or(self.unknown)
    }
}

pub struct Campaigns<'a> {
    codes: Map<&'a [u8], u32>,
    numeric: Vec<u32>,
    pub labels: Vec<(&'a str, &'a str)>,
    pub organic: u32,
}

fn numeric_campaign(id: &[u8]) -> Option<usize> {
    let digits = id.strip_prefix(b"c_")?;
    if digits.is_empty() || digits.len() > 4 || (digits.len() > 1 && digits[0] == b'0') {
        return None;
    }
    let mut number = 0;
    for &byte in digits {
        let digit = byte.wrapping_sub(b'0');
        if digit > 9 {
            return None;
        }
        number = number * 10 + usize::from(digit);
    }
    Some(number)
}

impl<'a> Campaigns<'a> {
    pub fn new(bytes: &'a [u8]) -> io::Result<Self> {
        let rows = dimension_rows(bytes)?;
        let mut labels = Vec::with_capacity(rows.len() + 1);
        let mut codes = Map::default();
        let mut numeric = Vec::new();
        for [id, channel] in rows {
            let code = labels.len() as u32;
            if let Some(index) = numeric_campaign(id) {
                if index >= numeric.len() {
                    numeric.resize(index + 1, u32::MAX);
                }
                numeric[index] = code;
            } else {
                codes.insert(id, code);
            }
            labels.push((
                std::str::from_utf8(id).map_err(io::Error::other)?,
                std::str::from_utf8(channel).map_err(io::Error::other)?,
            ));
        }
        let organic = labels.len() as u32;
        labels.push(("organic", "organic"));
        Ok(Self {
            codes,
            numeric,
            labels,
            organic,
        })
    }

    pub fn code(&self, id: &[u8]) -> Option<u32> {
        if let Some(index) = numeric_campaign(id) {
            self.numeric
                .get(index)
                .copied()
                .filter(|&code| code != u32::MAX)
        } else {
            self.codes.get(id).copied()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Campaigns;

    #[test]
    fn numeric_campaign_codes_preserve_exact_spelling() {
        let campaigns =
            Campaigns::new(b"campaign_id,channel\nc_1,email\nc_01,social\nc_12345,video\n")
                .unwrap();
        assert_eq!(campaigns.code(b"c_1"), Some(0));
        assert_eq!(campaigns.code(b"c_01"), Some(1));
        assert_eq!(campaigns.code(b"c_12345"), Some(2));
        assert_eq!(campaigns.code(b"c_2"), None);
        assert_eq!(campaigns.code(b"c_001"), None);
    }
}
