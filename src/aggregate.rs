use crate::clean::{Click, Event, EventDecoder};
use crate::dimensions::Map;
use crate::input::{DAY, WEEK};
use crate::person::PersonKey;

const SESSION_GAP: i64 = 30 * 60 * 1_000;

#[derive(Clone, Copy, Default)]
pub struct CampaignDay {
    pub sessions: i64,
    pub purchasing_sessions: i64,
    pub revenue: i64,
    pub assists: i64,
}

impl CampaignDay {
    fn merge(&mut self, other: Self) {
        self.sessions += other.sessions;
        self.purchasing_sessions += other.purchasing_sessions;
        self.revenue += other.revenue;
        self.assists += other.assists;
    }
}

#[derive(Clone, Copy)]
pub struct CampaignLayout {
    first: i32,
    width: usize,
    cells: usize,
}

impl CampaignLayout {
    pub fn new(start: i64, end: i64, width: usize) -> Self {
        let first = utc_day(start);
        let days = (i64::from(utc_day(end - 1)) - i64::from(first) + 1) as usize;
        // Bound the allocation for wider manifests or campaign lists.
        let cells = days
            .checked_mul(width)
            .filter(|&n| n <= 65_536)
            .unwrap_or(0);
        Self {
            first,
            width,
            cells,
        }
    }
}

pub enum CampaignDays {
    Dense {
        start: i32,
        width: usize,
        cells: Vec<CampaignDay>,
    },
    Sparse(Map<(u32, i32), CampaignDay>),
}

impl CampaignDays {
    pub fn new(layout: CampaignLayout) -> Self {
        if layout.cells == 0 {
            Self::Sparse(Map::default())
        } else {
            Self::Dense {
                start: layout.first,
                width: layout.width,
                cells: vec![CampaignDay::default(); layout.cells],
            }
        }
    }

    #[inline]
    fn get_mut(&mut self, campaign: u32, day: i32) -> &mut CampaignDay {
        match self {
            Self::Dense {
                start,
                width,
                cells,
            } => &mut cells[(day - *start) as usize * *width + campaign as usize],
            Self::Sparse(cells) => cells.entry((campaign, day)).or_default(),
        }
    }

    pub fn merge(mut self, other: Self) -> Self {
        match (&mut self, other) {
            (Self::Dense { cells, .. }, Self::Dense { cells: other, .. }) => {
                for (left, right) in cells.iter_mut().zip(other) {
                    left.merge(right);
                }
            }
            (Self::Sparse(cells), Self::Sparse(other)) => {
                for (key, value) in other {
                    cells.entry(key).or_default().merge(value);
                }
            }
            _ => unreachable!("all shards use the same campaign layout"),
        }
        self
    }

    pub fn into_rows(self) -> Vec<((u32, i32), CampaignDay)> {
        match self {
            Self::Dense {
                start,
                width,
                cells,
            } => cells
                .into_iter()
                .enumerate()
                .filter(|(_, value)| value.sessions != 0 || value.assists != 0)
                .map(|(index, value)| {
                    (
                        ((index % width) as u32, start + (index / width) as i32),
                        value,
                    )
                })
                .collect(),
            Self::Sparse(cells) => cells.into_iter().collect(),
        }
    }
}

pub struct UserDay<'a> {
    pub person: PersonKey,
    pub day: i32,
    pub sessions: i64,
    pub events: i64,
    pub revenue: i64,
    pub top: Option<&'a str>,
}

struct Session {
    start: i64,
    revenue: i64,
}

#[derive(Default)]
struct DailyTotals {
    sessions: i64,
    events: i64,
    revenue: i64,
}

#[derive(Clone, Copy)]
struct Coverage {
    first_click: i64,
    last_click: i64,
}

pub struct Aggregator<'a> {
    pub campaigns: CampaignDays,
    sessions: Vec<Session>,
    categories: &'a [&'a str],
    category_revenue: Vec<i64>,
    purchased_categories: Vec<usize>,
    coverage: Vec<Option<Coverage>>,
    touched_campaigns: Vec<u32>,
    organic: u32,
}

fn utc_day(time: i64) -> i32 {
    time.div_euclid(DAY) as i32
}

impl<'a> Aggregator<'a> {
    pub fn new(categories: &'a [&'a str], organic: u32, layout: CampaignLayout) -> Self {
        Self {
            campaigns: CampaignDays::new(layout),
            sessions: Vec::new(),
            categories,
            category_revenue: vec![0; categories.len()],
            purchased_categories: Vec::new(),
            coverage: vec![None; organic as usize],
            touched_campaigns: Vec::new(),
            organic,
        }
    }

    pub fn person(
        &mut self,
        person: PersonKey,
        events: &[Event],
        clicks: &[Click<'_>],
        decoder: &EventDecoder,
        output: &mut Vec<UserDay<'a>>,
    ) {
        self.sessions.clear();
        let mut session = Session {
            start: decoder.unpack(&events[0]).time,
            revenue: 0,
        };
        let mut previous_time = session.start;
        let mut day = utc_day(session.start);
        let mut totals = DailyTotals {
            sessions: 1,
            ..DailyTotals::default()
        };

        for event in events {
            let event = decoder.unpack(event);
            if event.time - previous_time > SESSION_GAP {
                self.sessions.push(session);
                session = Session {
                    start: event.time,
                    revenue: 0,
                };
                let start_day = utc_day(event.time);
                if start_day != day {
                    output.push(self.finish_day(person, day, totals));
                    totals = DailyTotals::default();
                    day = start_day;
                }
                totals.sessions += 1;
            }
            previous_time = event.time;
            totals.events += 1;
            if event.price != 0 {
                let price = i64::from(event.price);
                session.revenue += price;
                totals.revenue += price;
                let category = event.category as usize;
                let revenue = &mut self.category_revenue[category];
                if *revenue == 0 {
                    self.purchased_categories.push(category);
                }
                *revenue += price;
            }
        }

        self.sessions.push(session);
        output.push(self.finish_day(person, day, totals));
        self.attribute(clicks);
    }

    fn finish_day(&mut self, person: PersonKey, day: i32, totals: DailyTotals) -> UserDay<'a> {
        let mut top = None;
        let mut top_revenue = 0;
        for category in self.purchased_categories.drain(..) {
            let revenue = std::mem::take(&mut self.category_revenue[category]);
            if revenue > top_revenue || (revenue == top_revenue && Some(category) < top) {
                top = Some(category);
                top_revenue = revenue;
            }
        }
        UserDay {
            person,
            day,
            sessions: totals.sessions,
            events: totals.events,
            revenue: totals.revenue,
            top: top.map(|category| self.categories[category]),
        }
    }

    fn attribute(&mut self, clicks: &[Click<'_>]) {
        let mut next = 0;
        for session in &self.sessions {
            while next < clicks.len() && clicks[next].time <= session.start {
                let click = &clicks[next];
                let slot = &mut self.coverage[click.campaign as usize];
                match slot {
                    Some(interval) if click.time <= interval.last_click + WEEK => {
                        interval.last_click = click.time;
                    }
                    Some(interval) => {
                        add_assists(
                            &self.sessions,
                            &mut self.campaigns,
                            click.campaign,
                            *interval,
                        );
                        *interval = Coverage {
                            first_click: click.time,
                            last_click: click.time,
                        };
                    }
                    None => {
                        *slot = Some(Coverage {
                            first_click: click.time,
                            last_click: click.time,
                        });
                        self.touched_campaigns.push(click.campaign);
                    }
                }
                next += 1;
            }

            // Clicks are sorted by timestamp ascending and ID descending.
            let campaign = next
                .checked_sub(1)
                .map(|index| &clicks[index])
                .filter(|click| click.time >= session.start - WEEK)
                .map_or(self.organic, |click| click.campaign);
            let totals = self.campaigns.get_mut(campaign, utc_day(session.start));
            totals.sessions += 1;
            totals.purchasing_sessions += i64::from(session.revenue > 0);
            totals.revenue += session.revenue;
        }

        for campaign in self.touched_campaigns.drain(..) {
            let interval = self.coverage[campaign as usize]
                .take()
                .expect("active campaign");
            add_assists(&self.sessions, &mut self.campaigns, campaign, interval);
        }
    }
}

fn add_assists(sessions: &[Session], totals: &mut CampaignDays, campaign: u32, coverage: Coverage) {
    // Disjoint intervals for a campaign can never count the same session twice.
    let first = sessions.partition_point(|session| session.start < coverage.first_click);
    let len =
        sessions[first..].partition_point(|session| session.start <= coverage.last_click + WEEK);
    let mut covered = &sessions[first..first + len];
    while let Some(start) = covered.first() {
        let day = utc_day(start.start);
        let end_of_day = (i64::from(day) + 1) * DAY;
        let count = if covered.last().expect("nonempty interval").start < end_of_day {
            covered.len()
        } else {
            covered.partition_point(|session| session.start < end_of_day)
        };
        totals.get_mut(campaign, day).assists += count as i64;
        covered = &covered[count..];
    }
}
