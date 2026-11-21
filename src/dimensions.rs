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
    parent: usize,
    size: usize,
    minimum: &'a [u8],
}

fn root(nodes: &mut [Node<'_>], mut index: usize) -> usize {
    while nodes[index].parent != index {
        nodes[index].parent = nodes[nodes[index].parent].parent;
        index = nodes[index].parent;
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
                    let index = *ids.entry(people.key(id)).or_insert_with(|| {
                        let index = nodes.len();
                        nodes.push(Node {
                            parent: index,
                            size: 1,
                            minimum: id,
                        });
                        index
                    });
                    root(&mut nodes, index)
                });
                if a == b {
                    continue;
                }
                if nodes[a].size < nodes[b].size {
                    std::mem::swap(&mut a, &mut b);
                }
                nodes[b].parent = a;
                nodes[a].size += nodes[b].size;
                nodes[a].minimum = nodes[a].minimum.min(nodes[b].minimum);
            }
        }

        let canonical: Vec<_> = (0..nodes.len())
            .map(|index| {
                let index = root(&mut nodes, index);
                people.key(nodes[index].minimum)
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
    pub codes: Map<&'a [u8], u32>,
    pub labels: Vec<(&'a str, &'a str)>,
    pub organic: u32,
}

impl<'a> Campaigns<'a> {
    pub fn new(bytes: &'a [u8]) -> io::Result<Self> {
        let rows = dimension_rows(bytes)?;
        let mut labels = Vec::with_capacity(rows.len() + 1);
        let mut codes = Map::default();
        for [id, channel] in rows {
            codes.insert(id, labels.len() as u32);
            labels.push((
                std::str::from_utf8(id).map_err(io::Error::other)?,
                std::str::from_utf8(channel).map_err(io::Error::other)?,
            ));
        }
        let organic = labels.len() as u32;
        labels.push(("organic", "organic"));
        Ok(Self {
            codes,
            labels,
            organic,
        })
    }
}
