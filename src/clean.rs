use std::ops::Range;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use ahash::RandomState;
use hashbrown::HashTable;
use hashbrown::hash_table::Entry;
use rayon::prelude::*;

use crate::dimensions::{Campaigns, LinkedIds, Products};
use crate::input::{DAY, Input, decimal, fields, first_id, ingest_millis, timestamp};

use crate::person::{People, PersonKey, inline_ascii};

const SHARDS: usize = 128;

#[derive(Clone, Copy)]
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
struct Candidate<'a> {
    key: u64,
    long_id: Option<&'a [u8]>,
    priority: i64,
    event: Option<Event>,
}

pub struct Repeats(Vec<AtomicU64>);

impl Repeats {
    pub fn new(input: &Input, hashes: &RandomState) -> Self {
        // Each bucket has a seen bit and a repeated bit. Byte size only
        // determines the collision rate; collisions cause extra comparisons.
        let words = (input.bytes() / 384)
            .next_power_of_two()
            .clamp(1024, 1 << 24);
        let repeats = Self((0..words).map(|_| AtomicU64::new(0)).collect());
        input.chunks.par_iter().for_each(|chunk| {
            for row in input.rows(chunk) {
                if let Some(id) = first_id(row) {
                    repeats.observe(hashes.hash_one(id));
                }
            }
        });
        repeats
    }

    #[inline]
    fn bucket(&self, hash: u64) -> (&AtomicU64, u64) {
        let word = (hash >> 5) as usize & (self.0.len() - 1);
        (&self.0[word], 1 << (hash & 31))
    }

    fn observe(&self, hash: u64) {
        let (word, bit) = self.bucket(hash);
        if word.fetch_or(bit, Relaxed) & bit != 0 {
            word.fetch_or(bit << 32, Relaxed);
        }
    }

    fn might_repeat(&self, hash: u64) -> bool {
        let (word, bit) = self.bucket(hash);
        word.load(Relaxed) & (bit << 32) != 0
    }
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

fn event<'a>(
    [id, user, time, _, kind, product, price]: [&'a [u8]; 7],
    links: &LinkedIds,
    products: &Products<'_>,
    identities: &People<'a>,
    window: &Range<i64>,
    ingest: i64,
) -> Option<Event> {
    if id.is_empty() || user.is_empty() || product.is_empty() {
        return None;
    }
    let time = timestamp(time)?;
    if !window.contains(&time) || ingest - time > DAY {
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
    let person = links.person(identities.key(user));
    Some(Event {
        person,
        time,
        price,
        category,
    })
}

pub fn events<'a>(
    input: &'a Input,
    links: &LinkedIds,
    products: &Products<'_>,
    identities: &People<'a>,
    window: Range<i64>,
    hashes: &RandomState,
    repeats: &Repeats,
) -> [Vec<Event>; SHARDS] {
    let people = Shards::new();
    let candidates = Shards::new();
    input.chunks.par_iter().for_each_init(
        || {
            (
                std::array::from_fn(|_| Vec::new()),
                std::array::from_fn(|_| Vec::new()),
            )
        },
        |(users, suspects), chunk| {
            for row in input.rows(chunk) {
                let Some(values) = fields::<7>(row) else {
                    continue;
                };
                let [id, ..] = values;
                if id.is_empty() {
                    continue;
                }
                let hash = hashes.hash_one(id);
                let ingest = ingest_millis(values[3]);
                let event = event(values, links, products, identities, &window, ingest);
                if repeats.might_repeat(hash) {
                    let (key, long_id) = match inline_ascii(id) {
                        Some(key) => (key, None),
                        None => (hash, Some(id)),
                    };
                    suspects[shard(hash)].push(Candidate {
                        key,
                        long_id,
                        priority: ingest,
                        event,
                    });
                } else if let Some(event) = event {
                    users[shard(hashes.hash_one(event.person))].push(event);
                }
            }
            people.append(users);
            candidates.append(suspects);
        },
    );

    candidates.into_vecs().into_par_iter().for_each_init(
        || std::array::from_fn(|_| Vec::new()),
        |users, candidates| {
            let mut unique = HashTable::<Candidate<'_>>::with_capacity(candidates.len());
            for candidate in candidates {
                match unique.entry(
                    hashes.hash_one(candidate.key),
                    |existing| {
                        existing.key == candidate.key && existing.long_id == candidate.long_id
                    },
                    |existing| hashes.hash_one(existing.key),
                ) {
                    Entry::Vacant(slot) => {
                        slot.insert(candidate);
                    }
                    Entry::Occupied(mut slot) if candidate.priority < slot.get().priority => {
                        *slot.get_mut() = candidate;
                    }
                    Entry::Occupied(_) => {}
                }
            }
            for winner in unique.into_iter().filter_map(|candidate| candidate.event) {
                users[shard(hashes.hash_one(winner.person))].push(winner);
            }
            people.append(users);
        },
    );
    people.into_vecs()
}

pub fn clicks<'a>(
    input: &'a Input,
    links: &LinkedIds,
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
            for row in input.rows(chunk) {
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
                let person = links.person(identities.key(user));
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
