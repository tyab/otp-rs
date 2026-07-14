//! 実データ統合テスト。
//!
//! babymobi の `infra/otp/data/toei-train-gtfs.zip`（都営地下鉄 GTFS）を対象に、
//! `Feed::load_from_dir` が大規模フィードでも件数・参照整合を保って読めることを検証する。
//!
//! データの与え方は2通り:
//! - `OTP_RS_GTFS_DIR` 環境変数: 既に解凍済みのディレクトリを直接指す (CI 等向け)
//! - 既定: `otp-rs/../infra/otp/data/toei-train-gtfs.zip` (babymobi サブモジュール構成) を
//!   `unzip` コマンドで一時ディレクトリに解凍して読む。`OTP_RS_GTFS_ZIP` で zip パスを
//!   上書きできる。
//!
//! いずれの手段でもデータが見つからなければ `eprintln!` して早期 return する
//! (テストを失敗させない: rigor の「外部データ不在時は握りつぶさず正直にskip」に従う)。

use std::path::{Path, PathBuf};
use std::process::Command;

fn default_zip_path() -> PathBuf {
    // crates/gtfs から見て ../../../infra/otp/data (= otp-rs の親 = babymobi 直下)。
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../infra/otp/data/toei-train-gtfs.zip"))
}

/// 実データディレクトリを用意する。取得できなければ None (呼び出し側で skip する)。
fn prepare_data_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("OTP_RS_GTFS_DIR") {
        let path = PathBuf::from(dir);
        if path.is_dir() {
            return Some(path);
        }
        eprintln!("OTP_RS_GTFS_DIR={} は存在しないディレクトリ。skip します。", path.display());
        return None;
    }

    let zip_path = std::env::var("OTP_RS_GTFS_ZIP").map(PathBuf::from).unwrap_or_else(|_| default_zip_path());
    if !zip_path.is_file() {
        eprintln!(
            "実データ GTFS zip が見つかりません ({}). babymobi サブモジュール構成でないか、\
             infra/otp/data が未取得の可能性。このテストは skip します。",
            zip_path.display()
        );
        return None;
    }

    let out_dir = std::env::temp_dir().join("otp-rs-toei-gtfs-test");
    if !already_extracted(&out_dir) {
        std::fs::create_dir_all(&out_dir).ok()?;
        let status = Command::new("unzip").arg("-o").arg("-q").arg(&zip_path).arg("-d").arg(&out_dir).status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => {
                eprintln!("unzip が失敗しました (status={s}). skip します。");
                return None;
            }
            Err(e) => {
                eprintln!("unzip コマンドが見つかりません ({e}). skip します。");
                return None;
            }
        }
    }
    Some(out_dir)
}

fn already_extracted(dir: &Path) -> bool {
    dir.join("stops.txt").is_file() && dir.join("stop_times.txt").is_file()
}

#[test]
fn toei_feed_loads_with_expected_scale_and_referential_integrity() {
    let Some(dir) = prepare_data_dir() else { return };

    let feed = otp_gtfs::Feed::load_from_dir(&dir).expect("実データ feed のロードに失敗");

    eprintln!(
        "toei-train-gtfs: stops={} routes={} trips={} stop_times={} calendars={} calendar_dates={} \
         fare_attributes={} fare_rules={}",
        feed.stops.len(),
        feed.routes.len(),
        feed.trips.len(),
        feed.stop_times.len(),
        feed.calendars.len(),
        feed.calendar_dates.len(),
        feed.fare_attributes.len(),
        feed.fare_rules.len(),
    );

    assert!(feed.stops.len() > 100, "stops should exceed 100, got {}", feed.stops.len());
    assert!(feed.stop_times.len() > 100_000, "stop_times should exceed 100,000, got {}", feed.stop_times.len());

    use std::collections::HashSet;
    let trip_ids: HashSet<&str> = feed.trips.iter().map(|t| t.id.as_str()).collect();
    let stop_ids: HashSet<&str> = feed.stops.iter().map(|s| s.id.as_str()).collect();
    let route_ids: HashSet<&str> = feed.routes.iter().map(|r| r.id.as_str()).collect();

    let mut bad_trip = 0usize;
    let mut bad_stop = 0usize;
    for st in &feed.stop_times {
        if !trip_ids.contains(st.trip_id.as_str()) {
            bad_trip += 1;
        }
        if !stop_ids.contains(st.stop_id.as_str()) {
            bad_stop += 1;
        }
    }
    assert_eq!(bad_trip, 0, "{bad_trip} 件の stop_times が未知の trip_id を参照");
    assert_eq!(bad_stop, 0, "{bad_stop} 件の stop_times が未知の stop_id を参照");

    let mut bad_route = 0usize;
    for t in &feed.trips {
        if !route_ids.contains(t.route_id.as_str()) {
            bad_route += 1;
        }
    }
    assert_eq!(bad_route, 0, "{bad_route} 件の trips が未知の route_id を参照");
}
