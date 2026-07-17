//! footpath 乗換 (RAPTOR 内部の近接駅間徒歩) の街路ルーティングを検証する。
//!
//! 本番で確認した問題: 丸ノ内線新宿 → 京王線新宿 (324m) のような乗換徒歩 leg が
//! 「2点直線・distance_m=0・段差なし」で返り、下流が「0m, 階段は検出されませんでした」
//! と **検証していないデータを検証済みのように** 表示していた。修正後は:
//!   - 街路グラフで経路が引けるなら access/egress と同じ profile の A* で
//!     実ジオメトリ・実距離・実段差/EV を返す (`street_routed=true`)。
//!     ただし所要秒は RAPTOR の乗換所要のまま (時刻表接続の整合を壊さない)。
//!   - 引けない場合は 2点直線に `street_routed=false` を立てて返し、サーバが
//!     hasStairs/hasElevator キーを省略する (正直なフォールバック)。
//!
//! フィクスチャ: `tests/fixtures/xfer` (A→X駅東口, 徒歩乗換, X駅西口→D)。
//! X駅東口/西口は約167m 離れた別停留所 (parent_station 無し) なので、RAPTOR が
//! 乗換 footpath の Walk leg を **中間に** 生成する (mini fixture の C1/C2 は同一
//! 親駅に正規化されるため中間 Walk leg が出ない。こちらは出るように設計)。

use std::collections::HashMap;
use std::path::PathBuf;

use otp_core::LatLng;
use otp_engine::{Engine, Leg, Mobility, RouteRequest};
use otp_gtfs::Feed;
use otp_raptor::Timetable;
use otp_street::StreetGraph;

/// A駅側 (access)・D駅側 (egress) の小コンポーネントに加え、X駅東口 (35.02,139.02) と
/// X駅西口 (35.0215,139.02) を中間ノード経由でつなぐ乗換路を持つ徒歩グラフ。
/// 乗換路は 階段 (highway=steps) + エレベーター併設路 (elevator=yes) で構成し、
/// has_stairs/has_elevator が OSM 実データから立つことを検証できるようにする。
const WALK_FIXTURE_WITH_TRANSFER_OSM: &str = r#"<osm version="0.6">
    <node id="1" lat="34.9995" lon="138.9995"/>
    <node id="2" lat="35.00" lon="139.00"/>
    <way id="1"><nd ref="1"/><nd ref="2"/><tag k="highway" v="footway"/></way>
    <node id="10" lat="35.02" lon="139.02"/>
    <node id="11" lat="35.0208" lon="139.0205"/>
    <node id="12" lat="35.0215" lon="139.02"/>
    <way id="3"><nd ref="10"/><nd ref="11"/><tag k="highway" v="steps"/></way>
    <way id="4"><nd ref="11"/><nd ref="12"/><tag k="highway" v="footway"/><tag k="elevator" v="yes"/></way>
    <node id="3" lat="35.03" lon="139.03"/>
    <node id="4" lat="35.0305" lon="139.0305"/>
    <way id="2"><nd ref="3"/><nd ref="4"/><tag k="highway" v="footway"/></way>
</osm>"#;

/// 乗換路 (ノード10-12) を持たない徒歩グラフ。X駅東口/西口の最近傍ノードはどちらも
/// D駅側コンポーネントの同一ノードにスナップされ、街路経路が「1点・距離0」になる
/// → engine はフォールバック (2点直線 + street_routed=false) に落ちるはず。
const WALK_FIXTURE_WITHOUT_TRANSFER_OSM: &str = r#"<osm version="0.6">
    <node id="1" lat="34.9995" lon="138.9995"/>
    <node id="2" lat="35.00" lon="139.00"/>
    <way id="1"><nd ref="1"/><nd ref="2"/><tag k="highway" v="footway"/></way>
    <node id="3" lat="35.03" lon="139.03"/>
    <node id="4" lat="35.0305" lon="139.0305"/>
    <way id="2"><nd ref="3"/><nd ref="4"/><tag k="highway" v="footway"/></way>
</osm>"#;

const ORIGIN_NEAR_A: LatLng = LatLng::new(34.9995, 138.9995);
const DEST_NEAR_D: LatLng = LatLng::new(35.0305, 139.0305);
const X_EAST: LatLng = LatLng::new(35.02, 139.02);
const X_WEST: LatLng = LatLng::new(35.0215, 139.02);

/// RAPTOR が乗換 footpath に与える所要秒 (crates/raptor の
/// `WALK_SPEED_MPS`=1.3 / `DEFAULT_TRANSFER_BUFFER_S`=120 と同じ式)。
/// 「street 経路を引いても duration_s は RAPTOR のこの値のまま変えない」ことを
/// テストで突き合わせるために再計算する。
fn raptor_transfer_duration_s() -> u32 {
    (X_EAST.haversine_m(&X_WEST) / 1.3).ceil() as u32 + 120
}

fn build_engine(osm: &str) -> Engine {
    let dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/xfer"));
    let feed = Feed::load_from_dir(&dir).expect("xfer fixture should load");
    let timetable = Timetable::build(&[feed]).expect("timetable should build");
    let street = StreetGraph::build_from_osm_xml_str(osm);
    Engine::new(street, timetable, HashMap::new())
}

fn request() -> RouteRequest {
    RouteRequest {
        origin: ORIGIN_NEAR_A,
        destination: DEST_NEAR_D,
        depart_at: 7 * 3600 + 57 * 60, // T1 の A発 08:00 に access 徒歩 (~78m) が間に合う
        service_date: 20260713,
        mobility: Mobility::Solo,
        arrive_by: false,
    }
}

/// 経路の中間 (先頭 access・末尾 egress 以外) にある徒歩 leg = footpath 乗換を取り出す。
fn transfer_walk(legs: &[Leg]) -> &Leg {
    assert!(legs.len() >= 3, "access + (乗車/乗換...) + egress の構成のはず: {legs:?}");
    legs[1..legs.len() - 1]
        .iter()
        .find(|l| matches!(l, Leg::Walk { .. }))
        .expect("中間に footpath 乗換の Walk leg があるはず")
}

#[test]
fn footpath_transfer_is_street_routed_with_real_geometry_and_unchanged_duration() {
    let engine = build_engine(WALK_FIXTURE_WITH_TRANSFER_OSM);
    let itineraries = engine.plan(&request()).expect("plan should not error");
    assert!(!itineraries.is_empty(), "経路が1つも見つからなかった");

    match transfer_walk(&itineraries[0].legs) {
        Leg::Walk { distance_m, duration_s, has_stairs, has_elevator, street_routed, geometry, .. } => {
            // 実ジオメトリ: ノード10→11→12 の3点折れ線 (2点直線ではない)。
            assert!(geometry.len() > 2, "街路経路の折れ線は2点を超えるはず: {geometry:?}");
            // 実距離: 約190m (10→11 ~100m + 11→12 ~90m)。0 ではない。
            assert!(*distance_m > 0.0, "distance_m は実距離のはず: {distance_m}");
            assert!((100.0..400.0).contains(distance_m), "distance_m={distance_m} は経路長 ~190m 近辺のはず");
            // 所要秒は RAPTOR の乗換所要 (直線距離/1.3 + バッファ120s) のまま変えない
            // (この秒数で時刻表接続が計算済み。変えると board/alight がずれる)。
            assert_eq!(*duration_s, raptor_transfer_duration_s(), "duration_s は RAPTOR の乗換所要のまま");
            // 段差/EV は OSM 実データ (steps + elevator=yes) から立つ。
            assert!(*has_stairs, "乗換路の highway=steps を検出するはず");
            assert!(*has_elevator, "乗換路の elevator=yes を検出するはず");
            assert!(*street_routed, "街路経路が引けたので street_routed=true");
        }
        other => panic!("Walk leg のはず: {other:?}"),
    }
}

#[test]
fn footpath_transfer_falls_back_to_straight_line_when_street_unroutable() {
    let engine = build_engine(WALK_FIXTURE_WITHOUT_TRANSFER_OSM);
    let itineraries = engine.plan(&request()).expect("plan should not error");
    assert!(!itineraries.is_empty(), "経路が1つも見つからなかった");

    match transfer_walk(&itineraries[0].legs) {
        Leg::Walk { distance_m, duration_s, has_stairs, has_elevator, street_routed, geometry, .. } => {
            // 街路経路が引けない → 従来どおり 2点直線 + 距離0。ただし street_routed=false
            // で「未検証」を明示する (サーバは hasStairs/hasElevator キーを省略する)。
            assert_eq!(geometry.len(), 2, "フォールバックは2点直線: {geometry:?}");
            assert_eq!(*distance_m, 0.0, "フォールバックの distance_m は 0 (未検証)");
            assert!(!street_routed, "街路経路が引けなかったので street_routed=false");
            assert!(!has_stairs && !has_elevator, "未検証の段差/EV は false のまま");
            assert_eq!(*duration_s, raptor_transfer_duration_s(), "duration_s は RAPTOR の乗換所要のまま");
        }
        other => panic!("Walk leg のはず: {other:?}"),
    }
}
