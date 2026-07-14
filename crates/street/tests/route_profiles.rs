//! プロファイル (通常/ベビーカー/車いす) によって A* の選ぶ経路が変わることを
//! 検証する。otp-street の核心 (アクセシビリティ・コスト) の実証テスト。
//!
//! フィクスチャ (`fixtures/profile_routes.osm`) は出発ノード1→到着ノード2への
//! 経路を2通り持つ: 階段の直行 (短いが `has_stairs=true`) と、迂回の遠回り
//! (長いが `wheelchair=yes` かつ階段なし)。加えて、歩行不可のはずの
//! `highway=motorway` / `foot=no` の「近道」も混ぜてあり、フィルタが正しく
//! 効いていないと (誤って含まれると) どのプロファイルでもテストが落ちる。

use std::path::PathBuf;

use otp_core::LatLng;
use otp_street::{StreetGraph, WalkProfile};

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

const ORIGIN: LatLng = LatLng::new(35.69000, 139.70000); // ノード1
const DEST: LatLng = LatLng::new(35.69090, 139.70000); // ノード2

#[test]
fn normal_profile_takes_shortest_route_even_with_stairs() {
    let graph = StreetGraph::build_from_osm_xml(&fixture_path("profile_routes.osm"))
        .expect("fixture should build");
    let path = graph
        .route(ORIGIN, DEST, &WalkProfile::normal())
        .expect("route should be found");

    assert!(
        path.has_stairs,
        "normal profile should take the short stairs route"
    );
    // 階段直行 (約100m) を選ぶはず。迂回 (約250m) よりだいぶ短い。
    assert!(
        path.distance_m < 150.0,
        "expected the direct stairs route (~100m), got {}",
        path.distance_m
    );
}

#[test]
fn stroller_profile_avoids_stairs_by_taking_the_detour() {
    let graph = StreetGraph::build_from_osm_xml(&fixture_path("profile_routes.osm"))
        .expect("fixture should build");
    let path = graph
        .route(ORIGIN, DEST, &WalkProfile::stroller())
        .expect("route should be found");

    assert!(
        !path.has_stairs,
        "stroller profile should avoid stairs even though it's the shorter path"
    );
    // 迂回路 (約250m) を選ぶはず。
    assert!(
        path.distance_m > 150.0,
        "expected the detour route (~250m), got {}",
        path.distance_m
    );
}

#[test]
fn wheelchair_profile_also_avoids_stairs() {
    let graph = StreetGraph::build_from_osm_xml(&fixture_path("profile_routes.osm"))
        .expect("fixture should build");
    let path = graph
        .route(ORIGIN, DEST, &WalkProfile::wheelchair())
        .expect("route should be found");

    assert!(!path.has_stairs, "wheelchair profile should avoid stairs");
    assert!(
        path.distance_m > 150.0,
        "expected the detour route (~250m), got {}",
        path.distance_m
    );
}

#[test]
fn wheelchair_and_stroller_route_diverges_from_normal() {
    // 3プロファイルを同一グラフ・同一ODで通し、normal だけが違う経路
    // (has_stairs=true) を選ぶことを1テストで並べて確認する。
    let graph = StreetGraph::build_from_osm_xml(&fixture_path("profile_routes.osm"))
        .expect("fixture should build");

    let normal = graph.route(ORIGIN, DEST, &WalkProfile::normal()).unwrap();
    let stroller = graph.route(ORIGIN, DEST, &WalkProfile::stroller()).unwrap();
    let wheelchair = graph
        .route(ORIGIN, DEST, &WalkProfile::wheelchair())
        .unwrap();

    assert!(normal.has_stairs);
    assert!(!stroller.has_stairs);
    assert!(!wheelchair.has_stairs);
    // 車いすは速度が遅い分、同じ迂回路でも所要時間が長くなる。
    assert!(
        wheelchair.duration_s > stroller.duration_s,
        "wheelchair should take longer on the same detour due to slower speed"
    );
}
