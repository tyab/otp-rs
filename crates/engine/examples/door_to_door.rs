//! 座標to座標のドアtoドア経路探索を出す手動検証用 CLI。
//!
//! `otp-rs/scripts/compare_door2door.sh` から呼ばれる想定 (直接 `cargo run` してもよい)。
//! `crates/raptor/examples/plan.rs` (駅to駅) の座標to座標版。街路データ
//! (OSM XML, `scripts/extract_osm_xml.sh` で前処理済み) + 複数鉄道 GTFS フィード
//! (都営/メトロ/りんかい/京王/東武/自前頻度JR) を読み込み、`Engine::plan` の結果を
//! 人間可読テキストで stdout に出す。
//!
//! 使い方:
//! ```sh
//! cargo run -p otp-engine --example door_to_door -- <osm.xml> <origin_lat> <origin_lon> \
//!   <dest_lat> <dest_lon> <HH:MM> <YYYYMMDD> <solo|stroller|wheelchair>
//! ```
//! GTFS の在り処は `crates/raptor/examples/plan.rs` と同じ規約 (既定は
//! `../../../infra/otp/data/<feed>.zip`。`OTP_RS_GTFS_DATA_DIR` で差し替え可能)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use otp_core::LatLng;
use otp_engine::{Engine, Leg, Mobility, RouteRequest};
use otp_fares::FareModel;
use otp_gtfs::Feed;
use otp_raptor::Timetable;
use otp_street::StreetGraph;

/// (フィードID, zipファイル名)。本家 OTP の feedId 割り当てと揃えてある
/// (`crates/raptor/examples/plan.rs` のモジュールdoc参照)。
const FEEDS: &[(&str, &str)] = &[
    ("1", "frequency-jr-gtfs.zip"),
    ("2", "twr-train-gtfs.zip"),
    ("3", "tokyometro-train-gtfs.zip"),
    ("4", "challenge-keio-train-gtfs.zip"),
    ("5", "challenge-tobu-train-gtfs.zip"),
    ("6", "toei-train-gtfs.zip"),
];

fn data_dir() -> PathBuf {
    std::env::var("OTP_RS_GTFS_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../infra/otp/data")))
}

fn prepare_feed_dir(feed_prefix: &str, zip_path: &Path) -> PathBuf {
    if !zip_path.is_file() {
        eprintln!("GTFS zip が見つかりません: {} (feed={feed_prefix})", zip_path.display());
        std::process::exit(1);
    }
    let out_dir = std::env::temp_dir().join(format!("otp-rs-engine-plan-gtfs-{feed_prefix}"));
    if !(out_dir.join("stops.txt").is_file() && out_dir.join("stop_times.txt").is_file()) {
        std::fs::create_dir_all(&out_dir).unwrap();
        let status = Command::new("unzip").arg("-o").arg("-q").arg(zip_path).arg("-d").arg(&out_dir).status().unwrap();
        if !status.success() {
            eprintln!("unzip 失敗 (feed={feed_prefix})");
            std::process::exit(1);
        }
    }
    out_dir
}

fn load_all_feeds() -> Vec<Feed> {
    let dir = data_dir();
    FEEDS
        .iter()
        .map(|(prefix, filename)| {
            let zip_path = dir.join(filename);
            let feed_dir = prepare_feed_dir(prefix, &zip_path);
            Feed::load_from_dir_namespaced(&feed_dir, prefix).unwrap_or_else(|e| panic!("feed {prefix} load failed: {e}"))
        })
        .collect()
}

fn parse_hhmm(s: &str) -> i32 {
    let (h, m) = s.split_once(':').expect("HH:MM 形式で指定");
    h.parse::<i32>().unwrap() * 3600 + m.parse::<i32>().unwrap() * 60
}

fn parse_mobility(s: &str) -> Mobility {
    match s {
        "solo" => Mobility::Solo,
        "stroller" => Mobility::Stroller,
        "wheelchair" => Mobility::Wheelchair,
        other => {
            eprintln!("不明な mobility: {other} (solo|stroller|wheelchair)");
            std::process::exit(1);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 9 {
        eprintln!("usage: plan <osm.xml> <origin_lat> <origin_lon> <dest_lat> <dest_lon> <HH:MM> <YYYYMMDD> <solo|stroller|wheelchair>");
        std::process::exit(1);
    }
    let osm_path = PathBuf::from(&args[1]);
    let origin = LatLng::new(args[2].parse().expect("origin_lat"), args[3].parse().expect("origin_lon"));
    let destination = LatLng::new(args[4].parse().expect("dest_lat"), args[5].parse().expect("dest_lon"));
    let depart_at = parse_hhmm(&args[6]);
    let service_date: u32 = args[7].parse().expect("YYYYMMDD");
    let mobility = parse_mobility(&args[8]);

    let t0 = std::time::Instant::now();
    let feeds = load_all_feeds();
    let timetable = Timetable::build(&feeds).expect("timetable build failed");
    eprintln!("timetable built: {} stops ({:?})", timetable.stop_ids.len(), t0.elapsed());

    let t1 = std::time::Instant::now();
    let street = StreetGraph::build_from_osm_xml(&osm_path).expect("street graph build failed");
    eprintln!("street graph built: {} nodes, {} edges ({:?})", street.nodes.len(), street.edges.len(), t1.elapsed());

    // フィードごとに FareModel を組み、Engine に登録する (`otp_engine::Engine` の
    // モジュールdoc参照: `Feed::load_from_dir_namespaced` が付けた prefix がキー)。
    let fares: HashMap<String, FareModel> =
        FEEDS.iter().zip(feeds.iter()).map(|((prefix, _), feed)| (prefix.to_string(), FareModel::from_gtfs(feed))).collect();

    let engine = Engine::new(street, timetable, fares);
    let req = RouteRequest { origin, destination, depart_at, service_date, mobility, arrive_by: false };

    let t2 = std::time::Instant::now();
    let itineraries = engine.plan(&req).expect("plan failed");
    eprintln!("plan() done ({:?})", t2.elapsed());

    if itineraries.is_empty() {
        println!("NO_ITINERARY");
        return;
    }
    for (i, it) in itineraries.iter().enumerate() {
        let h = it.total_duration_s / 3600;
        let m = (it.total_duration_s % 3600) / 60;
        println!("[{i}] total={h}h{m:02}m ({}s) transfers={} fare_yen={:?}", it.total_duration_s, it.transfers, it.fare_yen);
        for leg in &it.legs {
            match leg {
                Leg::Walk { from_name, to_name, distance_m, duration_s, has_stairs, .. } => {
                    println!("  WALK {from_name} -> {to_name} distance={distance_m:.1}m duration={duration_s}s has_stairs={has_stairs}");
                }
                Leg::Transit { route_short_name, from_name, to_name, duration_s, .. } => {
                    println!("  TRANSIT route={route_short_name} {from_name} -> {to_name} duration={duration_s}s");
                }
            }
        }
    }
}
