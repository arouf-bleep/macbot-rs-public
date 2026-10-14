use super::*;
use serde_json::json;

fn row(open: i64) -> Value {
    json!([
        open,
        "100",
        "103",
        "99",
        "101",
        "20",
        open + KLINE_15M_MS - 1
    ])
}

#[tokio::test]
async fn source_validation_rejects_malformed_ohlcv_time_order_and_page_size() {
    let end = 20 * KLINE_15M_MS;
    let first = row(10 * KLINE_15M_MS);
    let second = row(11 * KLINE_15M_MS);
    let mut cases = Vec::new();
    for (field, bad) in [
        (1, json!(0)),
        (2, json!("NaN")),
        (3, json!(104)),
        (4, json!(105)),
        (5, json!(-1)),
        (6, json!(0)),
        (0, json!(10 * KLINE_15M_MS + 1)),
    ] {
        let mut changed = first.clone();
        changed[field] = bad;
        cases.push(vec![changed]);
    }
    cases.push(vec![row(7 * KLINE_15M_MS)]); // before requested horizon8
    cases.push(vec![row(21 * KLINE_15M_MS)]); // after requested end20
    cases.push(vec![second.clone(), first.clone()]); // descending
    cases.push(vec![json!(null)]);
    cases.push(vec![json!([
        10 * KLINE_15M_MS,
        "100",
        "103",
        "99",
        "101",
        "20"
    ])]); // missing close time
    cases.push((0..14).map(|_| first.clone()).collect()); // requested page limit13
    for bad in cases {
        let error =
            fetch_historical_pages(4, end, |_, _| std::future::ready(Ok(bad.clone()))).await;
        assert!(error.is_err(), "invalid source payload accepted: {bad:?}");
    }
}

#[tokio::test]
async fn identical_overlap_is_one_observation_but_changed_ohlcv_is_refused() {
    let end = 20 * KLINE_15M_MS;
    let mut source: Vec<_> = (8..20).map(|i| row(i * KLINE_15M_MS)).collect();
    let mut equivalent = source[0].clone();
    equivalent[1] = json!(100.0); // numeric spelling differs, observation does not
    source.insert(1, equivalent);
    let result = fetch_historical_pages(4, end, |_, _| std::future::ready(Ok(source.clone())))
        .await
        .unwrap();
    assert_eq!(result.len(), 4);
    assert_eq!(result.first().unwrap().0, 16 * KLINE_15M_MS);
    assert_eq!(result.last().unwrap().0, 19 * KLINE_15M_MS);
    source[1][5] = json!("21");
    let error = fetch_historical_pages(4, end, |_, _| std::future::ready(Ok(source.clone())))
        .await
        .unwrap_err();
    assert!(error.contains("conflicting"));
}

#[tokio::test]
async fn adjacent_pages_may_overlap_only_on_identical_already_observed_candles() {
    let target = 1600;
    let end = 2000 * KLINE_15M_MS;
    let horizon = end - (target + 8) as i64 * KLINE_15M_MS;
    let mut calls = 0;
    let result = fetch_historical_pages(target, end, |start, limit| {
        calls += 1;
        let begin = if calls == 2 {
            start - KLINE_15M_MS
        } else {
            start
        };
        std::future::ready(Ok((0..limit)
            .map(|i| row(begin + i as i64 * KLINE_15M_MS))
            .collect()))
    })
    .await
    .unwrap();
    assert_eq!(calls, 2);
    assert_eq!(result.len(), target);
    assert_eq!(result.last().unwrap().0, end - KLINE_15M_MS);
    assert!(result.iter().all(|(open, _)| *open >= horizon));
    calls = 0;
    let error = fetch_historical_pages(target, end, |start, limit| {
        calls += 1;
        let mut batch: Vec<_> = (0..limit)
            .map(|i| row(start + i as i64 * KLINE_15M_MS))
            .collect();
        if calls == 2 {
            let mut changed = row(start - KLINE_15M_MS);
            changed[4] = json!("102");
            batch[0] = changed;
        }
        std::future::ready(Ok(batch))
    })
    .await
    .unwrap_err();
    assert_eq!(calls, 2);
    assert!(error.contains("conflicting"));
}
