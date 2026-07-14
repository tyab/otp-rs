//! HTTP (tiny_http) 非依存のリクエストハンドラ本体。
//!
//! TCP を介さず「JSONバイト列 in → JSON文字列 out」の純粋関数として書くことで、
//! 実サーバを起動せずに小さな実 `Engine` (fixture) を使ったユニットテストができる
//! (`tests/plan_handler.rs`)。main.rs はこの関数を呼び、tiny_http の
//! `Request`/`Response` に薄く配線するだけにする。

use otp_engine::Engine;

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
