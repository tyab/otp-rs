//! 実データスモークテスト: 都営地下鉄GTFSで新宿⇄本郷三丁目 (共に大江戸線) を解けることを検証する。
//!
//! 手計算で厳密検証するのはスライス1のフィクスチャテストの役目。ここでは
//! 「実データ規模でパニックせず、妥当な (到着 > 出発、乗換回数が上限以内) Journey が
//! 返る」ことだけを確認する (タスク仕様通りのスコープ)。
//!
//! データの用意方法は otp-gtfs の `load_real_data.rs` と同じ (README/コメント参照)。

use std::path::{Path, PathBuf};
use std::process::Command;

use otp_gtfs::Feed;
use otp_raptor::{RaptorQuery, StreetLink, Timetable};

fn default_zip_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../infra/otp/data/toei-train-gtfs.zip"))
}

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
        eprintln!("実データ GTFS zip が見つかりません ({}). skip します。", zip_path.display());
        return None;
    }

    let out_dir = std::env::temp_dir().join("otp-rs-toei-gtfs-test"); // gtfs crate のテストと共有 (同一zip)
    if !already_extracted(&out_dir) {
        std::fs::create_dir_all(&out_dir).ok()?;
        let status = Command::new("unzip").arg("-o").arg("-q").arg(&zip_path).arg("-d").arg(&out_dir).status();
        match status {
            Ok(s) if s.success() => {}
            _ => {
                eprintln!("unzip に失敗しました。skip します。");
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
fn shinjuku_to_hongo_sanchome_returns_sane_journey() {
    let Some(dir) = prepare_data_dir() else { return };
    let feed = Feed::load_from_dir(&dir).expect("実データ feed のロードに失敗");
    let tt = Timetable::build(std::slice::from_ref(&feed)).expect("timetable should build");

    // 実測 (infra/otp/data/toei-train-gtfs.zip の stops.txt): 新宿(大江戸線 E-27)=428,
    // 本郷三丁目(大江戸線 E-08)=409。
    let shinjuku = tt.stop_idx(&otp_core::StopId::new("428")).expect("新宿 (428) が時刻表に無い");
    let hongo = tt.stop_idx(&otp_core::StopId::new("409")).expect("本郷三丁目 (409) が時刻表に無い");

    let query = RaptorQuery {
        access: vec![StreetLink { stop: shinjuku, duration_s: 0 }],
        egress: vec![StreetLink { stop: hongo, duration_s: 0 }],
        earliest_departure: 8 * 3600, // 08:00:00
        service_date: 20260713,       // 月曜日 (calendar.txt の実測値と付き合わせ済み)
        max_rounds: 4,
    };

    let journeys = tt.search(&query).expect("search should not panic/error");
    assert!(!journeys.is_empty(), "新宿→本郷三丁目 の経路が1つも見つからなかった");

    for j in &journeys {
        assert!(j.arrival_s > query.earliest_departure, "到着が出発より前: {}", j.arrival_s);
        assert!(j.transfers <= query.max_rounds, "乗換回数が上限超過: {}", j.transfers);
        assert!(!j.legs.is_empty());
    }

    let best = journeys.last().unwrap();
    eprintln!(
        "新宿(428)→本郷三丁目(409) 08:00発: 到着={}秒 ({}時{}分), 乗換={}回, legs={}",
        best.arrival_s,
        best.arrival_s / 3600,
        (best.arrival_s % 3600) / 60,
        best.transfers,
        best.legs.len()
    );
    for leg in &best.legs {
        eprintln!("  {leg:?}");
    }
}
