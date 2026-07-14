//! 本家 OTP との突き合わせ用の小さな CLI。
//!
//! `otp-rs/scripts/compare_otp.sh` から呼ばれる想定 (直接 `cargo run` してもよい)。
//! 都営地下鉄 GTFS (`infra/otp/data/toei-train-gtfs.zip`) を読み込み、駅to駅の
//! RAPTOR 探索結果を人間可読テキストで stdout に出す。
//!
//! 使い方:
//! ```sh
//! cargo run -p otp-raptor --example plan -- <origin_stop_id> <dest_stop_id> <HH:MM> <YYYYMMDD>
//! # 例: cargo run -p otp-raptor --example plan -- 428 409 08:00 20260713
//! ```
//! GTFS の在り処は otp-gtfs の実データテストと同じ規約 (`OTP_RS_GTFS_DIR` /
//! `OTP_RS_GTFS_ZIP` 環境変数、既定は `../../../infra/otp/data/toei-train-gtfs.zip`)。

use std::path::PathBuf;
use std::process::Command;

use otp_gtfs::Feed;
use otp_raptor::{RaptorQuery, StreetLink, Timetable};

fn default_zip_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../infra/otp/data/toei-train-gtfs.zip"))
}

fn prepare_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("OTP_RS_GTFS_DIR") {
        return PathBuf::from(dir);
    }
    let zip_path = std::env::var("OTP_RS_GTFS_ZIP").map(PathBuf::from).unwrap_or_else(|_| default_zip_path());
    if !zip_path.is_file() {
        eprintln!("GTFS zip が見つかりません: {}", zip_path.display());
        std::process::exit(1);
    }
    let out_dir = std::env::temp_dir().join("otp-rs-toei-gtfs-test");
    if !(out_dir.join("stops.txt").is_file() && out_dir.join("stop_times.txt").is_file()) {
        std::fs::create_dir_all(&out_dir).unwrap();
        let status = Command::new("unzip").arg("-o").arg("-q").arg(&zip_path).arg("-d").arg(&out_dir).status().unwrap();
        if !status.success() {
            eprintln!("unzip 失敗");
            std::process::exit(1);
        }
    }
    out_dir
}

fn parse_hhmm(s: &str) -> i32 {
    let (h, m) = s.split_once(':').expect("HH:MM 形式で指定");
    h.parse::<i32>().unwrap() * 3600 + m.parse::<i32>().unwrap() * 60
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        eprintln!("usage: plan <origin_stop_id> <dest_stop_id> <HH:MM> <YYYYMMDD>");
        std::process::exit(1);
    }
    let origin_id = &args[1];
    let dest_id = &args[2];
    let earliest_departure = parse_hhmm(&args[3]);
    let service_date: u32 = args[4].parse().expect("YYYYMMDD");

    let dir = prepare_data_dir();
    let feed = Feed::load_from_dir(&dir).expect("feed load failed");
    let tt = Timetable::build(std::slice::from_ref(&feed)).expect("timetable build failed");

    let origin = tt.stop_idx(&otp_core::StopId::new(origin_id.as_str())).unwrap_or_else(|| panic!("origin stop {origin_id} not found"));
    let dest = tt.stop_idx(&otp_core::StopId::new(dest_id.as_str())).unwrap_or_else(|| panic!("dest stop {dest_id} not found"));

    let query = RaptorQuery {
        access: vec![StreetLink { stop: origin, duration_s: 0 }],
        egress: vec![StreetLink { stop: dest, duration_s: 0 }],
        earliest_departure,
        service_date,
        max_rounds: 4,
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
