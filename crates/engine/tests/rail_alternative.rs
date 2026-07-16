//! `Engine::plan` が「バスが最速でも鉄道の代替を併せて返す」ことを、決定的な合成
//! フィクスチャで検証する。単一基準 (最早到着) RAPTOR は停留所ごとに最早ラベルしか
//! 残さないため、鉄道経路がバス経路に上書きされて表に出ない (方南町→高尾山口 等で実測)。
//! `plan` は全モード探索 (最速) と鉄道限定探索 (代替) の2本を同一 access/egress で走らせ、
//! シグネチャで重複排除してマージする — その挙動をここで固定する。
//!
//! フィクスチャ: 発地すぐそば (~78m) の A駅 と 着地すぐそば の D駅 を、それぞれ独立した
//! 小さな徒歩コンポーネントで結ぶ (synthetic_door_to_door.rs と同じ作り)。時刻表は
//! ハンドメイドの Feed を `ModeFilter::RailAndBus` で構築し、A→D に「速いバス」「遅い鉄道」
//! を用意する。

use std::collections::HashMap;

use otp_core::{LatLng, RouteId, ServiceId, StopId, TripId};
use otp_engine::{Engine, Leg, Mobility, RouteRequest};
use otp_gtfs::{Calendar, Feed, Route, RouteType, Stop, StopTime, Trip, WheelchairBoarding};
use otp_raptor::{ModeFilter, Timetable};
use otp_street::StreetGraph;

// A駅 (35.00,139.00) 手前の発地と、D駅 (35.03,139.03) 先の着地を footway で結ぶだけの
// 2 コンポーネント徒歩グラフ。
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

fn stops_ad() -> Vec<Stop> {
    vec![
        Stop { id: StopId::new("A"), name: "A駅".into(), lat: 35.00, lng: 139.00, wheelchair_boarding: WheelchairBoarding::Unknown, parent_station: None, zone_id: None },
        Stop { id: StopId::new("D"), name: "D駅".into(), lat: 35.03, lng: 139.03, wheelchair_boarding: WheelchairBoarding::Unknown, parent_station: None, zone_id: None },
    ]
}

fn wd_calendar() -> Calendar {
    Calendar { service_id: ServiceId::new("WD"), weekdays: [true, true, true, true, true, false, false], start_date: 20260101, end_date: 20301231 }
}

/// A→D に速いバス (08:00→08:10) と遅い鉄道 (08:00→08:25) を持つ Engine。
fn engine_bus_and_rail() -> Engine {
    let feed = Feed {
        stops: stops_ad(),
        routes: vec![
            Route { id: RouteId::new("RBUS"), agency_id: None, short_name: "急行バス".into(), long_name: "急行バス".into(), route_type: RouteType::Bus },
            Route { id: RouteId::new("RRAIL"), agency_id: None, short_name: "各停".into(), long_name: "各停".into(), route_type: RouteType::Rail },
        ],
        trips: vec![
            Trip { id: TripId::new("TBUS"), route_id: RouteId::new("RBUS"), service_id: ServiceId::new("WD"), headsign: None, wheelchair_accessible: WheelchairBoarding::Unknown },
            Trip { id: TripId::new("TRAIL"), route_id: RouteId::new("RRAIL"), service_id: ServiceId::new("WD"), headsign: None, wheelchair_accessible: WheelchairBoarding::Unknown },
        ],
        stop_times: vec![
            StopTime { trip_id: TripId::new("TBUS"), stop_id: StopId::new("A"), stop_sequence: 1, arrival: 8 * 3600, departure: 8 * 3600 },
            StopTime { trip_id: TripId::new("TBUS"), stop_id: StopId::new("D"), stop_sequence: 2, arrival: 8 * 3600 + 600, departure: 8 * 3600 + 600 },
            StopTime { trip_id: TripId::new("TRAIL"), stop_id: StopId::new("A"), stop_sequence: 1, arrival: 8 * 3600, departure: 8 * 3600 },
            StopTime { trip_id: TripId::new("TRAIL"), stop_id: StopId::new("D"), stop_sequence: 2, arrival: 8 * 3600 + 1500, departure: 8 * 3600 + 1500 },
        ],
        calendars: vec![wd_calendar()],
        ..Feed::default()
    };
    // バスも載せるため RailAndBus で構築する。
    let timetable = Timetable::build_with_modes(&[feed], ModeFilter::RailAndBus).expect("timetable should build");
    let street = StreetGraph::build_from_osm_xml_str(WALK_FIXTURE_OSM);
    Engine::new(street, timetable, HashMap::new())
}

/// A→D に鉄道 (08:00→08:25) のみを持つ Engine (最速が既に全鉄道のケース)。
fn engine_rail_only() -> Engine {
    let feed = Feed {
        stops: stops_ad(),
        routes: vec![Route { id: RouteId::new("RRAIL"), agency_id: None, short_name: "各停".into(), long_name: "各停".into(), route_type: RouteType::Rail }],
        trips: vec![Trip { id: TripId::new("TRAIL"), route_id: RouteId::new("RRAIL"), service_id: ServiceId::new("WD"), headsign: None, wheelchair_accessible: WheelchairBoarding::Unknown }],
        stop_times: vec![
            StopTime { trip_id: TripId::new("TRAIL"), stop_id: StopId::new("A"), stop_sequence: 1, arrival: 8 * 3600, departure: 8 * 3600 },
            StopTime { trip_id: TripId::new("TRAIL"), stop_id: StopId::new("D"), stop_sequence: 2, arrival: 8 * 3600 + 1500, departure: 8 * 3600 + 1500 },
        ],
        calendars: vec![wd_calendar()],
        ..Feed::default()
    };
    let timetable = Timetable::build_with_modes(&[feed], ModeFilter::RailAndBus).expect("timetable should build");
    let street = StreetGraph::build_from_osm_xml_str(WALK_FIXTURE_OSM);
    Engine::new(street, timetable, HashMap::new())
}

fn request() -> RouteRequest {
    RouteRequest {
        origin: ORIGIN_NEAR_A,
        destination: DEST_NEAR_D,
        // 07:57:00 発。access 徒歩 (~78m) を終えても A の 08:00 発に間に合う。
        depart_at: 7 * 3600 + 57 * 60,
        service_date: 20260713, // 月曜 (WD 運行)
        mobility: Mobility::Solo,
    }
}

/// 各 Itinerary の乗車 leg の mode 列を返すヘルパ。
fn transit_modes(legs: &[Leg]) -> Vec<&'static str> {
    legs.iter()
        .filter_map(|l| match l {
            Leg::Transit { mode, .. } => Some(*mode),
            _ => None,
        })
        .collect()
}

#[test]
fn plan_surfaces_rail_alternative_even_when_bus_is_fastest() {
    let engine = engine_bus_and_rail();
    let itineraries = engine.plan(&request()).expect("plan should not error");

    // 最速 (バス) と 鉄道の代替 の2本が出るはず。
    assert!(itineraries.len() >= 2, "バス最速+鉄道代替の2本以上が出るはず: {itineraries:?}");

    // 先頭 (最速) はバスを含む経路。
    let fastest = &itineraries[0];
    assert!(transit_modes(&fastest.legs).contains(&"BUS"), "最速はバス経路のはず: {:?}", transit_modes(&fastest.legs));

    // どこかに「乗車 leg が全て鉄道 (バスを含まない)」経路が存在するはず。
    let has_all_rail = itineraries.iter().any(|it| {
        let modes = transit_modes(&it.legs);
        !modes.is_empty() && modes.iter().all(|m| *m != "BUS")
    });
    assert!(has_all_rail, "鉄道のみの代替経路が含まれるはず: {itineraries:?}");

    // 鉄道の代替はバスより遅い (08:25着 vs 08:10着) が、押し出されず残っている。
    let rail_it = itineraries
        .iter()
        .find(|it| { let m = transit_modes(&it.legs); !m.is_empty() && m.iter().all(|x| *x != "BUS") })
        .unwrap();
    assert!(rail_it.total_duration_s > fastest.total_duration_s, "鉄道代替はバスより所要が長いはず");
}

#[test]
fn plan_does_not_duplicate_when_fastest_is_already_rail() {
    let engine = engine_rail_only();
    let itineraries = engine.plan(&request()).expect("plan should not error");

    // 全モード探索も鉄道限定探索も同じ鉄道経路を返す → 重複排除で1本だけ。
    assert_eq!(itineraries.len(), 1, "最速が既に全鉄道なら重複せず1本のはず: {itineraries:?}");
    assert_eq!(transit_modes(&itineraries[0].legs), vec!["RAIL"], "唯一の経路は鉄道のはず");
}
