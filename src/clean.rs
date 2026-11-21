use std::ops::Range;
use std::sync::Mutex;

use ahash::RandomState;
use hashbrown::HashTable;
use hashbrown::hash_table::Entry;
use rayon::prelude::*;

use crate::csv::records;
use crate::dimensions::{Campaigns, LinkedIds, Products};
use crate::input::{DAY, Input, decimal, ingest_seconds, timestamp};

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

const ID_BITS: u32 = 42;
const ID_MASK: u64 = (1 << ID_BITS) - 1;
const WIDE_RANK: u64 = u64::MAX >> ID_BITS;
const SHARD_BITS: u32 = SHARDS.trailing_zeros();

// The 42-bit ID leaves room for ingest seconds in the same word. A missing
// person marks an invalid or discarded row without losing its ingest priority.
struct PackedCandidate {
    id_and_rank: u64,
    payload_or_index: u64,
    person: Option<PersonKey>,
}

struct WideCandidate {
    rank: i64,
    payload: u64,
}

#[derive(Default)]
struct WideCandidates(Mutex<Vec<WideCandidate>>);

impl PackedCandidate {
    #[inline]
    fn new(id: u64, rank: i64, event: Option<Event>, wide: &WideCandidates) -> Self {
        let payload = event.map_or(0, |row| row.payload);
        let (rank, payload_or_index) = if (rank as u64) < WIDE_RANK {
            (rank as u64, payload)
        } else {
            let mut overflow = wide.0.lock().expect("wide candidate lock");
            let index = overflow.len() as u64;
            overflow.push(WideCandidate { rank, payload });
            (WIDE_RANK, index)
        };
        Self {
            id_and_rank: id | (rank << ID_BITS),
            payload_or_index,
            person: event.map(|row| row.person),
        }
    }

    fn event(self, wide: &[WideCandidate]) -> Option<Event> {
        let person = self.person?;
        let payload = if self.id_and_rank >> ID_BITS == WIDE_RANK {
            wide[self.payload_or_index as usize].payload
        } else {
            self.payload_or_index
        };
        Some(Event { person, payload })
    }
}

// For exactly eight lowercase base-36 digits, the numeric value uniquely
// identifies the spelling and fits in 42 bits. Other strings use the exact
// general ID representation, including strings with a different prefix or length.
#[inline]
fn packed_event_id(id: &[u8]) -> Option<u64> {
    let [b'e', digits @ ..] = *<&[u8; 9]>::try_from(id).ok()? else {
        return None;
    };
    let ascii = u64::from_le_bytes(digits);
    const TOP: u64 = 0x8080_8080_8080_8080;
    if ascii & TOP != 0 {
        return None;
    }
    // The high bit marks lanes at or above 'a'. Subtract '0' or 'a' - 10
    // in each lane and reject values outside the selected ASCII range.
    let letters = (ascii + 0x1f1f_1f1f_1f1f_1f1f) & TOP;
    let digits = (ascii | TOP) - (0x3030_3030_3030_3030 + (letters >> 7) * 39);
    if digits & TOP != TOP {
        return None;
    }
    let digits = digits ^ TOP;
    let above_digit = (digits + 0x7676_7676_7676_7676) & !letters;
    let above_letter = digits + 0x5c5c_5c5c_5c5c_5c5c;
    if (above_digit | above_letter) & TOP != 0 {
        return None;
    }
    const BYTE_LANES: u64 = 0x00ff_00ff_00ff_00ff;
    let pairs = (digits & BYTE_LANES) * 36 + ((digits >> 8) & BYTE_LANES);
    const PAIR_LANES: u64 = 0x0000_ffff_0000_ffff;
    let groups = (pairs & PAIR_LANES) * (36 * 36) + ((pairs >> 16) & PAIR_LANES);
    Some(u64::from(groups as u32) * (36 * 36 * 36 * 36) + (groups >> 32))
}

#[inline]
fn direct_event_key(id: u64) -> u64 {
    // Undo the final multiply and XOR shift in the public generator's ID
    // permutation. The low bits are then a permutation of sequential row
    // numbers. The mapping stays bijective over all 42-bit IDs.
    const LOW: u64 = (1 << 40) - 1;
    const INVERSE_MULTIPLIER: u64 = 0xa090_5881_e9;
    let mixed = id.wrapping_mul(INVERSE_MULTIPLIER) & LOW;
    (id & !LOW) | (mixed ^ (mixed >> 17) ^ (mixed >> 34))
}

#[inline]
fn direct_hash(key: u64, salt: u64) -> u64 {
    let key = key ^ salt;
    // Pair the direct bucket after the partition bits with a mixed fingerprint.
    let bucket = (key >> SHARD_BITS) as u32;
    let fingerprint = key.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (fingerprint & 0xffff_ffff_0000_0000) | u64::from(bucket)
}

pub struct EventCandidates<'a> {
    packed: [Vec<PackedCandidate>; SHARDS],
    short: [Vec<Candidate>; SHARDS],
    long: [Vec<LongCandidate<'a>>; SHARDS],
    wide: Vec<WideCandidate>,
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

#[inline]
fn event_hash(key: u64, salt: u64) -> u64 {
    // Mix both halves of a full-width product so the partition and table
    // index can use separate bits. Lookups still compare exact IDs.
    let product = u128::from(key ^ salt) * u128::from(0xa076_1d64_78bd_642f_u64);
    product as u64 ^ (product >> 64) as u64
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
    if !window.contains(&time) || ingest - DAY > time {
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
    let salt = hashes.hash_one(0u64);
    let packed = Shards::with_capacity(input.bytes() / 65);
    let short = Shards::with_capacity(input.bytes() / 640);
    let long = Shards::with_capacity(0);
    let wide = WideCandidates::default();
    // Leave room for ingest timestamps preceding the start of the window.
    // Dates outside the packed range retain their full priority in `wide`.
    let rank_origin = window.start.div_euclid(1_000) - (1 << 20);
    input.chunks.par_iter().for_each_init(
        || {
            (
                std::array::from_fn(|_| Vec::new()),
                std::array::from_fn(|_| Vec::new()),
                std::array::from_fn(|_| Vec::new()),
            )
        },
        |(packed_local, short_local, long_local), chunk| {
            records::<7>(input.chunk_bytes(chunk), |values| {
                let id = values[0];
                if id.is_empty() {
                    return;
                }
                let ingested_at = ingest_seconds(values[3]);
                let priority = ingested_at - rank_origin;
                let event = event(
                    values,
                    products,
                    identities,
                    &window,
                    ingested_at * 1_000,
                    encoder,
                );
                if let Some(key) = packed_event_id(id) {
                    let key = direct_event_key(key);
                    let partition = (key ^ salt) as usize & (SHARDS - 1);
                    packed_local[partition].push(PackedCandidate::new(key, priority, event, &wide));
                } else if let Some(key) = inline_ascii(id) {
                    short_local[shard(event_hash(key, salt))].push(Candidate {
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
            packed.append(packed_local);
            short.append(short_local);
            long.append(long_local);
        },
    );
    EventCandidates {
        packed: packed.into_vecs(),
        short: short.into_vecs(),
        long: long.into_vecs(),
        wide: wide.0.into_inner().expect("wide candidate lock"),
    }
}

fn deduplicate<T>(
    rows: &mut [T],
    key: impl Fn(&T) -> u64,
    same: impl Fn(&T, &T) -> bool,
    rank: impl Fn(&T) -> i64,
    discard: impl Fn(&mut T),
    hash_id: impl Fn(u64) -> u64,
    direct: bool,
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
    let bucket_count = if direct { rows.len() } else { rows.len() * 2 };
    let mut primary = vec![0u32; bucket_count.next_power_of_two()];
    let mask = primary.len() - 1;
    let overflow_capacity = if direct { 0 } else { rows.len() / 4 };
    let mut overflow = HashTable::<u32>::with_capacity(overflow_capacity);
    let winner = |rows: &mut [T], a: u32, b: u32| {
        let (keep, rejected) = if rank(&rows[a as usize]) < rank(&rows[b as usize]) {
            (a, b)
        } else {
            (b, a)
        };
        discard(&mut rows[rejected as usize]);
        keep
    };

    for current in 0..length {
        let a = &rows[current as usize];
        let id = key(a);
        let hash = hash_id(id);
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
            if key(b) == id && same(b, a) {
                *bucket = fingerprint | (winner(rows, current, previous) + 1);
                continue;
            }
        }

        match overflow.entry(
            hash,
            |&existing| key(&rows[existing as usize]) == id && same(&rows[existing as usize], a),
            |&existing| hash_id(key(&rows[existing as usize])),
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
        let rows = self.packed.iter().map(Vec::len).sum::<usize>()
            + self.short.iter().map(Vec::len).sum::<usize>()
            + self.long.iter().map(Vec::len).sum::<usize>();
        let people = Shards::with_capacity(rows + rows / 16);
        let wide = &self.wide;
        let salt = hashes.hash_one(0u64);
        self.packed
            .into_par_iter()
            .zip(self.short)
            .zip(self.long)
            .for_each_init(
                || std::array::from_fn(|_| Vec::new()),
                |local, ((mut packed, mut short), mut long)| {
                    deduplicate(
                        &mut packed,
                        |row| row.id_and_rank & ID_MASK,
                        |_, _| true,
                        |row| {
                            let rank = row.id_and_rank >> ID_BITS;
                            if rank == WIDE_RANK {
                                wide[row.payload_or_index as usize].rank
                            } else {
                                rank as i64
                            }
                        },
                        |row| row.person = None,
                        |id| direct_hash(id, salt),
                        true,
                    );
                    deduplicate(
                        &mut short,
                        |row| row.key,
                        |_, _| true,
                        |row| row.priority,
                        |row| row.event = None,
                        |id| event_hash(id, salt),
                        false,
                    );
                    deduplicate(
                        &mut long,
                        |row| row.row.key,
                        |a, b| a.id == b.id,
                        |row| row.row.priority,
                        |row| row.row.event = None,
                        |id| event_hash(id, salt),
                        false,
                    );
                    let events = packed
                        .into_iter()
                        .filter_map(|row| row.event(wide))
                        .chain(short.into_iter().filter_map(|row| row.event))
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
                let Some(campaign) = campaigns.code(campaign) else {
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
    fn direct_keys_keep_high_bits_and_have_distinct_slots_for_consecutive_ids() {
        const COUNT: usize = 1 << 10;
        const MASK: u64 = (1 << 40) - 1;
        let mut occupied = [false; COUNT];
        for row in 313..313 + COUNT as u64 {
            let key = (row.wrapping_mul(0x9e37_79b9_7f4b) ^ 42) & MASK;
            let scrambled = (key ^ (key >> 17)).wrapping_mul(0xd6e8_feb8_6659) & MASK;
            assert_eq!(direct_event_key(scrambled), key);
            assert_eq!(direct_event_key(scrambled | (1 << 41)), key | (1 << 41));
            let slot = key as usize & (COUNT - 1);
            assert!(!occupied[slot]);
            occupied[slot] = true;
        }
    }

    #[test]
    fn direct_bucket_conflict_does_not_merge_ids() {
        let people = People::default();
        let event = Event {
            person: people.key(b"user"),
            payload: 17,
        };
        let make = |key, priority, event| Candidate {
            key,
            priority,
            event,
        };
        let first = 42;
        let second = first + ((4 * SHARDS) as u64);
        let mut rows = [
            make(first, 5, Some(event)),
            make(second, 6, Some(event)),
            make(second, 4, None),
        ];
        deduplicate(
            &mut rows,
            |row| row.key,
            |_, _| true,
            |row| row.priority,
            |row| row.event = None,
            |id| direct_hash(id, 313),
            true,
        );
        assert_eq!(rows.map(|row| row.event.is_some()), [true, false, false]);
    }

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
        let salt = hashes.hash_one(0u64);
        let key_a = 1u64;
        let key_b = (2u64..)
            .find(|&key| event_hash(key, salt) & 15 == event_hash(key_a, salt) & 15)
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
        deduplicate(
            &mut rows,
            |row| row.key,
            |_, _| true,
            |row| row.priority,
            |row| row.event = None,
            |id| event_hash(id, salt),
            false,
        );
        assert_eq!(
            rows.map(|row| row.event.is_some()),
            [false, false, true, false, false]
        );
    }

    #[test]
    fn long_ids_with_identical_fingerprints_stay_distinct() {
        let people = People::default();
        let event = Event {
            person: people.key(b"user"),
            payload: 17,
        };
        let candidate = |id, priority, event| LongCandidate {
            id,
            row: Candidate {
                key: 1,
                priority,
                event,
            },
        };
        let mut rows = [
            candidate(b"first-long-id", 2, Some(event)),
            candidate(b"other-long-id", 3, Some(event)),
            candidate(b"first-long-id", 1, None),
        ];
        deduplicate(
            &mut rows,
            |row| row.row.key,
            |a, b| a.id == b.id,
            |row| row.row.priority,
            |row| row.row.event = None,
            |id| event_hash(id, RandomState::with_seeds(1, 2, 3, 4).hash_one(0u64)),
            false,
        );
        assert!(rows[0].row.event.is_none());
        assert!(rows[1].row.event.is_some());
        assert!(rows[2].row.event.is_none());
    }
}
