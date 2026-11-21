use std::ops::Range;
use std::sync::Mutex;

use ahash::RandomState;
use hashbrown::HashTable;
use hashbrown::hash_table::Entry;
use rayon::prelude::*;

use crate::csv::records;
use crate::dimensions::{Campaigns, LinkedIds, Products};
use crate::input::{DAY, Input, decimal, ingest_millis, timestamp};

use crate::person::{People, PersonKey, inline_ascii};

const SHARDS: usize = 128;

const EXTENDED_EVENT: u64 = 255 << 56;

#[derive(Clone, Copy)]
pub struct Event {
    pub person: PersonKey,
    payload: u64,
}

#[derive(Clone, Copy)]
pub struct EventDetails {
    pub time: i64,
    pub price: u32,
    pub category: u32,
}

pub struct EventEncoder {
    origin: i64,
    extended: Mutex<Vec<EventDetails>>,
}

pub struct EventDecoder {
    origin: i64,
    extended: Vec<EventDetails>,
}

impl EventEncoder {
    pub fn new(origin: i64) -> Self {
        Self {
            origin,
            extended: Mutex::new(Vec::new()),
        }
    }

    fn pack(&self, person: PersonKey, details: EventDetails) -> Event {
        let offset = details.time - self.origin;
        let payload = if details.category < 255 && (0..=i64::from(u32::MAX)).contains(&offset) {
            // The price is validated to at most 10,000,000, which fits in 24 bits.
            offset as u64 | (u64::from(details.price) << 32) | (u64::from(details.category) << 56)
        } else {
            let mut extended = self.extended.lock().expect("extended events lock");
            let index = extended.len() as u64;
            extended.push(details);
            EXTENDED_EVENT | index
        };
        Event { person, payload }
    }

    pub fn finish(self) -> EventDecoder {
        EventDecoder {
            origin: self.origin,
            extended: self.extended.into_inner().expect("extended events lock"),
        }
    }
}

impl EventDecoder {
    #[inline]
    pub fn unpack(&self, event: &Event) -> EventDetails {
        if event.payload >= EXTENDED_EVENT {
            return self.extended[(event.payload ^ EXTENDED_EVENT) as usize];
        }
        EventDetails {
            time: self.origin + i64::from(event.payload as u32),
            price: ((event.payload >> 32) as u32) & 0x00ff_ffff,
            category: (event.payload >> 56) as u32,
        }
    }

    pub fn sort_time(&self, events: &mut [Event]) {
        if self.extended.is_empty() {
            events.sort_unstable_by_key(|row| row.payload as u32);
        } else {
            events.sort_unstable_by_key(|row| self.unpack(row).time);
        }
    }
}

pub struct Click<'a> {
    pub person: PersonKey,
    pub time: i64,
    pub id: &'a [u8],
    pub campaign: u32,
}

struct Candidate {
    key: u64,
    priority: i64,
    event: Option<Event>,
}

struct LongCandidate<'a> {
    id: &'a [u8],
    row: Candidate,
}

pub struct EventCandidates<'a> {
    short: [Vec<Candidate>; SHARDS],
    long: [Vec<LongCandidate<'a>>; SHARDS],
}

struct Shards<T>([Mutex<Vec<T>>; SHARDS]);

impl<T> Shards<T> {
    fn with_capacity(rows: usize) -> Self {
        // The estimate only reserves space. Shards still grow for uneven data.
        let per_shard = (rows / SHARDS).min(600_000);
        Self(std::array::from_fn(|_| {
            Mutex::new(Vec::with_capacity(per_shard))
        }))
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
    products: &Products<'_>,
    identities: &People<'a>,
    window: &Range<i64>,
    ingest: i64,
    encoder: &EventEncoder,
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
    let person = identities.key(user);
    Some(encoder.pack(
        person,
        EventDetails {
            time,
            price,
            category,
        },
    ))
}

pub fn event_candidates<'a>(
    input: &'a Input,
    products: &Products<'_>,
    identities: &People<'a>,
    window: Range<i64>,
    hashes: &RandomState,
    encoder: &EventEncoder,
) -> EventCandidates<'a> {
    let short = Shards::with_capacity(input.bytes() / 65);
    let long = Shards::with_capacity(0);
    input.chunks.par_iter().for_each_init(
        || {
            (
                std::array::from_fn(|_| Vec::new()),
                std::array::from_fn(|_| Vec::new()),
            )
        },
        |(short_local, long_local), chunk| {
            records::<7>(input.chunk_bytes(chunk), |values| {
                let id = values[0];
                if id.is_empty() {
                    return;
                }
                let priority = ingest_millis(values[3]);
                let event = event(values, products, identities, &window, priority, encoder);
                if let Some(key) = inline_ascii(id) {
                    short_local[shard(hashes.hash_one(key))].push(Candidate {
                        key,
                        priority,
                        event,
                    });
                } else {
                    let key = hashes.hash_one(id);
                    long_local[shard(key)].push(LongCandidate {
                        id,
                        row: Candidate {
                            key,
                            priority,
                            event,
                        },
                    });
                }
            });
            short.append(short_local);
            long.append(long_local);
        },
    );
    EventCandidates {
        short: short.into_vecs(),
        long: long.into_vecs(),
    }
}

fn deduplicate<T>(
    rows: &mut [T],
    key: impl Fn(&T) -> u64,
    same: impl Fn(&T, &T) -> bool,
    get: impl Fn(&mut T) -> &mut Candidate,
    hashes: &RandomState,
) {
    // Keep the first distinct ID for each bucket in a compact direct index.
    // Only different IDs that land in an occupied bucket use the overflow table.
    // A primary bucket changes owners only to another copy of the same ID.
    let length = u32::try_from(rows.len()).expect("event shard row count");
    let mut primary = vec![0u32; (rows.len() * 2).next_power_of_two()];
    let mask = primary.len() - 1;
    let mut overflow = HashTable::<u32>::with_capacity(rows.len() / 4);
    let winner = |rows: &mut [T], a: u32, b: u32| {
        let rank = get(&mut rows[a as usize]).priority;
        let (keep, discard) = if rank < get(&mut rows[b as usize]).priority {
            (a, b)
        } else {
            (b, a)
        };
        get(&mut rows[discard as usize]).event = None;
        keep
    };

    for current in 0..length {
        let a = &rows[current as usize];
        let id = key(a);
        let hash = hashes.hash_one(id);
        let bucket = &mut primary[hash as usize & mask];
        let Some(previous) = bucket.checked_sub(1) else {
            *bucket = current + 1;
            continue;
        };
        let b = &rows[previous as usize];
        if key(b) == id && same(b, a) {
            *bucket = winner(rows, current, previous) + 1;
            continue;
        }

        match overflow.entry(
            hash,
            |&existing| key(&rows[existing as usize]) == id && same(&rows[existing as usize], a),
            |&existing| hashes.hash_one(key(&rows[existing as usize])),
        ) {
            Entry::Vacant(slot) => {
                slot.insert(current);
            }
            Entry::Occupied(mut slot) => {
                *slot.get_mut() = winner(rows, current, *slot.get());
            }
        }
    }
}

impl EventCandidates<'_> {
    pub fn into_events(self, links: &LinkedIds, hashes: &RandomState) -> [Vec<Event>; SHARDS] {
        let rows = self.short.iter().map(Vec::len).sum::<usize>()
            + self.long.iter().map(Vec::len).sum::<usize>();
        let people = Shards::with_capacity(rows + rows / 16);
        self.short.into_par_iter().zip(self.long).for_each_init(
            || std::array::from_fn(|_| Vec::new()),
            |local, (mut short, mut long)| {
                deduplicate(&mut short, |row| row.key, |_, _| true, |row| row, hashes);
                deduplicate(
                    &mut long,
                    |row| row.row.key,
                    |a, b| a.id == b.id,
                    |row| &mut row.row,
                    hashes,
                );
                let events = short
                    .into_iter()
                    .filter_map(|row| row.event)
                    .chain(long.into_iter().filter_map(|row| row.row.event));
                for mut event in events {
                    event.person = links.person(event.person);
                    local[shard(hashes.hash_one(event.person))].push(event);
                }
                people.append(local);
            },
        );
        people.into_vecs()
    }
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
    let people = Shards::with_capacity(input.bytes() / 40);
    input.chunks.par_iter().for_each_init(
        || std::array::from_fn(|_| Vec::new()),
        |local, chunk| {
            records::<4>(input.chunk_bytes(chunk), |[id, user, campaign, time]| {
                if id.is_empty() || user.is_empty() {
                    return;
                }
                let Some(time) = timestamp(time).filter(|time| window.contains(time)) else {
                    return;
                };
                let Some(&campaign) = campaigns.codes.get(campaign) else {
                    return;
                };
                let person = links.person(identities.key(user));
                local[shard(hashes.hash_one(person))].push(Click {
                    person,
                    time,
                    id,
                    campaign,
                });
            });
            people.append(local);
        },
    );
    people.into_vecs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_and_extended_payloads_sort_by_the_same_clock() {
        let identities = People::default();
        let person = identities.key(b"user");
        let encoder = EventEncoder::new(100);
        let mut events = [
            encoder.pack(
                person,
                EventDetails {
                    time: 100 + (1i64 << 32),
                    price: 10_000_000,
                    category: 3,
                },
            ),
            encoder.pack(
                person,
                EventDetails {
                    time: 300,
                    price: 12,
                    category: 255,
                },
            ),
            encoder.pack(
                person,
                EventDetails {
                    time: 101,
                    price: 24,
                    category: 254,
                },
            ),
        ];
        let decoder = encoder.finish();
        decoder.sort_time(&mut events);
        let rows = events.map(|row| {
            let event = decoder.unpack(&row);
            (event.time, event.price, event.category)
        });
        assert_eq!(
            rows,
            [
                (101, 24, 254),
                (300, 12, 255),
                (100 + (1i64 << 32), 10_000_000, 3)
            ]
        );
    }

    #[test]
    fn colliding_ids_choose_their_own_earliest_copy() {
        let hashes = RandomState::with_seeds(1, 2, 3, 4);
        let key_a = 1u64;
        let key_b = (2u64..)
            .find(|&key| hashes.hash_one(key) & 15 == hashes.hash_one(key_a) & 15)
            .expect("colliding key");
        let people = People::default();
        let event = EventEncoder::new(0).pack(
            people.key(b"user"),
            EventDetails {
                time: 17,
                price: 0,
                category: 0,
            },
        );
        let mut rows = [
            (key_a, 3, Some(event)),
            (key_b, 4, Some(event)),
            (key_a, 2, Some(event)),
            (key_b, 1, None),
            (key_b, 5, Some(event)),
        ]
        .map(|(key, priority, event)| Candidate {
            key,
            priority,
            event,
        });
        deduplicate(&mut rows, |row| row.key, |_, _| true, |row| row, &hashes);
        assert_eq!(
            rows.map(|row| row.event.is_some()),
            [false, false, true, false, false]
        );
    }
}
