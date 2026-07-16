//! `Engine::plan` の arrive-by (到着時刻指定) 配線を、決定的な合成フィクスチャで検証する。
//! `rail_alternative.rs` / `synthetic_door_to_door.rs` と同じ 2 コンポーネント徒歩グラフ
//! (発地すぐそばの A駅、着地すぐそばの D駅) に、A→D を走る鉄道3便 (08:00/08:20/08:40 発) の
//! 手組み Feed を合わせる。arrive-by 探索が「締切内で最も遅く出発する経路」を返し、実到着が
//! 締切 T 以下になることを固定する (no-fabrication: 締切を超える到着を返さない)。

use std::collections::HashMap;

use otp_core::{LatLng, RouteId, ServiceId, StopId, TripId};
use otp_engine::{Engine, Leg, Mobility, RouteRequest};
use otp_gtfs::{Calendar, Feed, Route, RouteType, Stop, StopTime, Trip, WheelchairBoarding};
use otp_raptor::Timetable;
use otp_street::StreetGraph;

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

/// A→D の鉄道1路線に3便 (08:00→08:30 / 08:20→08:50 / 08:40→09:10) を持つ Engine。
fn engine_three_trips() -> Engine {
    let trip = |id: &str, dep: i32, arr: i32| {
        (
            Trip { id: TripId::new(id), route_id: RouteId::new("RRAIL"), service_id: ServiceId::new("WD"), headsign: None, wheelchair_accessible: WheelchairBoarding::Unknown },
            vec![
                StopTime { trip_id: TripId::new(id), stop_id: StopId::new("A"), stop_sequence: 1, arrival: dep, departure: dep },
                StopTime { trip_id: TripId::new(id), stop_id: StopId::new("D"), stop_sequence: 2, arrival: arr, departure: arr },
            ],
        )
    };
    let trips = [
        trip("Tr1", 8 * 3600, 8 * 3600 + 1800),        // 08:00 → 08:30
        trip("Tr2", 8 * 3600 + 1200, 8 * 3600 + 3000), // 08:20 → 08:50
        trip("Tr3", 8 * 3600 + 2400, 9 * 3600 + 600),  // 08:40 → 09:10
    ];
    let feed = Feed {
        stops: vec![
            Stop { id: StopId::new("A"), name: "A駅".into(), lat: 35.00, lng: 139.00, wheelchair_boarding: WheelchairBoarding::Unknown, parent_station: None, zone_id: None },
            Stop { id: StopId::new("D"), name: "D駅".into(), lat: 35.03, lng: 139.03, wheelchair_boarding: WheelchairBoarding::Unknown, parent_station: None, zone_id: None },
        ],
        routes: vec![Route { id: RouteId::new("RRAIL"), agency_id: None, short_name: "各停".into(), long_name: "各停".into(), route_type: RouteType::Rail }],
        trips: trips.iter().map(|(t, _)| t.clone()).collect(),
        stop_times: trips.iter().flat_map(|(_, st)| st.clone()).collect(),
        calendars: vec![Calendar { service_id: ServiceId::new("WD"), weekdays: [true, true, true, true, true, false, false], start_date: 20260101, end_date: 20301231 }],
        ..Feed::default()
    };
    let timetable = Timetable::build(&[feed]).expect("timetable should build");
    let street = StreetGraph::build_from_osm_xml_str(WALK_FIXTURE_OSM);
    Engine::new(street, timetable, HashMap::new())
}

fn arrive_by_request(deadline: i32) -> RouteRequest {
    RouteRequest {
        origin: ORIGIN_NEAR_A,
        destination: DEST_NEAR_D,
        depart_at: deadline, // arrive_by=true なので「到着締切時刻」を表す
        service_date: 20260713, // 月曜
        mobility: Mobility::Solo,
        arrive_by: true,
    }
}

#[test]
fn plan_arrive_by_returns_itinerary_arriving_before_deadline_with_latest_departure() {
    let engine = engine_three_trips();
    let deadline = 9 * 3600; // 到着締切 09:00
    let itineraries = engine.plan(&arrive_by_request(deadline)).expect("plan should not error");

    assert!(!itineraries.is_empty(), "arrive-by で経路が1つも見つからなかった");
    let it = &itineraries[0];

    // 実到着 = depart_s + total_duration は締切 09:00 以下でなければならない (最重要: no-fabrication)。
    let arrival = it.depart_s + it.total_duration_s as i32;
    assert!(arrival <= deadline, "実到着 {arrival} は締切 {deadline} 以下のはず");

    // 最遅出発: 出発地を発つのは 08:10 より後 (最早の Tr1=08:00発 ではなく、締切内で最も遅い
    // Tr2=08:20発 を選んでいること)。access 徒歩ぶん 08:20 より少し手前になる。
    assert!(it.depart_s > 8 * 3600 + 10 * 60, "最遅出発のはず (depart_s={})、Tr1 なら 08:00 付近になる", it.depart_s);
    assert!(it.depart_s <= 8 * 3600 + 1200, "Tr2 の 08:20 発に間に合う範囲のはず (depart_s={})", it.depart_s);

    // 乗車 leg は各停 A駅→D駅 の1本。
    let transit: Vec<_> = it
        .legs
        .iter()
        .filter_map(|l| match l {
            Leg::Transit { route_short_name, from_name, to_name, .. } => Some((route_short_name.clone(), from_name.clone(), to_name.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(transit.len(), 1, "1便のはず: {transit:?}");
    assert_eq!(transit[0], ("各停".to_string(), "A駅".to_string(), "D駅".to_string()));
}

#[test]
fn plan_arrive_by_departs_later_than_depart_at_from_deadline_naive() {
    // arrive-by の出発は、締切時刻をそのまま出発時刻とみなす素朴な depart-at より必ず早い
    // (=締切前に出発する) が、日付 0時起点の素朴 depart-at よりは遅く出発する。ここでは
    // 「締切より前に出発し、実到着が締切以下」という基本性質を締切を変えて2回確認する。
    let engine = engine_three_trips();
    for deadline in [9 * 3600, 8 * 3600 + 3300 /* 08:55 */] {
        let it = engine.plan(&arrive_by_request(deadline)).expect("plan should not error").into_iter().next();
        let it = it.expect("経路が見つからなかった");
        let arrival = it.depart_s + it.total_duration_s as i32;
        assert!(it.depart_s < deadline, "出発 {} は締切 {} より前のはず", it.depart_s, deadline);
        assert!(arrival <= deadline, "実到着 {arrival} は締切 {deadline} 以下のはず");
    }
}
