#[allow(dead_code)]
#[path = "../src/csv.rs"]
mod csv;
#[allow(dead_code, unused_imports)]
#[path = "../src/input.rs"]
mod input;

use std::env;
use std::error::Error;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

use input::{Input, ingest_seconds, timestamp};
use rayon::prelude::*;

fn scan<const N: usize>(input: &Input, decode: impl Fn([&[u8]; N]) + Sync) -> usize {
    input
        .chunks
        .par_iter()
        .map(|chunk| {
            let mut rows = 0;
            csv::records(input.chunk_bytes(chunk), |row| {
                decode(black_box(row));
                rows += 1;
            });
            rows
        })
        .sum()
}

fn main() -> Result<(), Box<dyn Error>> {
    let start = Instant::now();
    let data = env::var_os("DATA_DIR").map_or_else(|| PathBuf::from("/data"), PathBuf::from);
    let workers = env::var("NCPU").map_or(Ok(8), |count| count.parse())?;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()?;
    let events = Input::open(&data.join("events"))?;
    let clicks = Input::open(&data.join("clicks"))?;
    let (events, clicks) = pool.install(|| {
        rayon::join(
            || {
                scan(&events, |[id, user, time, ingest, _, product, _]| {
                    if !id.is_empty() {
                        black_box(ingest_seconds(ingest));
                        if !user.is_empty() && !product.is_empty() {
                            black_box(timestamp(time));
                        }
                    }
                })
            },
            || {
                scan(&clicks, |[id, user, _, time]| {
                    if !id.is_empty() && !user.is_empty() {
                        black_box(timestamp(time));
                    }
                })
            },
        )
    });
    eprintln!(
        "parser body: {:.6}s ({events} event rows, {clicks} click rows)",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}
