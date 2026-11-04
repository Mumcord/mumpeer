//! One subscriber's queue with the same pacing policy as Mumcord's local fan-out
//! (`screen_share/fanout.rs`, spec 07 FR-7.45): a subscriber whose backlog grows loses the highest
//! temporal layer first (30 → 15 → 7.5 fps with L1T3) and gets it back once it keeps up; a queue
//! that overflows is cleared and the subscriber gets nothing until the next keyframe, so a slow
//! subscriber never holds up the others.
//!
//! Sound has its own short queue and goes out before waiting pictures; control units (`key`)
//! go out first of all. Plugin data (spec 09 FR-9.85) has the lowest priority: it goes out only
//! when nothing else waits. Every queue is bounded.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use super::control::{Content, MAX_TEMPORAL_LAYER, Unit};
use crate::limits::lock;

/// Frames waiting for one subscriber before it counts as overflowing (about a quarter second).
const CAPACITY: usize = 8;
/// A backlog this long means the subscriber does not keep up at its current frame rate.
const LAG_AT: usize = 3;
/// Time between two layer drops, so one burst does not cost every layer at once.
const DROP_PAUSE: Duration = Duration::from_millis(500);
/// How long a subscriber has to keep up before it gets the next layer back.
const RAISE_AFTER: Duration = Duration::from_secs(3);
/// Sound units waiting for one subscriber (half a second of 20 ms packets).
const AUDIO_CAPACITY: usize = 25;
/// `key` units waiting for one subscriber; it gets one per key change.
const CONTROL_CAPACITY: usize = 8;
/// Plugin-data units waiting for one subscriber; a sender over its rate loses the oldest.
const DATA_CAPACITY: usize = 32;

/// What the relay has to do after a unit was queued.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pushed {
    pub need_keyframe: bool,
}

/// The queue and layer choice of one subscriber, without any I/O.
pub struct Policy {
    frames: VecDeque<Unit>,
    audio: VecDeque<Unit>,
    control: VecDeque<Unit>,
    data: VecDeque<Unit>,
    /// Highest temporal layer the pacing allows.
    max_layer: u8,
    /// Highest temporal layer the subscriber asked for (`layers`).
    wanted_layer: u8,
    /// After a join or an overflow nothing decodes until the next keyframe.
    wait_keyframe: bool,
    last_drop: Option<Instant>,
    keeping_up_since: Option<Instant>,
}

impl Default for Policy {
    fn default() -> Self {
        Self::new()
    }
}

impl Policy {
    pub fn new() -> Self {
        Self {
            frames: VecDeque::with_capacity(CAPACITY),
            audio: VecDeque::with_capacity(AUDIO_CAPACITY),
            control: VecDeque::with_capacity(CONTROL_CAPACITY),
            data: VecDeque::new(),
            max_layer: MAX_TEMPORAL_LAYER,
            wanted_layer: MAX_TEMPORAL_LAYER,
            wait_keyframe: true,
            last_drop: None,
            keeping_up_since: None,
        }
    }

    pub fn push(&mut self, unit: Unit, now: Instant) -> Pushed {
        let (keyframe, layer) = match unit.header.content {
            Content::Video {
                keyframe,
                temporal_layer,
                ..
            } => (keyframe, temporal_layer),
            Content::Audio => {
                push_bounded(&mut self.audio, AUDIO_CAPACITY, unit);
                return Pushed::default();
            }
            Content::Control(_) => {
                push_bounded(&mut self.control, CONTROL_CAPACITY, unit);
                return Pushed::default();
            }
            Content::Data => {
                push_bounded(&mut self.data, DATA_CAPACITY, unit);
                return Pushed::default();
            }
        };
        if keyframe {
            // Nothing queued before a keyframe is needed any more.
            self.frames.clear();
            self.wait_keyframe = false;
        } else if self.wait_keyframe || layer > self.max_layer.min(self.wanted_layer) {
            return Pushed::default();
        }
        self.adapt(now);
        if self.frames.len() >= CAPACITY {
            // Far behind: restart at the lowest frame rate and climb back from there.
            self.frames.clear();
            self.wait_keyframe = true;
            self.max_layer = 0;
            self.last_drop = Some(now);
            return Pushed {
                need_keyframe: true,
            };
        }
        self.frames.push_back(unit);
        Pushed::default()
    }

    fn adapt(&mut self, now: Instant) {
        let backlog = self.frames.len();
        if backlog >= LAG_AT {
            self.keeping_up_since = None;
            let paused = self
                .last_drop
                .is_some_and(|at| now.duration_since(at) < DROP_PAUSE);
            if self.max_layer > 0 && !paused {
                self.max_layer -= 1;
                self.last_drop = Some(now);
            }
        } else if backlog <= 1 {
            let since = *self.keeping_up_since.get_or_insert(now);
            if self.max_layer < MAX_TEMPORAL_LAYER && now.duration_since(since) >= RAISE_AFTER {
                self.max_layer += 1;
                self.keeping_up_since = Some(now);
            }
        }
    }

    /// A video unit was lost before it reached this subscriber's queue (the publisher went over
    /// its upload limit): what is queued still plays, nothing after it until the next keyframe.
    pub fn resync(&mut self) {
        self.wait_keyframe = true;
    }

    pub fn set_wanted_layer(&mut self, layer: u8) {
        self.wanted_layer = layer.min(MAX_TEMPORAL_LAYER);
    }

    pub fn pop(&mut self) -> Option<Unit> {
        self.control
            .pop_front()
            .or_else(|| self.audio.pop_front())
            .or_else(|| self.frames.pop_front())
            .or_else(|| self.data.pop_front())
    }

    #[cfg(test)]
    pub fn max_layer(&self) -> u8 {
        self.max_layer
    }
}

/// Drops the oldest unit when full: a newer one is worth more to a live stream.
fn push_bounded(queue: &mut VecDeque<Unit>, capacity: usize, unit: Unit) {
    if queue.len() >= capacity {
        queue.pop_front();
    }
    queue.push_back(unit);
}

/// A policy shared between the publisher's reader and the subscriber's writer.
#[derive(Default)]
pub struct Queue {
    policy: Mutex<Policy>,
    ready: Notify,
}

impl Queue {
    pub fn push(&self, unit: Unit, now: Instant) -> Pushed {
        let pushed = lock(&self.policy).push(unit, now);
        self.ready.notify_one();
        pushed
    }

    pub fn with<T>(&self, change: impl FnOnce(&mut Policy) -> T) -> T {
        change(&mut lock(&self.policy))
    }

    /// The next unit to send, waiting for one.
    pub async fn next(&self) -> Unit {
        loop {
            if let Some(unit) = lock(&self.policy).pop() {
                return unit;
            }
            self.ready.notified().await;
        }
    }
}

/// A plain bounded queue (the publisher's control units), dropping the oldest when full.
pub struct ControlQueue {
    units: Mutex<VecDeque<Unit>>,
    capacity: usize,
    ready: Notify,
}

impl ControlQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            units: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
            ready: Notify::new(),
        }
    }

    pub fn push(&self, unit: Unit) {
        push_bounded(&mut lock(&self.units), self.capacity, unit);
        self.ready.notify_one();
    }

    #[cfg(test)]
    pub fn try_pop(&self) -> Option<Unit> {
        lock(&self.units).pop_front()
    }

    pub async fn next(&self) -> Unit {
        loop {
            if let Some(unit) = lock(&self.units).pop_front() {
                return unit;
            }
            self.ready.notified().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::super::control::{Header, VideoCodec};
    use super::*;

    fn video(keyframe: bool, temporal_layer: u8) -> Unit {
        Unit {
            header: Header {
                content: Content::Video {
                    codec: VideoCodec::Av1,
                    keyframe,
                    temporal_layer,
                },
                key_id: 0,
                sequence: 0,
                payload_len: 1,
            },
            bytes: Bytes::new(),
        }
    }

    fn audio() -> Unit {
        Unit {
            header: Header {
                content: Content::Audio,
                key_id: 0,
                sequence: 0,
                payload_len: 1,
            },
            bytes: Bytes::new(),
        }
    }

    fn data(sequence: u32) -> Unit {
        Unit {
            header: Header {
                content: Content::Data,
                key_id: 0,
                sequence,
                payload_len: 1,
            },
            bytes: Bytes::new(),
        }
    }

    /// The L1T3 layer of the `n`th frame after a keyframe.
    fn layer(n: usize) -> u8 {
        match n % 4 {
            0 => 0,
            2 => 1,
            _ => 2,
        }
    }

    #[test]
    fn waits_for_a_keyframe_then_keeps_up() {
        let mut policy = Policy::new();
        let now = Instant::now();
        assert_eq!(policy.push(video(false, 0), now), Pushed::default());
        assert!(policy.pop().is_none());
        policy.push(video(true, 0), now);
        policy.push(video(false, 2), now);
        assert!(policy.pop().is_some() && policy.pop().is_some());
        assert_eq!(policy.max_layer(), MAX_TEMPORAL_LAYER);
    }

    #[test]
    fn a_backlog_drops_the_top_layer_first_and_raises_it_later() {
        let mut policy = Policy::new();
        let start = Instant::now();
        policy.push(video(true, 0), start);
        for n in 1..4 {
            policy.push(video(false, layer(n)), start);
        }
        assert_eq!(policy.max_layer(), 2);
        // The pause keeps the next burst from costing another layer at once.
        policy.push(video(false, 0), start);
        assert_eq!(policy.max_layer(), 2);
        while policy.pop().is_some() {}
        let later = start + RAISE_AFTER;
        policy.push(video(false, 0), start + Duration::from_secs(1));
        policy.pop();
        policy.push(video(false, 0), later + Duration::from_secs(1));
        assert_eq!(policy.max_layer(), MAX_TEMPORAL_LAYER);
    }

    #[test]
    fn overflow_clears_and_asks_for_a_keyframe() {
        let mut policy = Policy::new();
        let now = Instant::now();
        policy.push(video(true, 0), now);
        let mut asked = false;
        for _ in 0..CAPACITY {
            asked |= policy.push(video(false, 0), now).need_keyframe;
        }
        assert!(asked);
        assert_eq!(policy.max_layer(), 0);
        assert!(policy.pop().is_none());
        policy.push(video(false, 0), now);
        assert!(policy.pop().is_none(), "waits for the next keyframe");
        policy.push(video(true, 0), now);
        assert!(policy.pop().is_some());
    }

    #[test]
    fn wanted_layer_caps_what_is_sent() {
        let mut policy = Policy::new();
        let now = Instant::now();
        policy.set_wanted_layer(0);
        policy.push(video(true, 0), now);
        policy.push(video(false, 2), now);
        policy.push(video(false, 1), now);
        policy.push(video(false, 0), now);
        assert_eq!(policy.pop().map(|u| u.header), Some(video(true, 0).header));
        assert_eq!(policy.pop().map(|u| u.header), Some(video(false, 0).header));
        assert!(policy.pop().is_none());
        policy.set_wanted_layer(9);
        policy.push(video(false, 3), now);
        assert!(policy.pop().is_some());
    }

    #[test]
    fn control_then_audio_then_video_and_audio_is_bounded() {
        let mut policy = Policy::new();
        let now = Instant::now();
        policy.push(video(true, 0), now);
        for _ in 0..AUDIO_CAPACITY + 5 {
            policy.push(audio(), now);
        }
        policy.push(Unit::key(b"k"), now);
        assert!(matches!(
            policy.pop().map(|u| u.header.content),
            Some(Content::Control(_))
        ));
        for _ in 0..AUDIO_CAPACITY {
            assert_eq!(policy.pop().map(|u| u.header.content), Some(Content::Audio));
        }
        assert!(matches!(
            policy.pop().map(|u| u.header.content),
            Some(Content::Video { .. })
        ));
    }

    #[test]
    fn resync_waits_for_the_next_keyframe() {
        let mut policy = Policy::new();
        let now = Instant::now();
        policy.push(video(true, 0), now);
        policy.resync();
        policy.push(video(false, 0), now);
        assert!(policy.pop().is_some(), "frames before the loss still play");
        assert!(policy.pop().is_none());
    }

    #[test]
    fn data_goes_out_last_needs_no_keyframe_and_is_bounded() {
        let mut policy = Policy::new();
        let now = Instant::now();
        for sequence in 0..u32::try_from(DATA_CAPACITY).unwrap() + 3 {
            assert_eq!(policy.push(data(sequence), now), Pushed::default());
        }
        policy.push(video(true, 0), now);
        policy.push(audio(), now);
        policy.push(Unit::key(b"k"), now);
        let order: Vec<_> = (0..3)
            .filter_map(|_| policy.pop())
            .map(|u| u.header.content)
            .collect();
        assert!(matches!(
            order[..],
            [Content::Control(_), Content::Audio, Content::Video { .. }]
        ));
        // The oldest data units were dropped; the rest leave in order.
        for sequence in 3..u32::try_from(DATA_CAPACITY).unwrap() + 3 {
            assert_eq!(policy.pop().map(|u| u.header.sequence), Some(sequence));
        }
        assert!(policy.pop().is_none());
    }
}
