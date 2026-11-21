mod aggregate;
mod clean;
mod csv;
mod dimensions;
mod input;
mod output;
mod person;

use std::env;
use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ahash::RandomState;
use rayon::prelude::*;

use crate::aggregate::{Aggregator, CampaignDays, CampaignLayout, UserDay};
use crate::clean::{Click, Event, EventDecoder, EventEncoder};
use crate::dimensions::{Campaigns, LinkedIds, Products};
use crate::input::{Input, WEEK, map_file, timestamp};
use crate::output::{Error, Users};
use crate::person::People;

fn window(data: &Path) -> Result<Range<i64>, Error> {
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(data.join("manifest.json"))?)?;
    let read = |name: &str| {
        manifest[name]
            .as_str()
            .and_then(|value| timestamp(value.as_bytes()))
            .ok_or_else(|| io::Error::other(format!("invalid manifest {name}")))
    };
    let range = read("window_start")?..read("window_end")?;
    if range.is_empty() {
        return Err(io::Error::other("empty manifest window").into());
    }
    Ok(range)
}

fn process<'a>(
    mut events: Vec<Event>,
    mut clicks: Vec<Click<'_>>,
    interned: &[&[u8]],
    decoder: &EventDecoder,
    users: &mut Users,
    aggregator: &mut Aggregator<'a>,
    rows: &mut Vec<UserDay<'a>>,
) -> Result<(), Error> {
    events.sort_unstable_by_key(|event| event.person);
    clicks.sort_unstable_by(|a, b| {
        a.person
            .cmp(&b.person)
            .then_with(|| a.time.cmp(&b.time))
            .then_with(|| b.id.cmp(a.id))
    });
    // Identical clicks leave the latest eligible campaign and its merged
    // coverage intervals unchanged, so attribution can consume them directly.

    let mut click_groups = clicks.chunk_by(|a, b| a.person == b.person).peekable();

    for events in events.chunk_by_mut(|a, b| a.person == b.person) {
        if events.len() > 1 {
            decoder.sort_time(events);
        }
        let person = events[0].person;
        while click_groups
            .peek()
            .is_some_and(|group| group[0].person < person)
        {
            click_groups.next();
        }
        let clicks = if click_groups
            .peek()
            .is_some_and(|group| group[0].person == person)
        {
            click_groups.next().expect("matching click group")
        } else {
            &[]
        };
        aggregator.person(person, events, clicks, decoder, rows);
        if rows.len() >= 8192 {
            users.write(rows, interned)?;
            rows.clear();
        }
    }

    Ok(())
}

fn run(data: &Path, out: &Path) -> Result<(), Error> {
    let profile = env::var_os("PROFILE").is_some();
    let mut previous = Instant::now();
    let mut elapsed = |stage: &str| {
        let now = Instant::now();
        if profile {
            eprintln!("{stage}: {:.3}s", (now - previous).as_secs_f64());
        }
        previous = now;
    };

    let window = window(data)?;
    let paths = (data.join("products.csv"), data.join("campaigns.csv"));
    let product_input = map_file(&paths.0)?;
    let campaign_input = map_file(&paths.1)?;
    let products = Products::new(product_input.as_deref().unwrap_or_default())?;
    let campaigns = Campaigns::new(campaign_input.as_deref().unwrap_or_default())?;
    let links = Input::open(&data.join("links"))?;
    let hashes = RandomState::new();
    let event_input = Input::open(&data.join("events"))?;
    let click_input = Input::open(&data.join("clicks"))?;
    let identities = People::default();
    let encoder = EventEncoder::new(window.start);
    let (linked_ids, candidates) = rayon::join(
        || LinkedIds::new(&links, &identities),
        || {
            clean::event_candidates(
                &event_input,
                &products,
                &identities,
                window.clone(),
                &hashes,
                &encoder,
            )
        },
    );
    elapsed("dimensions, linked IDs, and parse events");
    let (events, clicks) = rayon::join(
        || candidates.into_events(&linked_ids, &hashes),
        || {
            clean::clicks(
                &click_input,
                &linked_ids,
                &campaigns,
                &identities,
                window.start - WEEK..window.end,
                &hashes,
            )
        },
    );
    let interned = identities.into_names();
    let decoder = encoder.finish();
    elapsed("deduplicate events and parse clicks");

    let user_dir = out.join("user_daily");
    let campaign_dir = out.join("campaign_daily");
    fs::create_dir_all(&user_dir)?;
    fs::create_dir_all(&campaign_dir)?;
    if events.iter().all(Vec::is_empty) {
        Users::new(&user_dir.join("part-000.parquet"))?.finish()?;
    }

    let layout = CampaignLayout::new(window.start, window.end, campaigns.labels.len());
    // Give each task enough shards to reuse its writer and campaign totals.
    let totals = events
        .into_par_iter()
        .zip(clicks)
        .enumerate()
        .with_min_len(4)
        .try_fold(
            || None,
            |mut workspace, (part, (events, clicks))| {
                if !events.is_empty() {
                    if workspace.is_none() {
                        workspace = Some((
                            Aggregator::new(&products.categories, campaigns.organic, layout),
                            Vec::with_capacity(8192),
                            Users::new(&user_dir.join(format!("part-{part:03}.parquet")))?,
                        ));
                    }
                    let (aggregator, rows, users) =
                        workspace.as_mut().expect("active output group");
                    process(events, clicks, &interned, &decoder, users, aggregator, rows)?;
                }
                Ok::<_, Error>(workspace)
            },
        )
        .filter_map(|workspace| {
            workspace.transpose().map(|result| {
                let (aggregator, rows, mut users) = result?;
                if !rows.is_empty() {
                    users.write(&rows, &interned)?;
                }
                users.finish()?;
                Ok::<_, Error>(aggregator.campaigns)
            })
        })
        .try_reduce_with(|left, right| Ok(left.merge(right)))
        .transpose()?
        .unwrap_or_else(|| CampaignDays::new(layout));
    elapsed("sort, sessionize, attribute, and write users");
    output::campaigns(
        &campaign_dir.join("part-000.parquet"),
        totals,
        &campaigns.labels,
    )?;
    elapsed("write campaigns");
    Ok(())
}

fn main() -> Result<(), Error> {
    let mut args = env::args_os().skip(1);
    let data = args
        .next()
        .or_else(|| env::var_os("DATA_DIR"))
        .map_or_else(|| PathBuf::from("/data"), PathBuf::from);
    let output = args
        .next()
        .or_else(|| env::var_os("OUTPUT_DIR"))
        .map_or_else(|| PathBuf::from("/output"), PathBuf::from);
    if args.next().is_some() {
        return Err(io::Error::other("usage: shopping-sessions [DATA_DIR OUTPUT_DIR]").into());
    }
    let workers = env::var("NCPU")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8);
    if workers == 0 {
        return Err(io::Error::other("NCPU must be positive").into());
    }
    rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()?
        .install(|| run(&data, &output))
}
