//! `cursor.json`: per channel, an ID below which every event has been handled.
//! It never moves past an event still in flight, and it stops moving while
//! recovery is failing, so a later live event cannot skip over a gap.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

#[derive(Default)]
struct Inner {
    pos: BTreeMap<String, String>,
    in_flight: BTreeMap<String, BTreeSet<String>>,
    finished: BTreeMap<String, BTreeSet<String>>,
    frozen: bool,
}

pub struct Cursor(Mutex<Inner>);

impl Cursor {
    pub fn new(pos: BTreeMap<String, String>) -> Self {
        Cursor(Mutex::new(Inner { pos, ..Default::default() }))
    }

    pub fn get(&self, channel: &str) -> Option<String> {
        self.0.lock().unwrap().pos.get(channel).cloned()
    }

    /// A channel's first run starts at `now`: nothing before it is recovered.
    pub fn init(&self, channel: &str, now: String) -> Option<BTreeMap<String, String>> {
        let mut g = self.0.lock().unwrap();
        if g.pos.contains_key(channel) {
            return None;
        }
        g.pos.insert(channel.to_string(), now);
        Some(g.pos.clone())
    }

    pub fn begin(&self, channel: &str, id: &str) {
        self.0
            .lock()
            .unwrap()
            .in_flight
            .entry(channel.into())
            .or_default()
            .insert(id.into());
    }

    pub fn freeze(&self, frozen: bool) {
        self.0.lock().unwrap().frozen = frozen;
    }

    /// Marks an event done; returns the positions to save when they moved.
    pub fn finish(&self, channel: &str, id: &str) -> Option<BTreeMap<String, String>> {
        let mut g = self.0.lock().unwrap();
        let g = &mut *g;
        let flight = g.in_flight.entry(channel.into()).or_default();
        flight.remove(id);
        let done = g.finished.entry(channel.into()).or_default();
        done.insert(id.into());
        if g.frozen {
            return None;
        }
        let best = match flight.first() {
            Some(min) => done.range::<String, _>(..min).next_back(),
            None => done.last(),
        }
        .cloned()?;
        if g.pos.get(channel).is_some_and(|p| *p >= best) {
            return None;
        }
        done.retain(|d| *d > best);
        g.pos.insert(channel.to_string(), best);
        Some(g.pos.clone())
    }
}
