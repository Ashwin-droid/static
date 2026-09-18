//! Per-song WRA checkpoints in media time. Seeking never consumes the history.
use std::collections::BTreeMap;

const INITIAL_SPACING_NS: u64 = 100_000_000;
const MAX_POINTS: usize = 65_536;

pub(super) struct GainHistory {
    points: BTreeMap<u64, Option<f64>>,
    tail: Option<(u64, Option<f64>)>,
    spacing: u64,
}
impl Default for GainHistory {
    fn default() -> Self {
        Self {
            points: BTreeMap::new(),
            tail: None,
            spacing: INITIAL_SPACING_NS,
        }
    }
}
impl GainHistory {
    pub fn clear(&mut self) {
        *self = Self::default();
    }
    pub fn finish_span(&mut self) {
        if let Some((time, value)) = self.tail.take() {
            self.points.insert(time, value);
        }
        self.compact();
    }
    /// Preserve the boundary of a newly visited span, including an explicit None.
    pub fn start_span(&mut self, time: u64, value: Option<f64>) {
        self.finish_span();
        self.points.entry(time).or_insert(value);
        self.tail = Some((time, value));
        self.compact();
    }
    pub fn record(&mut self, time: u64, value: Option<f64>) {
        let last = self
            .points
            .range(..=time)
            .next_back()
            .map(|(&time, _)| time);
        if last.is_none_or(|last| time.saturating_sub(last) >= self.spacing) {
            self.points.entry(time).or_insert(value);
        }
        self.tail = Some((time, value));
        self.compact();
    }
    pub fn before(&self, time: u64) -> Option<Option<f64>> {
        self.points
            .range(..=time)
            .next_back()
            .map(|(_, value)| *value)
    }
    /// Only interpolate within recorded spans, never across a forward-seek gap.
    pub fn at(&self, time: u64) -> Option<Option<f64>> {
        let (&a, &left) = self.points.range(..=time).next_back()?;
        if a == time {
            return Some(left);
        }
        let (&b, &right) = self.points.range(time..).next()?;
        if b - a > self.spacing.saturating_mul(2) {
            return None;
        }
        match (left, right) {
            (Some(left), Some(right)) => Some(Some(
                left + (right - left) * (time - a) as f64 / (b - a) as f64,
            )),
            (None, None) => Some(None),
            _ => None,
        }
    }
    fn compact(&mut self) {
        while self.points.len() > MAX_POINTS {
            self.spacing = self.spacing.saturating_mul(2);
            let mut bucket = None;
            let last = *self.points.last_key_value().unwrap().0;
            self.points.retain(|&time, _| {
                let next = time / self.spacing;
                let keep = bucket != Some(next) || time == last;
                bucket = Some(next);
                keep
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn seek_interpolation_keeps_old_points_and_does_not_bridge_unvisited_gaps() {
        let mut history = GainHistory::default();
        history.start_span(0, None);
        history.record(15_000_000_000, Some(-30.0));
        history.record(15_100_000_000, Some(-28.0));
        history.finish_span();
        assert_eq!(history.at(15_050_000_000), Some(Some(-29.0)));
        assert_eq!(history.at(10_000_000_000), None);
        history.start_span(60_000_000_000, Some(-28.0));
        history.record(60_100_000_000, Some(-20.0));
        history.finish_span();
        assert_eq!(history.at(40_000_000_000), None);
        assert_eq!(history.before(40_000_000_000), Some(Some(-28.0)));
        assert_eq!(history.at(15_050_000_000), Some(Some(-29.0)));
    }
    #[test]
    fn long_tracks_compact_resolution_instead_of_discarding_early_history() {
        let mut history = GainHistory::default();
        history.start_span(0, Some(-20.0));
        for i in 1..MAX_POINTS * 2 {
            history.record(i as u64 * INITIAL_SPACING_NS, Some(-20.0));
        }
        history.finish_span();
        assert!(history.points.len() <= MAX_POINTS);
        assert_eq!(history.at(0), Some(Some(-20.0)));
        assert_eq!(history.at(1_000_000_000), Some(Some(-20.0)));
        assert!(history.spacing > INITIAL_SPACING_NS);
    }
}
