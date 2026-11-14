//! Suppression of OpenDeck's "pressed" key feedback.
//!
//! OpenDeck re-renders a key every time it is pressed *and* released (while pressed
//! the icon is drawn 10% smaller) and sends each render as a `SetImage`. The D200 can
//! only be updated by uploading the whole ZIP, so every click would cost two or three
//! full uploads for nothing.
//!
//! A frame that arrives for a key that is currently held is therefore *withheld*
//! instead of staged. It is dropped when the post-release render for the same key
//! arrives (that render carries the real state, whatever happened during the press).
//! Two situations show the withheld frame is a genuine update and not feedback, and
//! it is then staged after all:
//!  * frames for other keys arrive close to it (a profile switch re-renders every key,
//!    including the one that was pressed to trigger it), or
//!  * no post-release frame shows up within `RELEASE_GRACE` of the release.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Wait this long after a release for OpenDeck's re-render before trusting a withheld frame.
const RELEASE_GRACE: Duration = Duration::from_millis(150);
/// Frames for different keys closer together than this belong to the same burst.
const BURST_WINDOW: Duration = Duration::from_millis(100);
/// A key held longer than this is assumed to have lost its release event.
const MAX_HOLD: Duration = Duration::from_secs(30);

/// `(device id, position, base64 image)` that should be staged.
pub type Frame = (String, usize, String);

type KeyId = (String, usize);

struct Withheld {
    image: String,
    released_at: Option<Instant>,
}

pub struct PressFilter {
    enabled: bool,
    held: HashMap<KeyId, Instant>,
    withheld: HashMap<KeyId, Withheld>,
    last_set: Option<(KeyId, Instant)>,
}

impl PressFilter {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            held: HashMap::new(),
            withheld: HashMap::new(),
            last_set: None,
        }
    }

    pub fn key_down(&mut self, device: &str, position: usize, now: Instant) {
        self.held.insert((device.to_string(), position), now);
    }

    pub fn key_up(&mut self, device: &str, position: usize, now: Instant) {
        let key = (device.to_string(), position);
        self.held.remove(&key);
        if let Some(w) = self.withheld.get_mut(&key) {
            w.released_at = Some(now);
        }
    }

    /// A `SetImage` arrived. Returns the frames to stage now (possibly none).
    pub fn on_set_image(
        &mut self,
        device: &str,
        position: usize,
        image: String,
        now: Instant,
    ) -> Vec<Frame> {
        if !self.enabled {
            return vec![(device.to_string(), position, image)];
        }
        let key = (device.to_string(), position);
        let mut out = self.commit_others(device, position);

        let burst = matches!(&self.last_set, Some(((d, p), t))
            if d == device && *p != position && now.duration_since(*t) < BURST_WINDOW);
        self.last_set = Some((key.clone(), now));

        let held = match self.held.get(&key).copied() {
            Some(since) if now.duration_since(since) < MAX_HOLD => true,
            Some(_) => {
                self.held.remove(&key);
                false
            }
            None => false,
        };

        if held && !burst {
            self.withheld.insert(key, Withheld { image, released_at: None });
        } else {
            // Either a normal frame, or the post-release render that supersedes
            // whatever was withheld for this key.
            self.withheld.remove(&key);
            out.push((device.to_string(), position, image));
        }
        out
    }

    /// A `ClearImage` arrived. It always passes through, but supersedes a withheld frame
    /// and counts as burst evidence for the others. Returns frames to stage now.
    pub fn on_clear(&mut self, device: &str, position: usize) -> Vec<Frame> {
        if !self.enabled {
            return Vec::new();
        }
        self.withheld.remove(&(device.to_string(), position));
        self.commit_others(device, position)
    }

    /// Stage withheld frames whose key was released without a follow-up render.
    pub fn expire(&mut self, now: Instant) -> Vec<Frame> {
        let due: Vec<KeyId> = self
            .withheld
            .iter()
            .filter(|(_, w)| matches!(w.released_at, Some(t) if now.duration_since(t) >= RELEASE_GRACE))
            .map(|(k, _)| k.clone())
            .collect();
        due.into_iter()
            .filter_map(|k| self.withheld.remove(&k).map(|w| (k.0, k.1, w.image)))
            .collect()
    }

    /// Frames for other keys of `device` mean a burst: stage everything withheld for them.
    fn commit_others(&mut self, device: &str, position: usize) -> Vec<Frame> {
        let keys: Vec<KeyId> = self
            .withheld
            .keys()
            .filter(|(d, p)| d == device && *p != position)
            .cloned()
            .collect();
        keys.into_iter()
            .filter_map(|k| self.withheld.remove(&k).map(|w| (k.0, k.1, w.image)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEV: &str = "e9-test";

    fn frames(v: &[Frame]) -> Vec<(usize, &str)> {
        v.iter().map(|(_, p, i)| (*p, i.as_str())).collect()
    }

    #[test]
    fn frame_for_idle_key_is_staged() {
        let mut f = PressFilter::new(true);
        let out = f.on_set_image(DEV, 3, "a".into(), Instant::now());
        assert_eq!(frames(&out), vec![(3, "a")]);
    }

    #[test]
    fn pressed_frame_is_withheld_and_release_frame_supersedes_it() {
        let mut f = PressFilter::new(true);
        let t0 = Instant::now();
        f.key_down(DEV, 3, t0);
        let out = f.on_set_image(DEV, 3, "shrunk".into(), t0 + Duration::from_millis(15));
        assert!(out.is_empty());

        f.key_up(DEV, 3, t0 + Duration::from_millis(90));
        let out = f.on_set_image(DEV, 3, "full".into(), t0 + Duration::from_millis(105));
        assert_eq!(frames(&out), vec![(3, "full")]);
        // Nothing left to expire later: the withheld frame is gone for good.
        assert!(f.expire(t0 + Duration::from_secs(5)).is_empty());
    }

    #[test]
    fn withheld_frame_is_staged_if_no_release_frame_arrives() {
        let mut f = PressFilter::new(true);
        let t0 = Instant::now();
        f.key_down(DEV, 3, t0);
        assert!(f.on_set_image(DEV, 3, "genuine".into(), t0).is_empty());
        f.key_up(DEV, 3, t0 + Duration::from_millis(80));

        assert!(f.expire(t0 + Duration::from_millis(100)).is_empty()); // still in grace
        let out = f.expire(t0 + Duration::from_millis(80) + RELEASE_GRACE);
        assert_eq!(frames(&out), vec![(3, "genuine")]);
        assert!(f.expire(t0 + Duration::from_secs(5)).is_empty());
    }

    #[test]
    fn profile_switch_keeps_frame_of_the_held_key_when_it_comes_first() {
        let mut f = PressFilter::new(true);
        let t0 = Instant::now();
        f.key_down(DEV, 0, t0);
        assert!(f.on_set_image(DEV, 0, "new0".into(), t0).is_empty());
        // The next key's frame reveals the burst and releases the withheld one.
        let out = f.on_set_image(DEV, 1, "new1".into(), t0 + Duration::from_millis(5));
        assert_eq!(frames(&out), vec![(0, "new0"), (1, "new1")]);
    }

    #[test]
    fn profile_switch_keeps_frame_of_the_held_key_when_it_comes_last() {
        let mut f = PressFilter::new(true);
        let t0 = Instant::now();
        f.key_down(DEV, 5, t0);
        assert_eq!(f.on_set_image(DEV, 4, "new4".into(), t0).len(), 1);
        let out = f.on_set_image(DEV, 5, "new5".into(), t0 + Duration::from_millis(5));
        assert_eq!(frames(&out), vec![(5, "new5")]);
    }

    #[test]
    fn clear_of_other_key_releases_withheld_frame_and_own_clear_drops_it() {
        let mut f = PressFilter::new(true);
        let t0 = Instant::now();
        f.key_down(DEV, 2, t0);
        assert!(f.on_set_image(DEV, 2, "x".into(), t0).is_empty());
        assert_eq!(frames(&f.on_clear(DEV, 7)), vec![(2, "x")]);

        assert!(f.on_set_image(DEV, 2, "y".into(), t0 + Duration::from_secs(1)).is_empty());
        assert!(f.on_clear(DEV, 2).is_empty());
        f.key_up(DEV, 2, t0 + Duration::from_secs(2));
        assert!(f.expire(t0 + Duration::from_secs(9)).is_empty());
    }

    #[test]
    fn stale_hold_is_ignored() {
        let mut f = PressFilter::new(true);
        let t0 = Instant::now();
        f.key_down(DEV, 3, t0);
        let out = f.on_set_image(DEV, 3, "a".into(), t0 + MAX_HOLD + Duration::from_secs(1));
        assert_eq!(frames(&out), vec![(3, "a")]);
    }

    #[test]
    fn held_state_is_per_device_and_position() {
        let mut f = PressFilter::new(true);
        let t0 = Instant::now();
        f.key_down(DEV, 3, t0);
        assert_eq!(f.on_set_image("other", 3, "a".into(), t0).len(), 1);
        assert_eq!(f.on_set_image(DEV, 4, "b".into(), t0 + Duration::from_secs(1)).len(), 1);
    }

    #[test]
    fn disabled_filter_passes_everything_through() {
        let mut f = PressFilter::new(false);
        let t0 = Instant::now();
        f.key_down(DEV, 3, t0);
        assert_eq!(frames(&f.on_set_image(DEV, 3, "a".into(), t0)), vec![(3, "a")]);
        assert!(f.on_clear(DEV, 3).is_empty());
        assert!(f.expire(t0 + Duration::from_secs(1)).is_empty());
    }
}
