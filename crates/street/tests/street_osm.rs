//! OSM XML 取り込みのアクセシビリティ属性抽出・歩行不可 way の除外・
//! スナップ+A* の基本正当性を検証する。
//!
//! フィクスチャ `fixtures/tags.osm` は12本の独立した短い区間 (way) を持ち、
//! 座標帯 (lon を 0.0001 ずつずらす) で区別できる。9本が歩行可能属性の
//! バリエーション、3本 (`highway=motorway`/`foot=no`/`highway=cycleway`) が
//! 除外対象。

use std::path::PathBuf;

use otp_core::LatLng;
use otp_street::{StreetEdge, StreetGraph, WalkProfile};

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// 座標に一致するノードを探し、その出エッジのうち `to` が `to_coord` に
/// 一致するものを返す (テスト専用の素朴な検索。1m 以内を同一ノードとみなす)。
fn find_edge(graph: &StreetGraph, from_coord: LatLng, to_coord: LatLng) -> Option<&StreetEdge> {
    graph.edges.iter().find(|e| {
        graph.nodes[e.from as usize].coord.haversine_m(&from_coord) < 1.0
            && graph.nodes[e.to as usize].coord.haversine_m(&to_coord) < 1.0
    })
}

const fn ll(lat: f64, lon: f64) -> LatLng {
    LatLng::new(lat, lon)
}

#[test]
fn excluded_highways_produce_no_edges_others_are_all_bidirectional() {
    let graph =
        StreetGraph::build_from_osm_xml(&fixture_path("tags.osm")).expect("fixture should build");

    // 9本の歩行可能 way (各1区間) が双方向化されて 18 ノード・18 エッジになるはず。
    // motorway / foot=no / cycleway の3本 (highway=cycleway は歩行可能集合に無い) は
    // ノードごと除外される。
    assert_eq!(
        graph.nodes.len(),
        18,
        "9 walkable ways x 2 nodes each, 3 excluded ways contribute 0"
    );
    assert_eq!(graph.edges.len(), 18, "9 walkable ways x 2 directions");

    // 除外された3本の座標帯にはノードが一切存在しないはず。
    for lon in [139.70090, 139.70100, 139.70110] {
        assert!(
            find_edge(&graph, ll(35.70000, lon), ll(35.70010, lon)).is_none(),
            "excluded way at lon={lon} should not produce an edge"
        );
    }
}

#[test]
fn steps_and_elevator_tags_are_extracted() {
    let graph =
        StreetGraph::build_from_osm_xml(&fixture_path("tags.osm")).expect("fixture should build");

    let steps =
        find_edge(&graph, ll(35.70000, 139.70000), ll(35.70010, 139.70000)).expect("steps way");
    assert!(steps.has_stairs);
    assert!(!steps.has_elevator);

    let elevator_highway = find_edge(&graph, ll(35.70000, 139.70010), ll(35.70010, 139.70010))
        .expect("highway=elevator way");
    assert!(elevator_highway.has_elevator);
    assert!(!elevator_highway.has_stairs);

    let elevator_tag = find_edge(&graph, ll(35.70000, 139.70020), ll(35.70010, 139.70020))
        .expect("elevator=yes tag way");
    assert!(elevator_tag.has_elevator);
}

#[test]
fn wheelchair_tag_maps_to_option_bool_with_limited_as_unknown() {
    let graph =
        StreetGraph::build_from_osm_xml(&fixture_path("tags.osm")).expect("fixture should build");

    let yes = find_edge(&graph, ll(35.70000, 139.70030), ll(35.70010, 139.70030))
        .expect("wheelchair=yes way");
    assert_eq!(yes.wheelchair, Some(true));

    let no = find_edge(&graph, ll(35.70000, 139.70040), ll(35.70010, 139.70040))
        .expect("wheelchair=no way");
    assert_eq!(no.wheelchair, Some(false));

    let limited = find_edge(&graph, ll(35.70000, 139.70050), ll(35.70010, 139.70050))
        .expect("wheelchair=limited way");
    assert_eq!(
        limited.wheelchair, None,
        "limited は Option<bool> で表現できないため unknown 扱い"
    );
}

#[test]
fn incline_percent_is_parsed_as_absolute_value_non_numeric_is_none() {
    let graph =
        StreetGraph::build_from_osm_xml(&fixture_path("tags.osm")).expect("fixture should build");

    let positive = find_edge(&graph, ll(35.70000, 139.70060), ll(35.70010, 139.70060))
        .expect("incline=8% way");
    assert_eq!(positive.max_slope_pct, Some(8.0));

    let negative = find_edge(&graph, ll(35.70000, 139.70070), ll(35.70010, 139.70070))
        .expect("incline=-10% way");
    assert_eq!(
        negative.max_slope_pct,
        Some(10.0),
        "符号は無視して絶対値にする"
    );

    let qualitative = find_edge(&graph, ll(35.70000, 139.70080), ll(35.70010, 139.70080))
        .expect("incline=up way");
    assert_eq!(
        qualitative.max_slope_pct, None,
        "数値でない incline はパース不能として None"
    );
}

#[test]
fn route_snaps_to_nearest_node_even_when_query_coord_is_slightly_off() {
    let graph =
        StreetGraph::build_from_osm_xml(&fixture_path("tags.osm")).expect("fixture should build");

    // steps way (node 1: 35.70000,139.70000 <-> node 2: 35.70010,139.70000) の
    // 近傍座標 (数メートルずれ) からスナップして経路が引けることを確認する。
    let near_start = ll(35.700001, 139.700001);
    let near_end = ll(35.700099, 139.700001);
    let path = graph
        .route(near_start, near_end, &WalkProfile::normal())
        .expect("should snap and route");

    assert!(path.has_stairs);
    // haversine(node1, node2) は約 11.1m (0.0001 deg lat)。スナップ後の実距離もその程度のはず。
    assert!(
        (5.0..20.0).contains(&path.distance_m),
        "distance was {}",
        path.distance_m
    );
}

#[test]
fn route_returns_not_found_when_graph_has_no_connecting_path() {
    // 2つの孤立した区間だけを持つグラフ (共有ノード無し)。
    let xml = r#"<osm version="0.6">
        <node id="1" lat="35.0" lon="139.0"/>
        <node id="2" lat="35.001" lon="139.0"/>
        <way id="1"><nd ref="1"/><nd ref="2"/><tag k="highway" v="footway"/></way>
        <node id="3" lat="36.0" lon="140.0"/>
        <node id="4" lat="36.001" lon="140.0"/>
        <way id="2"><nd ref="3"/><nd ref="4"/><tag k="highway" v="footway"/></way>
    </osm>"#;
    let graph = StreetGraph::build_from_osm_xml_str(xml);

    let result = graph.route(ll(35.0, 139.0), ll(36.0, 140.0), &WalkProfile::normal());
    assert!(
        result.is_err(),
        "disconnected components should yield no route"
    );
}
