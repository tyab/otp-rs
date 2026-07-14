//! 座標to座標の徒歩経路を出す手動検証用 CLI。
//!
//! 使い方:
//! ```sh
//! cargo run -p otp-street --example route -- <osm.xml> <lat1> <lon1> <lat2> <lon2> [normal|stroller|wheelchair]
//! ```
//! `<osm.xml>` は `scripts/extract_osm_xml.sh` で作った前処理済み OSM XML。

use std::path::PathBuf;

use otp_core::LatLng;
use otp_street::{StreetGraph, WalkProfile};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 6 {
        eprintln!(
            "使い方: route <osm.xml> <lat1> <lon1> <lat2> <lon2> [normal|stroller|wheelchair]"
        );
        std::process::exit(1);
    }
    let osm_path = PathBuf::from(&args[1]);
    let lat1: f64 = args[2].parse().expect("lat1");
    let lon1: f64 = args[3].parse().expect("lon1");
    let lat2: f64 = args[4].parse().expect("lat2");
    let lon2: f64 = args[5].parse().expect("lon2");
    let profile_name = args.get(6).map(String::as_str).unwrap_or("normal");
    let profile = match profile_name {
        "normal" => WalkProfile::normal(),
        "stroller" => WalkProfile::stroller(),
        "wheelchair" => WalkProfile::wheelchair(),
        other => {
            eprintln!("不明なプロファイル: {other}");
            std::process::exit(1);
        }
    };

    let t0 = std::time::Instant::now();
    let graph = StreetGraph::build_from_osm_xml(&osm_path).expect("グラフ構築に失敗");
    eprintln!(
        "graph built: {} nodes, {} edges ({:?})",
        graph.nodes.len(),
        graph.edges.len(),
        t0.elapsed()
    );

    let t1 = std::time::Instant::now();
    match graph.route(LatLng::new(lat1, lon1), LatLng::new(lat2, lon2), &profile) {
        Ok(path) => {
            println!(
                "profile={profile_name} distance_m={:.1} duration_s={:.1} ({:.1}分) has_stairs={} nodes={} ({:?})",
                path.distance_m,
                path.duration_s,
                path.duration_s / 60.0,
                path.has_stairs,
                path.nodes.len(),
                t1.elapsed()
            );
        }
        Err(e) => {
            eprintln!("経路探索失敗: {e}");
            std::process::exit(1);
        }
    }
}
