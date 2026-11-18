// SPDX-License-Identifier: Apache-2.0
//! One bounded event queue per NeoHub session; overflow is observable by the client.
use crate::{Event, MAX_EVENTS, encode_batch};
use alloc::collections::VecDeque;
use alloc::vec::Vec;

#[derive(Default)]
pub struct SessionQueue {
    events: VecDeque<Event>,
    dropped: u32,
}
impl SessionQueue {
    pub fn new() -> Self {
        Self::default()
    }
    /// Returns true when the oldest event was dropped.
    pub fn push(&mut self, event: Event) -> bool {
        let overflow = self.events.len() == MAX_EVENTS;
        if overflow {
            self.events.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.events.push_back(event);
        overflow
    }
    pub fn len(&self) -> usize {
        self.events.len()
    }
    pub fn is_empty(&self) -> bool {
        self.events.is_empty() && self.dropped == 0
    }
    pub fn dropped(&self) -> u32 {
        self.dropped
    }
    /// Removes only whole events that fit. None means the caller must retain its RECV.
    pub fn batch(&mut self, max_len: usize) -> Option<Vec<u8>> {
        let mut events = Vec::with_capacity(self.events.len() + 1);
        if self.dropped != 0 {
            events.push(Event::Dropped(self.dropped));
        }
        events.extend(self.events.iter().cloned());
        let (b, mut count) = encode_batch(&events, max_len);
        if count == 0 {
            return None;
        }
        if self.dropped != 0 {
            self.dropped = 0;
            count -= 1;
        }
        self.events.drain(..count);
        Some(b)
    }
}
