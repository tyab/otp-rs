//! 決定的な合成フィクスチャで `Engine::plan` の統合フロー (access徒歩→RAPTOR→egress徒歩)
//! を検証する。実データ (OSM/GTFS) 無しで CI が常に回せるように、
//! `crates/gtfs/tests/fixtures/mini` (raptor クレートの `search_finds_known_shortest_path_with_one_transfer`
//! と同じ既知経路 A→C(乗換)→D, 08:00発→08:30着, 乗換1回) に、A駅・D駅それぞれの
//! すぐそば (~80m) を発着点にする小さな徒歩グラフを合わせて使う。
//!
//! 実データでの突き合わせ (新宿⇄本郷三丁目、本家OTPとの数値比較) は
//! `tests/door_to_door.rs` の役目。こちらは「配線が正しいか」の高速な回帰テスト。

use std::collections::HashMap;
use std::path::PathBuf;

use otp_core::LatLng;
use otp_engine::{Engine, Leg, Mobility, RouteRequest};
use otp_gtfs::Feed;
use otp_raptor::Timetable;
use otp_street::StreetGraph;

/// A駅 (35.00,139.00) の ~78m 手前とA駅本体、D駅 (35.03,139.03) 本体と ~78m 先を
/// footway でつないだだけの、2つの独立した小コンポーネントからなる徒歩グラフ。
/// A側・D側それぞれの access/egress ルーティングにしか使わないので、コンポーネント
/// 同士がつながっている必要はない。
const WALK_FIXTURE_OSM: &str = r#"<osm version="0.6">
    <node id="1" lat="34.9995" lon="138.9995"/>
    <node id="2" lat="35.00" lon="139.00"/>
    <way id="1"><nd ref="1"/><nd ref="2"/><tag k="highway" v="footway"/></way>
    <node id="3" lat="35.03" lon="139.03"/>
    <node id="4" lat="35.0305" lon="139.0305"/>
    <way id="2"><nd ref="3"/><nd ref="4"/><tag k="highway" v="footway"/></way>
</osm>"#;

const ORIGIN_NEAR_A: LatLng = LatLng::new(34.9995, 138.9995);
const DEST_NEAR_D: LatLng = LatLng::new(35.0305, 139.0305);

fn fixture_dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../gtfs/tests/fixtures/mini"))
}

fn build_engine() -> Engine {
    let feed = Feed::load_from_dir(&fixture_dir()).expect("mini fixture should load");
    let timetable = Timetable::build(&[feed]).expect("timetable should build");
    let street = StreetGraph::build_from_osm_xml_str(WALK_FIXTURE_OSM);
    // mini fixture は Feed::load_from_dir_namespaced を通さない単一フィード構成で、
    // stops.txt に zone_id も無い (運賃ゾーン無し)。運賃配線そのもの (フィード名前空間
    // 単位での FareModel 選択・ゾーン一致) の実データ検証は `tests/door_to_door.rs`
    // (都営/メトロ等6フィード、本家OTPとの数値突き合わせ) の役目なので、ここでは
    // FareModel を登録しない (`fare_yen` は otp_fares::FareModel が無いフィードとして
    // 常に None になる。`otp_engine::Engine::compute_fare` 参照)。
    Engine::new(street, timetable, HashMap::new())
}

fn base_request(mobility: Mobility) -> RouteRequest {
    RouteRequest {
        origin: ORIGIN_NEAR_A,
        destination: DEST_NEAR_D,
        // 07:57:00。T1 は A を 08:00:00 に発車するため、access 徒歩
        // (直線距離約72m、profileごとに最遅の wheelchair でも ~72秒) が
        // 08:00:00 を超えて T1 に乗り遅れないよう3分の余裕を持たせている。
        depart_at: 7 * 3600 + 57 * 60,
        service_date: 20260713, // 月曜 (mini fixture の WD/WD_EXTRA とも運行)
        mobility,
    }
}

#[test]
fn plan_wires_access_walk_raptor_and_egress_walk_into_one_itinerary() {
    let engine = build_engine();
    let itineraries = engine.plan(&base_request(Mobility::Solo)).expect("plan should not error");

    assert!(!itineraries.is_empty(), "経路が1つも見つからなかった");
    let best = &itineraries[0];

    // 期待する構成: [Walk(access, o1->A), Transit(A->C), Transit(C->D), Walk(egress, D->d1)]。
    // C1/C2 は同一駅Cに正規化されるため乗換は「同一停留所内バッファ」扱いで
    // 別立ての Walk leg にはならない (otp_raptor モジュール doc 参照)。
    assert_eq!(best.legs.len(), 4, "legs was {:?}", best.legs);

    match &best.legs[0] {
        Leg::Walk { distance_m, duration_s, has_stairs } => {
            // 34.9995,138.9995 -> 35.00,139.00 の実距離は約78m。
            assert!((50.0..120.0).contains(distance_m), "access distance_m={distance_m}");
            assert!(*duration_s > 0, "access duration should be non-zero");
            assert!(!has_stairs, "footway only, no stairs");
        }
        other => panic!("先頭は access の Walk leg のはず: {other:?}"),
    }

    let transit_legs: Vec<_> = best
        .legs
        .iter()
        .filter_map(|l| match l {
            Leg::Transit { route_name, from_stop, to_stop, duration_s } => Some((route_name.clone(), from_stop.clone(), to_stop.clone(), *duration_s)),
            _ => None,
        })
        .collect();
    assert_eq!(transit_legs.len(), 2, "大江戸線...ではなく mini fixture のT1/T2を2本乗り継ぐはず: {transit_legs:?}");
    assert_eq!(transit_legs[0].1, "A");
    assert_eq!(transit_legs[0].2, "C");
    assert_eq!(transit_legs[1].1, "C");
    assert_eq!(transit_legs[1].2, "D");

    match best.legs.last().unwrap() {
        Leg::Walk { distance_m, duration_s, has_stairs } => {
            assert!((50.0..120.0).contains(distance_m), "egress distance_m={distance_m}");
            assert!(*duration_s > 0, "egress duration should be non-zero");
            assert!(!has_stairs);
        }
        other => panic!("末尾は egress の Walk leg のはず: {other:?}"),
    }

    assert_eq!(best.transfers, 1, "都庁前相当 (C駅) で1回乗換のはず");
    // mini fixture には FareModel を登録していない (build_engine 参照) ので None。
    assert!(best.fare_yen.is_none(), "FareModelを登録していないフィードの運賃はNoneのはず");
    // RAPTOR の到着 (08:30 出発+乗換込み) に access/egress 徒歩が乗るので、
    // 少なくとも鉄道所要 (30分) より長くなるはず。
    assert!(best.total_duration_s > 30 * 60, "total_duration_s={}", best.total_duration_s);
}

#[test]
fn wheelchair_walks_slower_than_solo_so_total_duration_is_not_shorter() {
    let engine = build_engine();
    let solo = engine.plan(&base_request(Mobility::Solo)).expect("solo plan should not error");
    let wheelchair = engine.plan(&base_request(Mobility::Wheelchair)).expect("wheelchair plan should not error");

    assert!(!solo.is_empty() && !wheelchair.is_empty());
    // このフィクスチャに階段は無いので差は速度のみ (wheelchair: 1.0m/s < solo: 1.33m/s)。
    // 同じ距離を歩くならより時間がかかるはずで、短くなることはない。
    assert!(
        wheelchair[0].total_duration_s >= solo[0].total_duration_s,
        "wheelchair ({}) should not be faster than solo ({}) on identical stair-free paths",
        wheelchair[0].total_duration_s,
        solo[0].total_duration_s
    );
}

#[test]
fn plan_returns_empty_when_street_graph_is_unbuilt() {
    let feed = Feed::load_from_dir(&fixture_dir()).expect("mini fixture should load");
    let timetable = Timetable::build(&[feed]).expect("timetable should build");
    let engine = Engine::new(StreetGraph::default(), timetable, HashMap::new());

    let itineraries = engine.plan(&base_request(Mobility::Solo)).expect("should not error, just empty");
    assert!(itineraries.is_empty(), "未構築の street グラフでは空を返すはず");
}

#[test]
fn plan_returns_empty_when_no_stop_is_within_access_egress_radius() {
    let engine = build_engine();
    let mut req = base_request(Mobility::Solo);
    // どの駅からも1000m超離れた地点 (mini fixtureのA駅から約111km)。
    req.origin = LatLng::new(36.0, 139.0);

    let itineraries = engine.plan(&req).expect("should not error, just empty");
    assert!(itineraries.is_empty(), "近傍駅が無ければ空を返すはず");
}
