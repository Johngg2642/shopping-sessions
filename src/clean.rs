use std::ops::Range;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use ahash::RandomState;
use hashbrown::HashTable;
use hashbrown::hash_table::Entry;
use rayon::prelude::*;

use crate::csv::records;
use crate::dimensions::{Campaigns, LinkedIds, Products};
use crate::input::{DAY, Input, decimal, ingest_seconds, timestamp};

use crate::person::{People, PersonKey, inline_ascii};

const SHARDS: usize = 128;
const SHARD_BITS: u32 = SHARDS.trailing_zeros();
const SLOT_BITS: u32 = 25;
const SLOT_MASK: u64 = (1 << SLOT_BITS) - 1;

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
    row: InputRow,
}

#[derive(Clone, Copy)]
struct InputRow {
    chunk: u32,
    index: u32,
}

#[derive(Clone, Copy)]
struct RowData(u64);

impl RowData {
    fn new(ingest: i64, slot: usize) -> Self {
        // Four-digit UTC years fit in signed 39-bit epoch seconds. The low
        // 25 bits fit a partition and its local offset within an input chunk.
        // A 2 MiB chunk has fewer than 80,000 records given the ingest field and
        // separators, even if its last line extends past the chunk boundary.
        // Slot zero keeps the ingest time of a row with no valid event payload.
        debug_assert!((-(1i64 << 38)..1i64 << 38).contains(&ingest));
        assert!(slot as u64 <= SLOT_MASK, "event slot within input chunk");
        Self((ingest as u64) << SLOT_BITS | slot as u64)
    }

    fn priority(self) -> i64 {
        (self.0 as i64) >> SLOT_BITS
    }

    fn reject(self, starts: &[usize; SHARDS], rejected: &[Vec<AtomicU64>; SHARDS]) {
        let encoded_slot = (self.0 & SLOT_MASK) as usize;
        if encoded_slot != 0 {
            let slot = encoded_slot - 1;
            let shard = slot & (SHARDS - 1);
            let index = starts[shard] + (slot >> SHARD_BITS);
            rejected[shard][index / 64].fetch_or(1 << (index % 64), Ordering::Relaxed);
        }
    }
}

struct InputChunkRows {
    // Translate local row slots only for duplicate losers. The hot side index
    // can retain a 16-byte key and row reference for common short IDs.
    starts: [usize; SHARDS],
    rows: Vec<RowData>,
}

struct LongCandidate<'a> {
    id: &'a [u8],
    row: Candidate,
}

pub struct EventCandidates<'a> {
    events: [Vec<Event>; SHARDS],
    chunks: Vec<InputChunkRows>,
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
        self.append_with_starts(local, |_, _| {});
    }

    fn append_with_starts(
        &self,
        local: &mut [Vec<T>; SHARDS],
        mut start: impl FnMut(usize, usize),
    ) {
        for (shard, (shared, rows)) in self.0.iter().zip(local).enumerate() {
            if !rows.is_empty() {
                let mut shared = shared.lock().expect("shard lock");
                start(shard, shared.len());
                shared.append(rows);
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
    if !window.contains(&time) || ingest * 1_000 - DAY > time {
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
    links: &LinkedIds,
    window: Range<i64>,
    hashes: &RandomState,
    encoder: &EventEncoder,
) -> EventCandidates<'a> {
    let rows = input.bytes() / 65;
    let people = Shards::with_capacity(rows);
    let short = Shards::with_capacity(rows);
    let long = Shards::with_capacity(0);
    let chunks = input
        .chunks
        .par_iter()
        .enumerate()
        .map_init(
            || {
                (
                    std::array::from_fn(|_| Vec::new()),
                    std::array::from_fn(|_| Vec::new()),
                    std::array::from_fn(|_| Vec::new()),
                )
            },
            |(short_local, long_local, person_local), (chunk_id, chunk)| {
                let chunk_id = u32::try_from(chunk_id).expect("input chunk index");
                let bytes = input.chunk_bytes(chunk);
                let mut rows = Vec::with_capacity(bytes.len() / 75);
                records::<7>(bytes, |values| {
                    let id = values[0];
                    if id.is_empty() {
                        return;
                    }
                    let ingest = ingest_seconds(values[3]);
                    let slot = event(values, products, identities, &window, ingest, encoder)
                        .map_or(0, |mut event| {
                            event.person = links.person(event.person);
                            let partition = shard(hashes.hash_one(event.person));
                            let events = &mut person_local[partition];
                            let slot = events.len() * SHARDS + partition + 1;
                            events.push(event);
                            slot
                        });
                    let row = InputRow {
                        chunk: chunk_id,
                        index: u32::try_from(rows.len()).expect("input row index"),
                    };
                    rows.push(RowData::new(ingest, slot));
                    if let Some(key) = inline_ascii(id) {
                        short_local[shard(hashes.hash_one(key))].push(Candidate { key, row });
                    } else {
                        let key = hashes.hash_one(id);
                        long_local[shard(key)].push(LongCandidate {
                            id,
                            row: Candidate { key, row },
                        });
                    }
                });
                let mut starts = [0; SHARDS];
                people.append_with_starts(person_local, |shard, start| starts[shard] = start);
                short.append(short_local);
                long.append(long_local);
                InputChunkRows { starts, rows }
            },
        )
        .collect();
    EventCandidates {
        events: people.into_vecs(),
        chunks,
        short: short.into_vecs(),
        long: long.into_vecs(),
    }
}

fn deduplicate<T>(
    rows: &[T],
    candidate: impl Fn(&T) -> &Candidate,
    same: impl Fn(&T, &T) -> bool,
    chunks: &[InputChunkRows],
    rejected: &[Vec<AtomicU64>; SHARDS],
    hashes: &RandomState,
) {
    // Keep the first distinct ID for each bucket in a compact direct index.
    // Only different IDs that land in an occupied bucket use the overflow table.
    // A primary bucket changes owners only to another copy of the same ID.
    let length = u32::try_from(rows.len()).expect("event shard row count");
    if length < 2 {
        return;
    }
    // Store the one-based index and use its spare bits for a hash fingerprint.
    let index_mask = u32::MAX >> length.leading_zeros();
    let mut primary = vec![0u32; (rows.len() * 2).next_power_of_two()];
    let mask = primary.len() - 1;
    let mut overflow = HashTable::<u32>::with_capacity(rows.len() / 4);
    let lookup = |index: u32| {
        let row = candidate(&rows[index as usize]).row;
        let chunk = &chunks[row.chunk as usize];
        (&chunk.starts, chunk.rows[row.index as usize])
    };
    let winner = |a: u32, b: u32| {
        let (starts_a, first) = lookup(a);
        let (starts_b, second) = lookup(b);
        if first.priority() < second.priority() {
            second.reject(starts_b, rejected);
            a
        } else {
            first.reject(starts_a, rejected);
            b
        }
    };

    for current in 0..length {
        let a = &rows[current as usize];
        let id = candidate(a).key;
        let hash = hashes.hash_one(id);
        let fingerprint = (hash >> 32) as u32 & !index_mask;
        let bucket = &mut primary[hash as usize & mask];
        let entry = *bucket;
        if entry == 0 {
            *bucket = fingerprint | (current + 1);
            continue;
        }
        if entry & !index_mask == fingerprint {
            let previous = (entry & index_mask) - 1;
            let b = &rows[previous as usize];
            if candidate(b).key == id && same(b, a) {
                *bucket = fingerprint | (winner(current, previous) + 1);
                continue;
            }
        }

        match overflow.entry(
            hash,
            |&existing| {
                candidate(&rows[existing as usize]).key == id && same(&rows[existing as usize], a)
            },
            |&existing| hashes.hash_one(candidate(&rows[existing as usize]).key),
        ) {
            Entry::Vacant(slot) => {
                slot.insert(current);
            }
            Entry::Occupied(mut slot) => {
                *slot.get_mut() = winner(current, *slot.get());
            }
        }
    }
}

impl EventCandidates<'_> {
    pub fn into_events(self, hashes: &RandomState) -> [Vec<Event>; SHARDS] {
        let Self {
            mut events,
            chunks,
            short,
            long,
        } = self;
        let rejected = std::array::from_fn(|shard| {
            (0..events[shard].len().div_ceil(64))
                .map(|_| AtomicU64::new(0))
                .collect()
        });
        short.into_par_iter().zip(long).for_each(|(short, long)| {
            deduplicate(&short, |row| row, |_, _| true, &chunks, &rejected, hashes);
            deduplicate(
                &long,
                |row| &row.row,
                |a, b| a.id == b.id,
                &chunks,
                &rejected,
                hashes,
            );
        });
        events
            .par_iter_mut()
            .zip(rejected)
            .for_each(|(events, bits)| {
                remove_rejected(events, bits);
            });
        events
    }
}

fn remove_rejected(events: &mut Vec<Event>, rejected: Vec<AtomicU64>) {
    // Work backward so swapping in the last survivor cannot move a row that
    // still needs to be discarded. Aggregation sorts these partitions later.
    for (word, mask) in rejected.into_iter().enumerate().rev() {
        let mut mask = mask.into_inner();
        while mask != 0 {
            let bit = (u64::BITS - 1) - mask.leading_zeros();
            events.swap_remove(word * 64 + bit as usize);
            mask ^= 1 << bit;
        }
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
    fn colliding_ids_choose_their_own_earliest_copy_across_person_partitions() {
        let hashes = RandomState::with_seeds(1, 2, 3, 4);
        let key_a = 1u64;
        let key_b = (2u64..)
            .find(|&key| hashes.hash_one(key) & 15 == hashes.hash_one(key_a) & 15)
            .expect("colliding key");
        let rows = [
            (key_a, 0, 0),
            (key_b, 0, 1),
            (key_a, 1, 0),
            (key_b, 0, 2),
            (key_b, 1, 1),
        ]
        .map(|(key, chunk, index)| Candidate {
            key,
            row: InputRow { chunk, index },
        });
        let mut starts = [0; SHARDS];
        starts[1] = 64;
        let chunks = [
            InputChunkRows {
                starts: [0; SHARDS],
                rows: vec![RowData::new(3, 1), RowData::new(4, 2), RowData::new(1, 0)],
            },
            InputChunkRows {
                starts,
                rows: vec![RowData::new(2, 3), RowData::new(5, 2)],
            },
        ];
        let rejected = std::array::from_fn(|_| vec![AtomicU64::new(0), AtomicU64::new(0)]);
        deduplicate(&rows, |row| row, |_, _| true, &chunks, &rejected, &hashes);
        assert_eq!(rejected[0][0].load(Ordering::Relaxed), 1);
        assert_eq!(rejected[1][0].load(Ordering::Relaxed), 1);
        assert_eq!(rejected[1][1].load(Ordering::Relaxed), 1);
        assert_eq!(rejected[2][0].load(Ordering::Relaxed), 0);
    }

    #[test]
    fn long_ids_with_identical_fingerprints_stay_distinct() {
        let candidate = |id, index| LongCandidate {
            id,
            row: Candidate {
                key: 1,
                row: InputRow { chunk: 0, index },
            },
        };
        let rows = [
            candidate(b"first-long-id", 0),
            candidate(b"other-long-id", 1),
            candidate(b"first-long-id", 2),
        ];
        let chunks = [InputChunkRows {
            starts: [0; SHARDS],
            rows: vec![
                RowData::new(2, 1),
                RowData::new(3, SHARDS + 1),
                RowData::new(-1, 0),
            ],
        }];
        let rejected = std::array::from_fn(|_| vec![AtomicU64::new(0)]);
        deduplicate(
            &rows,
            |row| &row.row,
            |a, b| a.id == b.id,
            &chunks,
            &rejected,
            &RandomState::with_seeds(1, 2, 3, 4),
        );
        assert_eq!(rejected[0][0].load(Ordering::Relaxed), 1);
    }

    #[test]
    fn removes_rejected_rows_across_bitmap_words_in_place() {
        let people = People::default();
        let person = people.key(b"user");
        let mut events: Vec<_> = (0..130).map(|payload| Event { person, payload }).collect();
        remove_rejected(
            &mut events,
            vec![
                AtomicU64::new(1 | 1 << 63),
                AtomicU64::new(1),
                AtomicU64::new(2),
            ],
        );
        events.sort_unstable_by_key(|event| event.payload);
        assert!(
            events
                .into_iter()
                .map(|event| event.payload)
                .eq((1..63).chain(65..129))
        );
    }
}
