use std::cmp::Reverse;
use std::collections::BTreeMap;

use chrono::Utc;
use serde::Serialize;

pub const PRICE_TICK_SCALE: f64 = 1_000_000.0;
const MAX_ACCEPTED_PRICE: f64 = 1.0;
type BestLevel = Option<(u32, f64)>;

/// What the venue will accept for a market: the price grid and the smallest
/// order. Polymarket reports both on `/markets/{id}` (`minimum_tick_size`,
/// `minimum_order_size`) and the tick again on every WS `book` snapshot,
/// because it changes as a market nears resolution. The maker used to quote
/// on a hard-coded 0.001 grid at `$1 / price` shares; every one of its
/// fills was an order the venue would have refused on both counts.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct VenueRules {
    pub tick_size: f64,
    pub min_order_size: f64,
}

impl Default for VenueRules {
    /// The venue's own defaults for these markets, used until a market
    /// reports its own. Assuming a finer grid or a smaller minimum than the
    /// venue's would be the optimistic direction.
    fn default() -> Self {
        Self {
            tick_size: 0.01,
            min_order_size: 5.0,
        }
    }
}

impl VenueRules {
    pub fn round_to_tick(&self, px: f64) -> f64 {
        let tick = if self.tick_size > 0.0 {
            self.tick_size
        } else {
            0.01
        };
        // Snap, then re-quantise through the tick scale so 0.59 is 0.59 and
        // not 0.5900000000000001.
        let snapped = (px / tick).round() * tick;
        (snapped * PRICE_TICK_SCALE).round() / PRICE_TICK_SCALE
    }

    pub fn from_market_json(v: &serde_json::Value) -> Option<Self> {
        let tick_size = v
            .get("minimum_tick_size")
            .or_else(|| v.get("tick_size"))
            .and_then(crate::json_util::parse_f64)
            .filter(|t| *t > 0.0 && *t < 1.0)?;
        let min_order_size = v
            .get("minimum_order_size")
            .or_else(|| v.get("min_order_size"))
            .and_then(crate::json_util::parse_f64)
            .filter(|m| *m >= 0.0)?;
        Some(Self {
            tick_size,
            min_order_size,
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum BookSide {
    Bid,
    Ask,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TopOfBook {
    pub market_id: String,
    pub token_id: String,
    pub is_yes_token: Option<bool>,
    pub best_bid: Option<f64>,
    pub best_bid_size: Option<f64>,
    pub best_ask: Option<f64>,
    pub best_ask_size: Option<f64>,
    pub spread_bps: Option<f64>,
    pub sequence: Option<u64>,
    pub snapshot_loaded: bool,
    pub stale: bool,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplyResult {
    pub bbo_changed: bool,
    pub gap_detected: bool,
}

#[derive(Debug, Default)]
pub struct LocalL2Book {
    bids: BTreeMap<Reverse<u32>, f64>,
    asks: BTreeMap<u32, f64>,
    last_sequence: Option<u64>,
    snapshot_loaded: bool,
    stale: bool,
}

impl LocalL2Book {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot_loaded(&self) -> bool {
        self.snapshot_loaded
    }

    pub fn stale(&self) -> bool {
        self.stale
    }

    pub fn mark_stale(&mut self) {
        self.stale = true;
    }

    pub fn clear(&mut self) {
        self.bids.clear();
        self.asks.clear();
        self.snapshot_loaded = false;
        self.last_sequence = None;
        self.stale = true;
    }

    pub fn apply_snapshot(
        &mut self,
        bids: &[(u32, f64)],
        asks: &[(u32, f64)],
        sequence: Option<u64>,
    ) -> ApplyResult {
        let before = self.bbo_ticks();
        self.bids.clear();
        self.asks.clear();

        for &(price_ticks, size) in bids {
            if size.is_finite() && size > 0.0 && ticks_to_price(price_ticks) <= MAX_ACCEPTED_PRICE {
                self.bids.insert(Reverse(price_ticks), size);
            }
        }
        for &(price_ticks, size) in asks {
            if size.is_finite() && size > 0.0 && ticks_to_price(price_ticks) <= MAX_ACCEPTED_PRICE {
                self.asks.insert(price_ticks, size);
            }
        }

        self.last_sequence = sequence;
        self.snapshot_loaded = true;
        self.stale = false;
        let after = self.bbo_ticks();
        ApplyResult {
            bbo_changed: before != after,
            gap_detected: false,
        }
    }

    pub fn apply_delta(
        &mut self,
        side: BookSide,
        price_ticks: u32,
        size: f64,
        sequence: Option<u64>,
    ) -> ApplyResult {
        if !size.is_finite() || ticks_to_price(price_ticks) > MAX_ACCEPTED_PRICE {
            return ApplyResult {
                bbo_changed: false,
                gap_detected: false,
            };
        }
        if !self.snapshot_loaded {
            return ApplyResult {
                bbo_changed: false,
                gap_detected: false,
            };
        }

        if let Some(gap) = self.sequence_gap(sequence) {
            if gap {
                self.stale = true;
                self.snapshot_loaded = false;
                return ApplyResult {
                    bbo_changed: false,
                    gap_detected: true,
                };
            }
        }

        let before = self.bbo_ticks();
        match side {
            BookSide::Bid => {
                if size <= 0.0 {
                    self.bids.remove(&Reverse(price_ticks));
                } else {
                    self.bids.insert(Reverse(price_ticks), size);
                }
            }
            BookSide::Ask => {
                if size <= 0.0 {
                    self.asks.remove(&price_ticks);
                } else {
                    self.asks.insert(price_ticks, size);
                }
            }
        }
        self.stale = false;
        let after = self.bbo_ticks();
        ApplyResult {
            bbo_changed: before != after,
            gap_detected: false,
        }
    }

    pub fn top_of_book(
        &self,
        market_id: String,
        token_id: String,
        is_yes_token: Option<bool>,
    ) -> TopOfBook {
        let best_bid = self
            .bids
            .iter()
            .next()
            .map(|(p, sz)| (ticks_to_price(p.0), *sz));
        let best_ask = self
            .asks
            .iter()
            .next()
            .map(|(p, sz)| (ticks_to_price(*p), *sz));
        let spread_bps = match (best_bid.as_ref(), best_ask.as_ref()) {
            (Some((bid, _)), Some((ask, _))) if *ask > 0.0 && ask >= bid => {
                Some((ask - bid) * 10_000.0)
            }
            _ => None,
        };
        TopOfBook {
            market_id,
            token_id,
            is_yes_token,
            best_bid: best_bid.as_ref().map(|(p, _)| *p),
            best_bid_size: best_bid.as_ref().map(|(_, sz)| *sz),
            best_ask: best_ask.as_ref().map(|(p, _)| *p),
            best_ask_size: best_ask.as_ref().map(|(_, sz)| *sz),
            spread_bps,
            sequence: self.last_sequence,
            snapshot_loaded: self.snapshot_loaded,
            stale: self.stale,
            updated_at: Utc::now().to_rfc3339(),
        }
    }

    fn bbo_ticks(&self) -> (BestLevel, BestLevel) {
        (
            self.bids.iter().next().map(|(p, size)| (p.0, *size)),
            self.asks.iter().next().map(|(p, size)| (*p, *size)),
        )
    }

    fn sequence_gap(&mut self, sequence: Option<u64>) -> Option<bool> {
        let seq = sequence?;
        let gap = match self.last_sequence {
            Some(prev) => seq <= prev || seq > prev.saturating_add(1),
            None => false,
        };
        self.last_sequence = Some(seq);
        Some(gap)
    }
}

pub fn price_to_ticks(price: f64) -> Option<u32> {
    if !price.is_finite() || !(0.0..=MAX_ACCEPTED_PRICE).contains(&price) {
        return None;
    }
    let ticks = (price * PRICE_TICK_SCALE).round();
    if ticks < 0.0 {
        return None;
    }
    u32::try_from(ticks as u64).ok()
}

pub fn ticks_to_price(ticks: u32) -> f64 {
    ticks as f64 / PRICE_TICK_SCALE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_then_delta_updates_bbo() {
        let mut book = LocalL2Book::new();
        let r = book.apply_snapshot(
            &[(500_000, 100.0), (490_000, 50.0)],
            &[(510_000, 90.0), (520_000, 20.0)],
            Some(10),
        );
        assert!(r.bbo_changed);
        let top = book.top_of_book("m".into(), "t".into(), Some(true));
        assert_eq!(top.best_bid, Some(0.5));
        assert_eq!(top.best_ask, Some(0.51));
        assert_eq!(top.is_yes_token, Some(true));

        let r2 = book.apply_delta(BookSide::Bid, 505_000, 10.0, Some(11));
        assert!(r2.bbo_changed);
        let top2 = book.top_of_book("m".into(), "t".into(), Some(true));
        assert_eq!(top2.best_bid, Some(0.505));
    }

    #[test]
    fn zero_size_removes_price_level() {
        let mut book = LocalL2Book::new();
        book.apply_snapshot(&[(500_000, 100.0)], &[(510_000, 100.0)], Some(1));
        let r = book.apply_delta(BookSide::Ask, 510_000, 0.0, Some(2));
        assert!(r.bbo_changed);
        let top = book.top_of_book("m".into(), "t".into(), Some(false));
        assert_eq!(top.best_ask, None);
        assert_eq!(top.is_yes_token, Some(false));
    }

    #[test]
    fn size_changes_at_the_same_best_price_are_published() {
        let mut book = LocalL2Book::new();
        book.apply_snapshot(&[(500_000, 100.0)], &[(510_000, 100.0)], Some(1));
        assert!(
            book.apply_delta(BookSide::Bid, 500_000, 5.0, Some(2))
                .bbo_changed
        );
        let top = book.top_of_book("m".into(), "t".into(), Some(true));
        assert_eq!(top.best_bid, Some(0.5));
        assert_eq!(top.best_bid_size, Some(5.0));
        assert!(
            !book
                .apply_delta(BookSide::Bid, 500_000, 5.0, Some(3))
                .bbo_changed
        );
    }

    #[test]
    fn sequence_gap_marks_book_stale() {
        let mut book = LocalL2Book::new();
        book.apply_snapshot(&[(500_000, 100.0)], &[(510_000, 100.0)], Some(50));
        let r = book.apply_delta(BookSide::Bid, 499_000, 10.0, Some(53));
        assert!(r.gap_detected);
        assert!(book.stale());
        assert!(!book.snapshot_loaded());
    }

    #[test]
    fn deterministic_replay_produces_stable_bbo_sequence() {
        let run_once = || {
            let mut book = LocalL2Book::new();
            let mut out = Vec::new();
            let _ = book.apply_snapshot(
                &[(500_000, 100.0), (499_000, 50.0)],
                &[(510_000, 120.0), (511_000, 80.0)],
                Some(100),
            );
            let t0 = book.top_of_book("m".into(), "t".into(), Some(true));
            out.push((t0.best_bid, t0.best_ask));

            let _ = book.apply_delta(BookSide::Bid, 501_000, 90.0, Some(101));
            let t1 = book.top_of_book("m".into(), "t".into(), Some(true));
            out.push((t1.best_bid, t1.best_ask));

            let _ = book.apply_delta(BookSide::Ask, 510_000, 0.0, Some(102));
            let t2 = book.top_of_book("m".into(), "t".into(), Some(true));
            out.push((t2.best_bid, t2.best_ask));

            let _ = book.apply_delta(BookSide::Ask, 508_000, 70.0, Some(103));
            let t3 = book.top_of_book("m".into(), "t".into(), Some(true));
            out.push((t3.best_bid, t3.best_ask));
            out
        };

        let first = run_once();
        let second = run_once();
        assert_eq!(first, second);
        assert_eq!(
            first,
            vec![
                (Some(0.5), Some(0.51)),
                (Some(0.501), Some(0.51)),
                (Some(0.501), Some(0.511)),
                (Some(0.501), Some(0.508))
            ]
        );
    }
}
