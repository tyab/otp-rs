//! 複数フィード統合の実データスモークテスト。
//!
//! 東京メトロ・都営地下鉄の2フィードを名前空間化して1つの `Timetable` にまとめ、
//! 事業者をまたぐ乗換 (南北線 六本木一丁目 → [白金高輪で乗換] → 三田線 三田) が
//! 解けることを検証する。白金高輪はメトロ (3:803) と都営 (6:203) で別 stop_id
//! (直線距離 実測約16m) だが `parent_station` では繋がっていない (実測: どちらの
//! フィードも parent_station 列が常に空)。このテストは、近接駅の徒歩乗換エッジ
//! (`Timetable::build` が構築する) が事業者をまたいでも正しく機能することを保証する。
//!
//! 実測 (2026-07-15, 本家 OTP `planConnection`, stopLocationId `3:805`→`6:204`,
//! 2026-07-13T08:00 発): OTP 08:05発→08:18着 (乗換1回, 白金高輪で253秒接続)。
//! この RAPTOR (直線距離ベースの近似) は 08:01発→08:14着、白金高輪の乗換は133秒
//! ( = ceil(16m / 1.3m/s) + 120秒バッファ )。地下通路の実測とは異なるが、
//! 「乗換自体が発生し、かつ妥当な (0秒ではない) 所要時間を持つ」ことを確認する。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use otp_gtfs::Feed;
use otp_raptor::{JourneyLeg, RaptorQuery, StreetLink, Timetable};

/// このテストファイル内の2テストが同じフィード ("6"=都営, "3"=メトロ) を並行して
/// 展開しようとすると unzip 同士が競合する (実測: `cargo test` のデフォルト並列実行で
/// "cannot create .../agency.txt" が出た)。展開区間だけ直列化して回避する。
static EXTRACT_LOCK: Mutex<()> = Mutex::new(());

fn data_dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../infra/otp/data"))
}

fn prepare_feed_dir(feed_prefix: &str, zip_filename: &str) -> Option<PathBuf> {
    let zip_path = data_dir().join(zip_filename);
    if !zip_path.is_file() {
        eprintln!("実データ GTFS zip が見つかりません ({}). skip します。", zip_path.display());
        return None;
    }
    let _guard = EXTRACT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let out_dir = std::env::temp_dir().join(format!("otp-rs-multi-feed-test-{feed_prefix}"));
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
fn cross_operator_transfer_at_shirokanetakanawa_is_found() {
    let Some(toei_dir) = prepare_feed_dir("6", "toei-train-gtfs.zip") else { return };
    let Some(metro_dir) = prepare_feed_dir("3", "tokyometro-train-gtfs.zip") else { return };

    let toei = Feed::load_from_dir_namespaced(&toei_dir, "6").expect("都営フィードのロードに失敗");
    let metro = Feed::load_from_dir_namespaced(&metro_dir, "3").expect("メトロフィードのロードに失敗");
    let tt = Timetable::build(&[toei, metro]).expect("timetable should build");

    // 六本木一丁目 (メトロ南北線, 3:805) → 三田 (都営三田線, 6:204)。
    let roppongi_itchome = tt.stop_idx(&otp_core::StopId::new("3:805")).expect("六本木一丁目 (3:805) が時刻表に無い");
    let mita = tt.stop_idx(&otp_core::StopId::new("6:204")).expect("三田 (6:204) が時刻表に無い");

    let query = RaptorQuery {
        access: vec![StreetLink { stop: roppongi_itchome, duration_s: 0 }],
        egress: vec![StreetLink { stop: mita, duration_s: 0 }],
        earliest_departure: 8 * 3600,
        service_date: 20260713,
        max_rounds: 4,
        rail_only: false,
    };

    let journeys = tt.search(&query).expect("search should not panic/error");
    assert!(!journeys.is_empty(), "六本木一丁目→三田 の経路が1つも見つからなかった");

    let best = journeys.last().expect("経路が見つからなかった");
    assert_eq!(best.transfers, 1, "白金高輪で1回乗換のはず: {best:?}");

    let walk_legs: Vec<_> = best.legs.iter().filter_map(|l| match l { JourneyLeg::Walk { duration_s, .. } => Some(*duration_s), _ => None }).collect();
    assert_eq!(walk_legs.len(), 1, "白金高輪での乗換徒歩が1本あるはず: {:?}", best.legs);
    let transfer_walk_s = walk_legs[0];
    assert!(
        (120..300).contains(&transfer_walk_s),
        "近接駅徒歩乗換 (16m + 120秒バッファ) の所要時間が想定範囲外: {transfer_walk_s}s"
    );

    let transit_legs: Vec<_> = best
        .legs
        .iter()
        .filter_map(|l| match l {
            JourneyLeg::Transit { route_short_name, .. } => Some(route_short_name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(transit_legs.len(), 2, "南北線→三田線の2本乗り継ぎのはず: {transit_legs:?}");
    assert!(transit_legs[0].contains("南北線"), "1本目は南北線のはず: {transit_legs:?}");
    assert!(transit_legs[1].contains("三田線"), "2本目は三田線のはず: {transit_legs:?}");

    eprintln!(
        "六本木一丁目(3:805)→三田(6:204) 08:00発: 到着={}秒 ({}時{}分), 乗換={}回, 乗換徒歩={}秒",
        best.arrival_s,
        best.arrival_s / 3600,
        (best.arrival_s % 3600) / 60,
        best.transfers,
        transfer_walk_s,
    );
}

#[test]
fn namespacing_prevents_service_id_collision_across_feeds() {
    // 実測: 都営・メトロとも service_id "0"/"1" を使い回している (calendar.txt)。
    // 名前空間化せずにマージすると、片方のカレンダーがもう片方を上書きし、
    // 本来運行日ではない便が「運行あり」と誤判定されるおそれがある。
    // ここでは少なくとも運行日判定によって経路が過不足なく得られることを
    // (前段の cross_operator_transfer_at_shirokanetakanawa_is_found で) 間接検証済みだが、
    // ここでは直接 service_id の非衝突を検証する。
    let Some(toei_dir) = prepare_feed_dir("6", "toei-train-gtfs.zip") else { return };
    let Some(metro_dir) = prepare_feed_dir("3", "tokyometro-train-gtfs.zip") else { return };

    let toei = Feed::load_from_dir_namespaced(&toei_dir, "6").expect("都営フィードのロードに失敗");
    let metro = Feed::load_from_dir_namespaced(&metro_dir, "3").expect("メトロフィードのロードに失敗");

    use std::collections::HashSet;
    let toei_services: HashSet<&str> = toei.calendars.iter().map(|c| c.service_id.as_str()).collect();
    let metro_services: HashSet<&str> = metro.calendars.iter().map(|c| c.service_id.as_str()).collect();
    assert!(!toei_services.is_empty());
    assert!(!metro_services.is_empty());
    assert!(toei_services.is_disjoint(&metro_services), "名前空間化後は service_id が事業者間で衝突しないはず");

    let toei_routes: HashSet<&str> = toei.routes.iter().map(|r| r.id.as_str()).collect();
    let metro_routes: HashSet<&str> = metro.routes.iter().map(|r| r.id.as_str()).collect();
    assert!(toei_routes.is_disjoint(&metro_routes), "名前空間化後は route_id が事業者間で衝突しないはず");
}
