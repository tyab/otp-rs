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

    let vars = v.get("variables").cloned().unwrap_or(Value::Null);
    let origin = parse_coord(&vars, "origin").ok_or_else(|| ApiError::bad_request("origin coordinate missing"))?;
    let destination = parse_coord(&vars, "destination").ok_or_else(|| ApiError::bad_request("destination coordinate missing"))?;
    let (service_date, depart_at) = parse_datetime(vars.get("dateTime"));
    // babymobi は wheelchair=true/false の2クエリを投げる (engine.ts)。true=車いす相当
    // (段差を強く避ける)、false=通常徒歩。ベビーカーは true 由来経路を推奨に寄せる設計。
    let wheelchair = vars.get("wheelchair").and_then(Value::as_bool).unwrap_or(false);
    let mobility = if wheelchair { Mobility::Wheelchair } else { Mobility::Solo };

    let request = RouteRequest { origin, destination, depart_at, service_date, mobility };
    let itineraries = engine.plan(&request).map_err(|e| ApiError::internal(format!("plan failed: {e}")))?;

    let edges: Vec<Value> =
        itineraries.iter().map(|it| json!({ "node": itinerary_to_node(it, service_date, depart_at) })).collect();
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
/// (service_date=YYYYMMDD, depart_at=0時からの秒) に。null は現在時刻(JST)。
/// arriveBy(latestArrival) は本エンジンが出発時刻探索のみのため出発時刻として近似する。
fn parse_datetime(dt: Option<&Value>) -> (u32, i32) {
    let iso = dt.and_then(|d| {
        d.get("earliestDeparture").or_else(|| d.get("latestArrival")).and_then(Value::as_str)
    });
    match iso.and_then(parse_iso_datetime) {
        Some(v) => v,
        None => now_jst(),
    }
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

fn itinerary_to_node(it: &Itinerary, service_date: u32, depart_at: i32) -> Value {
    let start = format_iso(service_date, depart_at);
    let total = depart_at + it.total_duration_s as i32;
    let end = format_iso(add_days(service_date, (total.div_euclid(86400)) as i64), total.rem_euclid(86400));
    let legs: Vec<Value> = it.legs.iter().map(leg_to_json).collect();
    json!({ "start": start, "end": end, "legs": legs })
}

fn place_json(name: &str, c: LatLng) -> Value {
    json!({ "name": name, "lat": c.lat, "lon": c.lng })
}

fn geometry_json(coords: &[LatLng]) -> Value {
    json!({ "points": encode_polyline(coords), "length": coords.len() })
}

fn leg_to_json(leg: &Leg) -> Value {
    match leg {
        Leg::Walk { from_name, from_coord, to_name, to_coord, distance_m, duration_s, geometry, .. } => json!({
            "mode": "WALK",
            "duration": duration_s,
            "distance": distance_m,
            "from": place_json(from_name, *from_coord),
            "to": place_json(to_name, *to_coord),
            "route": Value::Null,
            "agency": Value::Null,
            "legGeometry": geometry_json(geometry),
        }),
        Leg::Transit { route_short_name, route_long_name, mode, agency_id, from_name, from_coord, to_name, to_coord, duration_s } => json!({
            "mode": mode,
            "duration": duration_s,
            "distance": from_coord.haversine_m(to_coord),
            "from": place_json(from_name, *from_coord),
            "to": place_json(to_name, *to_coord),
            "route": { "longName": route_long_name, "shortName": route_short_name },
            "agency": { "name": "", "gtfsId": agency_id },
            "legGeometry": geometry_json(&[*from_coord, *to_coord]),
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
