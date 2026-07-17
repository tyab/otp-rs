//! HTTP (tiny_http) 非依存のリクエストハンドラ本体。
//!
//! TCP を介さず「JSONバイト列 in → JSON文字列 out」の純粋関数として書くことで、
//! 実サーバを起動せずに小さな実 `Engine` (fixture) を使ったユニットテストができる
//! (`tests/plan_handler.rs`)。main.rs はこの関数を呼び、tiny_http の
//! `Request`/`Response` に薄く配線するだけにする。

use otp_core::LatLng;
use otp_engine::{Engine, Itinerary, Leg, Mobility, RouteRequest};
use serde_json::{json, Value};

use crate::dto::{ErrorDto, PlanRequestDto, PlanResponseDto};

/// ハンドラ内で起きたエラー。HTTP ステータスコードと JSON 本文の元になるメッセージを持つ。
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self { status: 400, message: message.into() }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self { status: 500, message: message.into() }
    }

    /// `{"error": "..."}` 形式の JSON 本文にする。
    pub fn to_json(&self) -> String {
        error_json(&self.message)
    }
}

/// `{"error": "..."}` 形式の JSON 文字列を作る。シリアライズ自体が失敗することは
/// 通常無い (String のみの構造体) が、万一失敗しても固定の JSON でフォールバックする
/// (エラー応答の生成でさらにパニックしないため)。
pub fn error_json(message: &str) -> String {
    serde_json::to_string(&ErrorDto { error: message.to_string() }).unwrap_or_else(|_| "{\"error\":\"internal error\"}".to_string())
}

/// `GET /health` の本体。
pub fn health_json() -> String {
    "{\"status\":\"ok\"}".to_string()
}

/// `POST /plan` の本体。リクエストボディ (JSON バイト列) を受け取り、成功なら
/// レスポンス本文 (JSON 文字列) を、失敗なら [`ApiError`] (ステータス+メッセージ) を返す。
///
/// 想定するエラー系統:
/// - JSON 自体が不正 / スキーマに合わない → 400
/// - `departAt` のパース失敗・未知の `mobility` → 400
/// - `Engine::plan` がエラーを返す (RAPTOR 内部エラー等) → 500
/// - 応答 JSON のシリアライズ失敗 (通常起きない) → 500
///
/// `Engine::plan` 自体がパニックした場合はこの関数の外 (呼び出し側の
/// `std::panic::catch_unwind`, `main.rs` 参照) で拾う。
pub fn handle_plan(engine: &Engine, body: &[u8]) -> Result<String, ApiError> {
    let req_dto: PlanRequestDto = serde_json::from_slice(body).map_err(|e| ApiError::bad_request(format!("invalid JSON: {e}")))?;
    let request = req_dto.into_route_request().map_err(ApiError::bad_request)?;

    let itineraries = engine.plan(&request).map_err(|e| ApiError::internal(format!("plan failed: {e}")))?;

    let response = PlanResponseDto::from_itineraries(&itineraries);
    serde_json::to_string(&response).map_err(|e| ApiError::internal(format!("failed to serialize response: {e}")))
}

// ───────────────────────── OTP2 GraphQL 互換 (planConnection) ─────────────────────────
//
// babymobi の apps/api/src/otp/client.ts は OTP2 の `POST /otp/gtfs/v1` に固定の
// `planConnection` クエリを投げ、`edges[].node.legs[]` を mapper.ts で Route に変換する。
// JVM OTP をこのサーバに置き換えても babymobi 側を無改修にするため、その1クエリぶんだけ
// OTP のレスポンス形状を模して返す (完全な GraphQL 実装ではなく、変数を読んで Engine::plan
// を回し、必要なフィールドだけ整形する)。

/// `POST /otp/gtfs/v1` の本体。ヘルス (`{ __typename }`) と planConnection の2種を扱う。
pub fn handle_gtfs_graphql(engine: &Engine, body: &[u8]) -> Result<String, ApiError> {
    let v: Value = serde_json::from_slice(body).map_err(|e| ApiError::bad_request(format!("invalid JSON: {e}")))?;
    let query = v.get("query").and_then(Value::as_str).unwrap_or("");

    // ヘルスチェック (client.ts checkOtpHealth の `{ __typename }`)。
    if query.contains("__typename") && !query.contains("planConnection") {
        return Ok(r#"{"data":{"__typename":"QueryType"}}"#.to_string());
    }

    // 前提: babymobi の client.ts が投げる固定クエリの変数形状 (トップレベルに
    // origin/destination/dateTime/wheelchair をそのまま渡す) を読む。汎用 GraphQL
    // パーサではないので、変数名や dateTime のネスト形が変わるとここを追随させること
    // (Codexレビュー指摘 P1: 別形状のクエリでは now_jst/Solo にフォールバックする)。
    let vars = v.get("variables").cloned().unwrap_or(Value::Null);
    let origin = parse_coord(&vars, "origin").ok_or_else(|| ApiError::bad_request("origin coordinate missing"))?;
    let destination = parse_coord(&vars, "destination").ok_or_else(|| ApiError::bad_request("destination coordinate missing"))?;
    let (service_date, depart_at, arrive_by) = parse_datetime(vars.get("dateTime"));
    // babymobi は wheelchair=true/false の2クエリを投げる (engine.ts)。true=車いす相当
    // (段差を強く避ける)、false=通常徒歩。ベビーカーは true 由来経路を推奨に寄せる設計。
    let wheelchair = vars.get("wheelchair").and_then(Value::as_bool).unwrap_or(false);
    let mobility = if wheelchair { Mobility::Wheelchair } else { Mobility::Solo };

    // arrive_by のとき depart_at は「到着締切時刻」を表す (engine が後方 RAPTOR を回す)。
    let request = RouteRequest { origin, destination, depart_at, service_date, mobility, arrive_by };
    let itineraries = engine.plan(&request).map_err(|e| ApiError::internal(format!("plan failed: {e}")))?;

    // start/end の anchor は各経路の実出発時刻 (`Itinerary::depart_s`)。arrive-by では締切から
    // 逆算した最遅出発で経路ごとに異なるため、単一の depart_at では anchor できない。
    let edges: Vec<Value> =
        itineraries.iter().map(|it| json!({ "node": itinerary_to_node(it, service_date) })).collect();
    let resp = json!({ "data": { "planConnection": { "routingErrors": [], "edges": edges } } });
    Ok(resp.to_string())
}

/// `variables[key].location.coordinate.{latitude,longitude}` を LatLng に。
fn parse_coord(vars: &Value, key: &str) -> Option<LatLng> {
    let coord = vars.get(key)?.get("location")?.get("coordinate")?;
    let lat = coord.get("latitude")?.as_f64()?;
    let lon = coord.get("longitude")?.as_f64()?;
    Some(LatLng::new(lat, lon))
}

/// `dateTime` (earliestDeparture / latestArrival の ISO8601、または null) を
/// (service_date=YYYYMMDD, time=0時からの秒, arrive_by) に。null は現在時刻(JST)の depart-at。
///
/// `latestArrival` が入っていれば arrive-by (到着締切) モード = `time` は到着締切時刻、
/// `arrive_by=true`。`earliestDeparture` なら depart-at (出発時刻) モード = `arrive_by=false`。
/// エンジンが後方 RAPTOR (最遅出発) を実装したため、arriveBy を出発時刻として近似することは
/// もう無い (latestArrival はそのまま締切として `RouteRequest.arrive_by` に流す)。
fn parse_datetime(dt: Option<&Value>) -> (u32, i32, bool) {
    // earliestDeparture を優先し、無ければ latestArrival (arriveBy) を見る。どちらが
    // 入っていたかで depart-at / arrive-by を判別する。
    let departure = dt.and_then(|d| d.get("earliestDeparture").and_then(Value::as_str));
    if let Some((date, s)) = departure.and_then(parse_iso_datetime) {
        return (date, s, false);
    }
    let arrival = dt.and_then(|d| d.get("latestArrival").and_then(Value::as_str));
    if let Some((date, s)) = arrival.and_then(parse_iso_datetime) {
        return (date, s, true);
    }
    let (date, s) = now_jst();
    (date, s, false)
}

/// "YYYY-MM-DDThh:mm:ss[±hh:mm|Z]" から (YYYYMMDD, 0時からの秒)。TZ は無視 (GTFS も
/// リクエストも JST 前提)。固定位置スライスで読む簡易実装。
fn parse_iso_datetime(s: &str) -> Option<(u32, i32)> {
    let (date, time) = s.split_once('T')?;
    let mut dparts = date.split('-');
    let y: u32 = dparts.next()?.parse().ok()?;
    let m: u32 = dparts.next()?.parse().ok()?;
    let d: u32 = dparts.next()?.parse().ok()?;
    let time = &time[..time.len().min(8)]; // "hh:mm:ss" まで (TZ 部を落とす)
    let mut tparts = time.split(':');
    let hh: i32 = tparts.next()?.parse().ok()?;
    let mm: i32 = tparts.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let ss: i32 = tparts.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    Some((y * 10000 + m * 100 + d, hh * 3600 + mm * 60 + ss))
}

/// 現在時刻 (JST) を (YYYYMMDD, 0時からの秒) で。
fn now_jst() -> (u32, i32) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let jst = secs + 9 * 3600;
    let (y, m, d) = civil_from_days(jst.div_euclid(86400));
    ((y as u32) * 10000 + (m as u32) * 100 + d as u32, jst.rem_euclid(86400) as i32)
}

fn itinerary_to_node(it: &Itinerary, service_date: u32) -> Value {
    // start = 実出発時刻 (depart-at では req.depart_at と一致、arrive-by では最遅出発)。
    // end = start + total_duration = 実到着時刻 (arrive-by では締切 T 以下)。depart-at では
    // depart_s ∈ [0,86400) なので日跨ぎ補正は end 側だけに効き、従来出力と一致する。
    let depart_s = it.depart_s;
    let total = depart_s + it.total_duration_s as i32;
    let start = format_iso(add_days(service_date, (depart_s.div_euclid(86400)) as i64), depart_s.rem_euclid(86400));
    let end = format_iso(add_days(service_date, (total.div_euclid(86400)) as i64), total.rem_euclid(86400));
    let legs: Vec<Value> = it.legs.iter().map(|l| leg_to_json(l, service_date)).collect();
    json!({ "start": start, "end": end, "legs": legs })
}

fn place_json(name: &str, c: LatLng) -> Value {
    json!({ "name": name, "lat": c.lat, "lon": c.lng })
}

fn geometry_json(coords: &[LatLng]) -> Value {
    json!({ "points": encode_polyline(coords), "length": coords.len() })
}

/// 乗車 leg の地図折れ線を、乗車駅 → 中間停車駅 (実トリップの停車列, 両端を除く) →
/// 降車駅 の順につなぐ。
///
/// `shapes.txt` (実運行の線形) は現状どのフィードにも無く (実測: 都営/メトロ/りんかい/
/// 京王/東武/自前頻度 GTFS のいずれも同梱せず) `Feed` も読み込んでいないため、
/// 停車駅の順序に沿う折れ線で近似する (2点直線より実際の駅列に沿う)。停車駅の座標は
/// GTFS `stops.txt` 由来の実データで、でっち上げは無い。中間駅が無ければ従来どおり
/// 乗車駅→降車駅の2点線になる。将来 `shapes.txt` を取り込めたら、その線形を
/// board→alight 区間にクリップした折れ線を優先するのが望ましい (README 参照)。
fn transit_leg_polyline(from: LatLng, intermediate: &[otp_engine::IntermediateStop], to: LatLng) -> Vec<LatLng> {
    let mut pts = Vec::with_capacity(intermediate.len() + 2);
    pts.push(from);
    pts.extend(intermediate.iter().map(|s| s.coord));
    pts.push(to);
    pts
}

fn leg_to_json(leg: &Leg, service_date: u32) -> Value {
    match leg {
        Leg::Walk { from_name, from_coord, to_name, to_coord, distance_m, duration_s, has_stairs, has_elevator, geometry } => json!({
            "mode": "WALK",
            "duration": duration_s,
            "distance": distance_m,
            "from": place_json(from_name, *from_coord),
            "to": place_json(to_name, *to_coord),
            "route": Value::Null,
            "agency": Value::Null,
            // OTP 標準の Leg には無い拡張フィールド (babymobi mapper が読む)。徒歩区間の
            // 段差/エレベーターは OSM 由来の実データ。
            "hasStairs": has_stairs,
            "hasElevator": has_elevator,
            "legGeometry": geometry_json(geometry),
            // 徒歩は途中駅の概念が無いが、乗車 leg と形を揃えるため空配列を出す。
            "intermediateStops": Vec::<Value>::new(),
        }),
        Leg::Transit { route_short_name, route_long_name, mode, agency_id, from_name, from_coord, to_name, to_coord, duration_s, intermediate_stops } => json!({
            "mode": mode,
            "duration": duration_s,
            "distance": from_coord.haversine_m(to_coord),
            "from": place_json(from_name, *from_coord),
            "to": place_json(to_name, *to_coord),
            "route": { "longName": route_long_name, "shortName": route_short_name },
            "agency": { "name": "", "gtfsId": agency_id },
            "legGeometry": geometry_json(&transit_leg_polyline(*from_coord, intermediate_stops, *to_coord)),
            // 乗車駅と降車駅の間に停車する中間駅 (途中下車提案用)。arrivalTime は
            // itinerary.start/end と同じ日跨ぎ補正・ISO 形式で揃える。
            "intermediateStops": intermediate_stops.iter().map(|s| json!({
                "name": s.name,
                "lat": s.coord.lat,
                "lon": s.coord.lng,
                "arrivalTime": format_iso(add_days(service_date, (s.arrival_s.div_euclid(86400)) as i64), s.arrival_s.rem_euclid(86400)),
                "secondsFromBoard": s.seconds_from_board,
            })).collect::<Vec<_>>(),
        }),
    }
}

/// Google encoded polyline (precision 5)。babymobi の decodePolyline と対。
fn encode_polyline(coords: &[LatLng]) -> String {
    let mut out = String::new();
    let (mut prev_lat, mut prev_lng) = (0i64, 0i64);
    for c in coords {
        let lat = (c.lat * 1e5).round() as i64;
        let lng = (c.lng * 1e5).round() as i64;
        encode_diff(lat - prev_lat, &mut out);
        encode_diff(lng - prev_lng, &mut out);
        prev_lat = lat;
        prev_lng = lng;
    }
    out
}

fn encode_diff(num: i64, out: &mut String) {
    let mut sgn = num << 1;
    if num < 0 {
        sgn = !sgn;
    }
    while sgn >= 0x20 {
        out.push((((0x20 | (sgn & 0x1f)) + 63) as u8) as char);
        sgn >>= 5;
    }
    out.push(((sgn + 63) as u8) as char);
}

fn format_iso(yyyymmdd: u32, sod: i32) -> String {
    let (y, m, d) = (yyyymmdd / 10000, (yyyymmdd / 100) % 100, yyyymmdd % 100);
    let (hh, mm, ss) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}+09:00")
}

/// YYYYMMDD に n 日足す。境界を跨ぐ到着時刻の日付ロールに使う。
fn add_days(yyyymmdd: u32, n: i64) -> u32 {
    let (y, m, d) = ((yyyymmdd / 10000) as i64, ((yyyymmdd / 100) % 100) as i64, (yyyymmdd % 100) as i64);
    let (ny, nm, nd) = civil_from_days(days_from_civil(y, m, d) + n);
    (ny as u32) * 10000 + (nm as u32) * 100 + nd as u32
}

// 暦日 ↔ 1970-01-01 からの通日 (Howard Hinnant のアルゴリズム)。対象は現代の
// 日付 (y>=0, z>=0) のみで、Rust の切り捨て除算が floor 除算と一致する範囲で使う。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use otp_core::LatLng;
    use otp_gtfs::Feed;
    use otp_raptor::Timetable;
    use otp_street::StreetGraph;

    use super::*;

    // `crates/engine/tests/synthetic_door_to_door.rs` と同じ決定的フィクスチャ
    // (A→C(乗換)→D, 08:00発→08:30着) に、A/D駅それぞれの近傍だけをつなぐ小さな
    // 徒歩グラフを合わせる。実データ無しで CI が常に回せる。
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
        Engine::new(street, timetable, HashMap::new())
    }

    fn valid_body() -> String {
        format!(
            r#"{{"origin":{{"lat":{},"lng":{}}},"destination":{{"lat":{},"lng":{}}},
                "departAt":"07:57","serviceDate":20260713,"mobility":"solo"}}"#,
            ORIGIN_NEAR_A.lat, ORIGIN_NEAR_A.lng, DEST_NEAR_D.lat, DEST_NEAR_D.lng
        )
    }

    #[test]
    fn health_json_reports_ok() {
        assert_eq!(health_json(), r#"{"status":"ok"}"#);
    }

    #[test]
    fn parse_datetime_latest_arrival_sets_arrive_by_and_earliest_departure_does_not() {
        use serde_json::json;
        // latestArrival → arrive-by (到着締切) モード。
        let arrive = json!({ "latestArrival": "2026-07-13T09:00:00+09:00" });
        let (date, s, arrive_by) = parse_datetime(Some(&arrive));
        assert_eq!(date, 20260713);
        assert_eq!(s, 9 * 3600);
        assert!(arrive_by, "latestArrival は arrive_by=true のはず");

        // earliestDeparture → depart-at (出発時刻) モード。
        let depart = json!({ "earliestDeparture": "2026-07-13T08:00:00+09:00" });
        let (date, s, arrive_by) = parse_datetime(Some(&depart));
        assert_eq!(date, 20260713);
        assert_eq!(s, 8 * 3600);
        assert!(!arrive_by, "earliestDeparture は arrive_by=false のはず");

        // 両方あれば earliestDeparture を優先 (depart-at)。
        let both = json!({ "earliestDeparture": "2026-07-13T08:00:00+09:00", "latestArrival": "2026-07-13T09:00:00+09:00" });
        let (_, s, arrive_by) = parse_datetime(Some(&both));
        assert_eq!(s, 8 * 3600, "earliestDeparture 優先");
        assert!(!arrive_by);
    }

    #[test]
    fn gtfs_graphql_arrive_by_node_end_is_not_after_deadline() {
        // latestArrival (arrive-by) の planConnection では、返る node.end (実到着) が締切以下で
        // なければならない (mini fixture: A→C→D, 各便 08時台, D着 08:30)。締切 09:00 で問い合わせる。
        let engine = build_engine();
        let body = format!(
            r#"{{"query":"planConnection","variables":{{
                "origin":{{"location":{{"coordinate":{{"latitude":{},"longitude":{}}}}}}},
                "destination":{{"location":{{"coordinate":{{"latitude":{},"longitude":{}}}}}}},
                "dateTime":{{"latestArrival":"2026-07-13T09:00:00+09:00"}},
                "wheelchair":false}}}}"#,
            ORIGIN_NEAR_A.lat, ORIGIN_NEAR_A.lng, DEST_NEAR_D.lat, DEST_NEAR_D.lng
        );
        let json = handle_gtfs_graphql(&engine, body.as_bytes()).expect("should succeed");
        let v: Value = serde_json::from_str(&json).expect("valid json");
        let edges = v["data"]["planConnection"]["edges"].as_array().expect("edges array");
        assert!(!edges.is_empty(), "arrive-by で経路が返るはず: {json}");
        for edge in edges {
            let end = edge["node"]["end"].as_str().expect("end iso");
            // 締切 09:00 以下 (ISO 文字列は同一 TZ 前提で辞書順比較可)。
            assert!(end.as_bytes() <= "2026-07-13T09:00:00+09:00".as_bytes(), "node.end {end} は締切 09:00 以下のはず");
        }
    }

    #[test]
    fn gtfs_graphql_transit_leg_includes_intermediate_stops_with_names_and_times() {
        // mini fixture の T1 (A→B→C) は B駅 を途中に通過する。GraphQL の乗車 leg には
        // intermediateStops が1件以上入り、各要素に name と arrivalTime があるはず。
        let engine = build_engine();
        let body = format!(
            r#"{{"query":"planConnection","variables":{{
                "origin":{{"location":{{"coordinate":{{"latitude":{},"longitude":{}}}}}}},
                "destination":{{"location":{{"coordinate":{{"latitude":{},"longitude":{}}}}}}},
                "dateTime":{{"earliestDeparture":"2026-07-13T07:57:00+09:00"}},
                "wheelchair":false}}}}"#,
            ORIGIN_NEAR_A.lat, ORIGIN_NEAR_A.lng, DEST_NEAR_D.lat, DEST_NEAR_D.lng
        );
        let json = handle_gtfs_graphql(&engine, body.as_bytes()).expect("should succeed");
        let v: Value = serde_json::from_str(&json).expect("valid json");
        let edges = v["data"]["planConnection"]["edges"].as_array().expect("edges array");
        assert!(!edges.is_empty(), "経路が返るはず: {json}");

        // 先頭経路の乗車 leg のうち、A駅発 (T1) の leg に B駅 が中間駅として入る。
        let legs = edges[0]["node"]["legs"].as_array().expect("legs array");
        let first_transit = legs
            .iter()
            .find(|l| l["from"]["name"] == "A駅" && l["mode"] != "WALK")
            .expect("A駅発の乗車 leg があるはず");
        let stops = first_transit["intermediateStops"].as_array().expect("intermediateStops array");
        assert!(!stops.is_empty(), "中間駅が1件以上あるはず: {first_transit}");
        assert_eq!(stops[0]["name"], "B駅", "中間駅名が解決されるはず");
        assert!(stops[0]["arrivalTime"].as_str().is_some_and(|t| t.starts_with("2026-07-13T08:05:00")), "arrivalTime は ISO で 08:05: {}", stops[0]);
        assert_eq!(stops[0]["secondsFromBoard"], 300);

        // legGeometry は乗車駅→中間駅→降車駅 の停車列に沿う折れ線 (2点直線ではない)。
        // A駅発 T1 (A→B→C1) は中間駅 B が1つなので 頂点は 乗車A + 中間B + 降車C1 = 3点。
        let geom = &first_transit["legGeometry"];
        assert_eq!(geom["length"], stops.len() + 2, "折れ線頂点数 = 中間駅数 + 乗降2点: {first_transit}");
        // デコードして中間駅 B (35.01,139.01) の座標が折れ線の中に現れることを確認する
        // (直線チョードでは B は含まれない = 停車列に沿っている証拠)。
        let pts = decode_polyline(geom["points"].as_str().expect("points string"));
        assert!(
            pts.iter().any(|(lat, lon)| (lat - 35.01).abs() < 1e-6 && (lon - 139.01).abs() < 1e-6),
            "折れ線が中間駅 B(35.01,139.01) を通過するはず: {pts:?}"
        );
    }

    /// `encode_polyline` の逆。テストで折れ線頂点を復元する (Google encoded polyline, 精度5)。
    fn decode_polyline(s: &str) -> Vec<(f64, f64)> {
        let bytes = s.as_bytes();
        let mut i = 0;
        let (mut lat, mut lng) = (0i64, 0i64);
        let mut out = Vec::new();
        let mut read = |i: &mut usize| -> i64 {
            let mut shift = 0;
            let mut result = 0i64;
            loop {
                let b = (bytes[*i] as i64) - 63;
                *i += 1;
                result |= (b & 0x1f) << shift;
                shift += 5;
                if b < 0x20 {
                    break;
                }
            }
            if result & 1 != 0 {
                !(result >> 1)
            } else {
                result >> 1
            }
        };
        while i < bytes.len() {
            lat += read(&mut i);
            lng += read(&mut i);
            out.push((lat as f64 / 1e5, lng as f64 / 1e5));
        }
        out
    }

    #[test]
    fn handle_plan_returns_itineraries_json_for_known_route() {
        let engine = build_engine();
        let json = handle_plan(&engine, valid_body().as_bytes()).expect("should succeed");
        assert!(json.contains("\"itineraries\":["), "json was: {json}");
        assert!(json.contains("\"mode\":\"TRANSIT\""), "json was: {json}");
        assert!(json.contains("\"transfers\":1"), "json was: {json}");
    }

    #[test]
    fn handle_plan_returns_empty_itineraries_when_no_stop_nearby() {
        let engine = build_engine();
        let body = r#"{"origin":{"lat":36.0,"lng":139.0},"destination":{"lat":35.0305,"lng":139.0305},
                        "departAt":"07:57","serviceDate":20260713,"mobility":"solo"}"#;
        let json = handle_plan(&engine, body.as_bytes()).expect("should succeed (empty result is not an error)");
        assert_eq!(json, r#"{"itineraries":[]}"#);
    }

    #[test]
    fn handle_plan_rejects_malformed_json_with_400() {
        let engine = build_engine();
        let err = handle_plan(&engine, b"not json").expect_err("should reject malformed JSON");
        assert_eq!(err.status, 400);
    }

    #[test]
    fn handle_plan_rejects_missing_field_with_400() {
        let engine = build_engine();
        let err = handle_plan(&engine, br#"{"origin":{"lat":35.0,"lng":139.0}}"#).expect_err("should reject missing fields");
        assert_eq!(err.status, 400);
    }

    #[test]
    fn handle_plan_rejects_unknown_mobility_with_400() {
        let engine = build_engine();
        let body = r#"{"origin":{"lat":35.0,"lng":139.0},"destination":{"lat":35.03,"lng":139.03},
                        "departAt":"08:00","serviceDate":20260713,"mobility":"jetpack"}"#;
        let err = handle_plan(&engine, body.as_bytes()).expect_err("should reject unknown mobility");
        assert_eq!(err.status, 400);
        assert!(err.message.contains("jetpack"));
    }
}
