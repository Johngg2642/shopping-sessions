use std::ops::Range;
use std::sync::Mutex;

use ahash::RandomState;
use hashbrown::HashTable;
use hashbrown::hash_table::Entry;
use rayon::prelude::*;

use crate::dimensions::{Campaigns, LinkedIds, Products};
use crate::input::{DAY, Input, Position, decimal, fields, id, timestamp};

use crate::person::{People, PersonKey, inline_ascii};

const SHARDS: usize = 128;
const HASHED_ID: u64 = 1 << 63;

pub struct Event {
    pub person: PersonKey,
    pub time: i64,
    pub price: u32,
    pub category: u32,
}

pub struct Click<'a> {
    pub person: PersonKey,
    pub time: i64,
    pub id: &'a [u8],
    pub campaign: u32,
}

#[derive(Clone, Copy)]
struct Candidate {
    key: u64,
    position: Position,
    priority: u64,
}

struct Shards<T>([Mutex<Vec<T>>; SHARDS]);

impl<T> Shards<T> {
    fn new() -> Self {
        Self(std::array::from_fn(|_| Mutex::new(Vec::new())))
    }

    fn append(&self, local: &mut [Vec<T>; SHARDS]) {
        for (shared, rows) in self.0.iter().zip(local) {
            if !rows.is_empty() {
                shared.lock().expect("shard lock").append(rows);
            }
        }
    }

    fn into_vecs(self) -> [Vec<T>; SHARDS] {
        self.0.map(|shard| shard.into_inner().expect("shard lock"))
    }
}

fn shard(hash: u64) -> usize {
    // Reserve the low bits for the local hash table's bucket index.
    (hash >> 32) as usize & (SHARDS - 1)
}

fn ingest_priority(timestamp: &[u8]) -> u64 {
    // Ingest is always a valid, fixed-width UTC timestamp. Pack successive
    // digit pairs to keep byte order chronological without touching the CSV
    // again when choosing a winner later.
    let timestamp: &[u8; 20] = timestamp.try_into().expect("fixed-width ingest timestamp");
    let pair = |i: usize| (timestamp[i] - b'0') * 10 + (timestamp[i + 1] - b'0');
    u64::from_be_bytes([
        pair(0),
        pair(2),
        pair(5),
        pair(8),
        pair(11),
        pair(14),
        pair(17),
        0,
    ])
}

fn duplicate_events(input: &Input, hashes: &RandomState) -> Vec<Position> {
    let candidates = Shards::new();
    input.chunks.par_iter().for_each_init(
        || std::array::from_fn(|_| Vec::new()),
        |local, chunk| {
            for (position, row) in input.rows(chunk) {
                let Some(values) = fields::<7>(row) else {
                    continue;
                };
                if values[0].is_empty() {
                    continue;
                }
                let key = inline_ascii(values[0])
                    .unwrap_or_else(|| hashes.hash_one(values[0]) | HASHED_ID);
                local[shard(hashes.hash_one(key))].push(Candidate {
                    key,
                    position,
                    priority: ingest_priority(values[3]),
                });
            }
            candidates.append(local);
        },
    );

    // Keep only losing positions. Winners can then be parsed in file order,
    // avoiding a random read of the mapped data for every surviving row.
    let mut rejected: Vec<_> = candidates
        .into_vecs()
        .into_par_iter()
        .flat_map_iter(|candidates| {
            let mut rejected = Vec::new();
            let mut unique = HashTable::<Candidate>::with_capacity(candidates.len());
            for candidate in candidates {
                let mut first_id = None;
                let entry = unique.entry(
                    hashes.hash_one(candidate.key),
                    |existing| {
                        existing.key == candidate.key
                            && (candidate.key & HASHED_ID == 0
                                || id(input.row(existing.position))
                                    == *first_id
                                        .get_or_insert_with(|| id(input.row(candidate.position))))
                    },
                    |existing| hashes.hash_one(existing.key),
                );
                match entry {
                    Entry::Vacant(entry) => {
                        entry.insert(candidate);
                    }
                    Entry::Occupied(mut entry) => {
                        if candidate.priority < entry.get().priority {
                            rejected.push(entry.get().position);
                            *entry.get_mut() = candidate;
                        } else {
                            rejected.push(candidate.position);
                        }
                    }
                }
            }
            rejected
        })
        .collect();
    rejected.par_sort_unstable();

    rejected
}

fn event<'a>(
    [id, user, time, ingest, kind, product, price]: [&'a [u8]; 7],
    links: &LinkedIds<'a>,
    products: &Products<'_>,
    identities: &People<'a>,
    window: &Range<i64>,
) -> Option<Event> {
    if id.is_empty() || user.is_empty() || product.is_empty() {
        return None;
    }
    let time = timestamp(time)?;
    if !window.contains(&time) || timestamp(ingest)? - time > DAY {
        return None;
    }
    let (price, category) = match kind {
        b"view" | b"cart" => (0, 0),
        b"purchase" => {
            let price = decimal(price, 10_000_000)? as u32;
            if price == 0 {
                return None;
            }
            (price, products.category(product))
        }
        _ => return None,
    };
    let person = identities.key(links.person(user));
    Some(Event {
        person,
        time,
        price,
        category,
    })
}

pub fn events<'a>(
    input: &'a Input,
    links: &LinkedIds<'a>,
    products: &Products<'_>,
    identities: &People<'a>,
    window: Range<i64>,
    hashes: &RandomState,
) -> [Vec<Event>; SHARDS] {
    let rejected = duplicate_events(input, hashes);
    let people = Shards::new();
    input.chunks.par_iter().for_each_init(
        || std::array::from_fn(|_| Vec::new()),
        |local, chunk| {
            let mut rejected = chunk.rejected(&rejected).iter().copied().peekable();
            for (position, row) in input.rows(chunk) {
                if rejected.next_if_eq(&position).is_some() {
                    continue;
                }
                let Some(values) = fields(row) else { continue };
                if let Some(event) = event(values, links, products, identities, &window) {
                    local[shard(hashes.hash_one(event.person))].push(event);
                }
            }
            people.append(local);
        },
    );
    people.into_vecs()
}

pub fn clicks<'a>(
    input: &'a Input,
    links: &LinkedIds<'a>,
    campaigns: &Campaigns<'_>,
    identities: &People<'a>,
    window: Range<i64>,
    hashes: &RandomState,
) -> [Vec<Click<'a>>; SHARDS] {
    // Click copies are identical after parsing. Validation and partitioning
    // keep them together, and the attribution sort makes them adjacent.
    let people = Shards::new();
    input.chunks.par_iter().for_each_init(
        || std::array::from_fn(|_| Vec::new()),
        |local, chunk| {
            for (_, row) in input.rows(chunk) {
                let Some([id, user, campaign, time]) = fields(row) else {
                    continue;
                };
                if id.is_empty() || user.is_empty() {
                    continue;
                }
                let Some(time) = timestamp(time).filter(|time| window.contains(time)) else {
                    continue;
                };
                let Some(&campaign) = campaigns.codes.get(campaign) else {
                    continue;
                };
                let person = identities.key(links.person(user));
                local[shard(hashes.hash_one(person))].push(Click {
                    person,
                    time,
                    id,
                    campaign,
                });
            }
            people.append(local);
        },
    );
    people.into_vecs()
}
