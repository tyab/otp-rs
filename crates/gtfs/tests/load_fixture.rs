//! ヘルメティックな小フィクスチャ (`fixtures/mini/`) で `Feed::load_from_dir` を検証する。
//!
//! フィクスチャは以下の CSV エッジケースを意図的に含む:
//! - `stops.txt`: 先頭 UTF-8 BOM
//! - `routes.txt`: ダブルクォート引用フィールド (内部にカンマ + `""` エスケープ)
//! - `calendar_dates.txt`: CRLF 改行
//! - `stop_times.txt`: 仕様上 optional な列 (trip_headsign 等) を省略 (列不足の許容を検証)

use std::collections::HashSet;
use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini")
}

#[test]
fn loads_all_tables_with_expected_counts() {
    let feed = otp_gtfs::Feed::load_from_dir(&fixture_dir()).expect("fixture should load");

    assert_eq!(feed.stops.len(), 6, "stops.txt: A,B,C1,C2,C,D");
    assert_eq!(feed.routes.len(), 3);
    assert_eq!(feed.trips.len(), 5);
    assert_eq!(feed.stop_times.len(), 12);
    assert_eq!(feed.calendars.len(), 1);
    assert_eq!(feed.calendar_dates.len(), 2);
    assert_eq!(feed.fare_attributes.len(), 1);
    assert_eq!(feed.fare_rules.len(), 1);
}

#[test]
fn stop_times_reference_existing_trips_and_stops() {
    let feed = otp_gtfs::Feed::load_from_dir(&fixture_dir()).expect("fixture should load");

    let trip_ids: HashSet<_> = feed.trips.iter().map(|t| t.id.as_str()).collect();
    let stop_ids: HashSet<_> = feed.stops.iter().map(|s| s.id.as_str()).collect();

    for st in &feed.stop_times {
        assert!(trip_ids.contains(st.trip_id.as_str()), "unknown trip_id {}", st.trip_id);
        assert!(stop_ids.contains(st.stop_id.as_str()), "unknown stop_id {}", st.stop_id);
    }
}

#[test]
fn bom_prefixed_stop_name_parses_correctly() {
    let feed = otp_gtfs::Feed::load_from_dir(&fixture_dir()).expect("fixture should load");
    let a = feed.stops.iter().find(|s| s.id.as_str() == "A").expect("stop A");
    assert_eq!(a.name, "A駅");
    assert!((a.lat - 35.00).abs() < 1e-9);
}

#[test]
fn quoted_route_long_name_with_comma_and_escaped_quote() {
    let feed = otp_gtfs::Feed::load_from_dir(&fixture_dir()).expect("fixture should load");
    let r1 = feed.routes.iter().find(|r| r.id.as_str() == "R1").expect("route R1");
    assert_eq!(r1.long_name, "Rapid \"Local\" Line, North");
}

#[test]
fn parent_station_groups_platforms() {
    let feed = otp_gtfs::Feed::load_from_dir(&fixture_dir()).expect("fixture should load");
    let c1 = feed.stops.iter().find(|s| s.id.as_str() == "C1").unwrap();
    let c2 = feed.stops.iter().find(|s| s.id.as_str() == "C2").unwrap();
    assert_eq!(c1.parent_station.as_ref().unwrap().as_str(), "C");
    assert_eq!(c2.parent_station.as_ref().unwrap().as_str(), "C");
}

#[test]
fn calendar_dates_crlf_parses_added_and_removed_exceptions() {
    let feed = otp_gtfs::Feed::load_from_dir(&fixture_dir()).expect("fixture should load");
    let added = feed.calendar_dates.iter().find(|d| d.service_id.as_str() == "WD_EXTRA").unwrap();
    assert_eq!(added.date, 20260713);
    assert!(added.added);
    let removed = feed.calendar_dates.iter().find(|d| d.service_id.as_str() == "WD").unwrap();
    assert_eq!(removed.date, 20260720);
    assert!(!removed.added);
}

#[test]
fn missing_optional_stop_times_columns_are_tolerated() {
    // stop_times.txt はフィクスチャで trip_headsign 等の optional 列を省略している。
    // それでも arrival/departure と stop_sequence が正しく読めることを確認する。
    let feed = otp_gtfs::Feed::load_from_dir(&fixture_dir()).expect("fixture should load");
    let st = feed
        .stop_times
        .iter()
        .find(|s| s.trip_id.as_str() == "T1" && s.stop_sequence == 1)
        .expect("T1 first stop_time");
    assert_eq!(st.arrival, 8 * 3600);
    assert_eq!(st.departure, 8 * 3600);
    assert_eq!(st.stop_id.as_str(), "A");
}

#[test]
fn missing_gtfs_files_yield_empty_vecs_not_errors() {
    // fare_attributes.txt / fare_rules.txt が存在しないディレクトリでも
    // load_from_dir はエラーにせず空 Vec で返す。
    let dir = tempdir_with_only_stops();
    let feed = otp_gtfs::Feed::load_from_dir(&dir).expect("partial feed should still load");
    assert!(feed.stops.is_empty()); // stops.txt も無いので空
    assert!(feed.fare_attributes.is_empty());
    assert!(feed.calendars.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

fn tempdir_with_only_stops() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("otp-rs-empty-feed-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
