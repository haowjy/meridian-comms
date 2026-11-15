//! `cursor.json`: per channel, an ID below which every event has been handled.
//! It never moves past an event still in flight, and a channel's position stops
//! moving while that channel's recovery is failing, so a later live event cannot
//! skip over a gap. Every save happens under the mutex, so saves cannot reorder.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use anyhow::Result;

use crate::state::Store;

#[derive(Default)]
struct Inner {
    pos: BTreeMap<String, String>,
    in_flight: BTreeMap<String, BTreeSet<String>>,
    finished: BTreeMap<String, BTreeSet<String>>,
    frozen: BTreeSet<String>,
}

pub struct Cursor {
    inner: Mutex<Inner>,
    store: Store,
}

impl Cursor {
    pub fn new(store: Store) -> Self {
        let pos = store.load_cursor();
        Cursor {
            inner: Mutex::new(Inner { pos, ..Default::default() }),
            store,
        }
    }

    pub fn get(&self, channel: &str) -> Option<String> {
        self.inner.lock().unwrap().pos.get(channel).cloned()
    }

    /// A channel's first run starts at `now`: nothing before it is recovered.
    pub fn init(&self, channel: &str, now: String) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        if g.pos.contains_key(channel) {
            return Ok(());
        }
        g.pos.insert(channel.to_string(), now);
        self.store.save_cursor(&g.pos)
    }

    pub fn begin(&self, channel: &str, id: &str) {
        self.inner
            .lock()
            .unwrap()
            .in_flight
            .entry(channel.into())
            .or_default()
            .insert(id.into());
    }

    pub fn freeze(&self, channel: &str, frozen: bool) {
        let mut g = self.inner.lock().unwrap();
        if frozen {
            g.frozen.insert(channel.to_string());
        } else {
            g.frozen.remove(channel);
        }
    }

    /// Marks an event done and saves the positions when this one moved.
    pub fn finish(&self, channel: &str, id: &str) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        let g = &mut *g;
        let flight = g.in_flight.entry(channel.into()).or_default();
        flight.remove(id);
        let done = g.finished.entry(channel.into()).or_default();
        done.insert(id.into());
        if g.frozen.contains(channel) {
            return Ok(());
        }
        let best = match flight.first() {
            Some(min) => done.range::<String, _>(..min).next_back(),
            None => done.last(),
        }
        .cloned();
        let Some(best) = best else { return Ok(()) };
        if g.pos.get(channel).is_some_and(|p| *p >= best) {
            return Ok(());
        }
        done.retain(|d| *d > best);
        g.pos.insert(channel.to_string(), best);
        self.store.save_cursor(&g.pos)
    }
}
