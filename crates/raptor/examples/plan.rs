//! 本家 OTP との突き合わせ用の小さな CLI。
//!
//! `otp-rs/scripts/compare_otp.sh` から呼ばれる想定 (直接 `cargo run` してもよい)。
//! 複数の鉄道 GTFS フィード (都営地下鉄・東京メトロ・りんかい線・京王・東武・
//! 自前頻度 JR) を名前空間化して読み込み、駅to駅の RAPTOR 探索結果を人間可読テキストで
//! stdout に出す。
//!
//! stop_id はフィード ID を前置した `"<feedId>:<rawStopId>"` 形式で指定する。
//! フィード ID は babymobi の本家 OTP (`infra/otp`) が `feeds { feedId }` /
//! `agencies { gtfsId }` で実際に採番している値と揃えてある (実測 2026-07-15,
//! `curl -X POST :8080/otp/gtfs/v1 --data '{"query":"{ agencies { gtfsId name } }"}'`):
//!   1=frequency-jr (自前頻度 GTFS, BMC-FREQ) / 2=twr (りんかい線) /
//!   3=tokyometro (東京メトロ) / 4=keio (京王, チャレンジデータ) /
//!   5=tobu (東武, チャレンジデータ) / 6=toei (都営地下鉄)
//! こう揃えることで、本家 OTP の `stopLocationId` にそのまま同じ文字列を使い回して
//! 突き合わせできる (`scripts/compare_otp.sh` 参照)。
//!
//! 使い方:
//! ```sh
//! cargo run -p otp-raptor --example plan -- <origin_stop_id> <dest_stop_id> <HH:MM> <YYYYMMDD>
//! # 例 (都営単一フィード内): 6:428 6:409 08:00 20260713
//! # 例 (メトロ→都営の乗換): 3:805 6:204 08:00 20260713
//! ```
//! GTFS の在り処は otp-gtfs の実データテストと同じ規約
//! (既定は `../../../infra/otp/data/<feed>.zip`)。`OTP_RS_GTFS_DATA_DIR` で
//! zip の置き場所を丸ごと差し替えられる。

use std::path::{Path, PathBuf};
use std::process::Command;

use otp_gtfs::Feed;
use otp_raptor::{RaptorQuery, StreetLink, Timetable};

/// (フィードID, zipファイル名)。本家 OTP の feedId 割り当てと揃えてある (モジュール doc 参照)。
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
    let out_dir = std::env::temp_dir().join(format!("otp-rs-plan-gtfs-{feed_prefix}"));
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        eprintln!("usage: plan <origin_stop_id> <dest_stop_id> <HH:MM> <YYYYMMDD>");
        eprintln!("  stop_id はフィードID前置 (例: 6:428 = 都営 新宿, 3:805 = メトロ 六本木一丁目)");
        std::process::exit(1);
    }
    let origin_id = &args[1];
    let dest_id = &args[2];
    let earliest_departure = parse_hhmm(&args[3]);
    let service_date: u32 = args[4].parse().expect("YYYYMMDD");

    let feeds = load_all_feeds();
    let tt = Timetable::build(&feeds).expect("timetable build failed");

    let origin = tt.stop_idx(&otp_core::StopId::new(origin_id.as_str())).unwrap_or_else(|| panic!("origin stop {origin_id} not found"));
    let dest = tt.stop_idx(&otp_core::StopId::new(dest_id.as_str())).unwrap_or_else(|| panic!("dest stop {dest_id} not found"));

    let query = RaptorQuery {
        access: vec![StreetLink { stop: origin, duration_s: 0 }],
        egress: vec![StreetLink { stop: dest, duration_s: 0 }],
        earliest_departure,
        service_date,
        max_rounds: 4,
        rail_only: false,
    };

    let journeys = tt.search(&query).expect("search failed");
    if journeys.is_empty() {
        println!("NO_JOURNEY");
        return;
    }
    for j in &journeys {
        let h = j.arrival_s / 3600;
        let m = (j.arrival_s % 3600) / 60;
        let s = j.arrival_s % 60;
        println!("arrival={h:02}:{m:02}:{s:02} ({}s) transfers={}", j.arrival_s, j.transfers);
        for leg in &j.legs {
            match leg {
                otp_raptor::JourneyLeg::Walk { stop, duration_s } => println!("  WALK stop_idx={stop} duration={duration_s}s"),
                otp_raptor::JourneyLeg::Transit { route_short_name, trip_id, from, to, board_s, alight_s, .. } => {
                    let bh = board_s / 3600;
                    let bm = (board_s % 3600) / 60;
                    let ah = alight_s / 3600;
                    let am = (alight_s % 3600) / 60;
                    println!(
                        "  TRANSIT route={route_short_name} trip={trip_id} from_idx={from}@{bh:02}:{bm:02} to_idx={to}@{ah:02}:{am:02}"
                    );
                }
            }
        }
    }
}
