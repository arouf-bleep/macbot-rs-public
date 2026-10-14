use serde_json::Value;

/// Parse a JSON value as `f64`, rejecting non-finite results (NaN, ±Infinity).
pub fn parse_f64(v: &Value) -> Option<f64> {
    let raw = v
        .as_f64()
        .or_else(|| v.as_i64().map(|x| x as f64))
        .or_else(|| v.as_u64().map(|x| x as f64))
        .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))?;
    if raw.is_finite() {
        Some(raw)
    } else {
        None
    }
}

/// Parse a JSON value as a market price, enforcing `(0.0, 1.0]` range
/// and finiteness.  Returns `None` for values outside the valid Polymarket
/// price domain.
pub fn parse_market_price(v: &Value) -> Option<f64> {
    let p = parse_f64(v)?;
    if p > 0.0 && p <= 1.0 {
        Some(p)
    } else {
        None
    }
}

pub fn extract_levels(book: &Value, side: &str) -> Vec<(f64, f64)> {
    let Some(arr) = book.get(side).and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(arr.len());
    for level in arr {
        let price = match level
            .get("price")
            .or_else(|| level.get("p"))
            .and_then(parse_market_price)
        {
            Some(p) => p,
            _ => continue,
        };
        let size = match level
            .get("size")
            .or_else(|| level.get("s"))
            .and_then(parse_f64)
        {
            Some(s) if s > 0.0 => s,
            _ => continue,
        };
        out.push((price, size));
    }
    // The CLOB REST book lists each side worst-first — bids ascending from
    // 0.01, asks descending from 0.99 — and every consumer of this takes
    // `.first()` or walks from the front expecting the best price. Read that
    // way, a book with a one-cent spread reports 0.98, so the taker path
    // rejected every entry it ever attempted as a thin book, and the fill
    // simulator broke on the first ask before filling anything. Sort here,
    // once, and never trust the wire order.
    if side == "bids" {
        out.sort_by(|a, b| b.0.total_cmp(&a.0));
    } else {
        out.sort_by(|a, b| a.0.total_cmp(&b.0));
    }
    out
}

/// The midpoint of the book that was actually fetched, when both sides exist.
///
/// The entry and close paths used to take `mid` from a second REST call to
/// `/midpoint`, made a moment after `/book`. On a fast-moving market the two
/// catch different instants: one live entry filled at 0.38 from a book whose
/// `mid` had been reported as 0.425 a moment earlier, and the "slippage" of
/// -1059 bps recorded against it was the gap between two snapshots, not a fill
/// quality. One snapshot, one truth. Returns None when a side is empty, since
/// a midpoint against a defaulted 0.50 is not a midpoint.
pub fn book_mid(book: &Value) -> Option<f64> {
    let bids = extract_levels(book, "bids");
    let asks = extract_levels(book, "asks");
    match (bids.first(), asks.first()) {
        (Some((bid, _)), Some((ask, _))) => Some((bid + ask) / 2.0),
        _ => None,
    }
}

#[cfg(test)]
mod level_order_tests {
    use super::extract_levels;
    use serde_json::json;

    /// The shape the CLOB actually sends, captured from a live BTC 15m book:
    /// bids listed from 0.01 upward, asks from 0.99 downward.
    fn live_shaped_book() -> serde_json::Value {
        json!({
            "bids": [
                {"price": "0.01", "size": "5000"},
                {"price": "0.30", "size": "200"},
                {"price": "0.50", "size": "40"}
            ],
            "asks": [
                {"price": "0.99", "size": "5000"},
                {"price": "0.70", "size": "200"},
                {"price": "0.51", "size": "40"}
            ]
        })
    }

    #[test]
    fn levels_come_back_best_first_whatever_order_the_wire_used() {
        let book = live_shaped_book();
        let bids = extract_levels(&book, "bids");
        let asks = extract_levels(&book, "asks");
        assert_eq!(
            bids[0].0, 0.50,
            "best bid is the highest, not the first listed"
        );
        assert_eq!(
            asks[0].0, 0.51,
            "best ask is the lowest, not the first listed"
        );
        assert!(
            (asks[0].0 - bids[0].0 - 0.01).abs() < 1e-9,
            "this book's spread is one cent; read from the wrong end it is 0.98"
        );
    }

    #[test]
    fn the_mid_comes_from_the_book_it_was_read_with() {
        let book = live_shaped_book();
        let mid = extract_mid(&book);
        assert!(
            (mid - 0.505).abs() < 1e-9,
            "mid of best 0.50 / 0.51, not of 0.01 / 0.99"
        );
    }

    #[test]
    fn a_one_sided_book_has_no_midpoint() {
        let book = json!({"bids": [{"price": "0.40", "size": "1"}]});
        assert_eq!(super::book_mid(&book), None);
    }

    fn extract_mid(book: &serde_json::Value) -> f64 {
        super::book_mid(book).expect("both sides present")
    }

    #[test]
    fn an_already_sorted_book_is_left_alone() {
        let book = json!({
            "bids": [{"price": "0.45", "size": "1"}, {"price": "0.44", "size": "1"}],
            "asks": [{"price": "0.55", "size": "1"}, {"price": "0.56", "size": "1"}]
        });
        assert_eq!(extract_levels(&book, "bids")[0].0, 0.45);
        assert_eq!(extract_levels(&book, "asks")[0].0, 0.55);
    }
}
