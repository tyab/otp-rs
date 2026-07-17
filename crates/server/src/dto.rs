//! `POST /plan` の JSON リクエスト/レスポンスの型。
//!
//! HTTP 層 (main.rs, tiny_http) から切り離してあるので、実サーバを起動せずに
//! シリアライズ/デシリアライズと変換ロジックだけを単体テストできる。
//!
//! リクエスト例:
//! ```json
//! {"origin":{"lat":35.690,"lng":139.700},"destination":{"lat":35.707,"lng":139.759},
//!  "departAt":"08:00","serviceDate":20260713,"mobility":"stroller"}
//! ```
//! `departAt` は `"HH:MM"` 文字列と 0時からの秒数(整数)のどちらも受け付ける
//! (`otp_engine::RouteRequest::depart_at` が `SecondsSinceMidnight` = `i32` のため)。

use serde::{Deserialize, Serialize};

use otp_core::LatLng;
use otp_engine::{IntermediateStop, Itinerary, Leg, Mobility, RouteRequest};

#[derive(Debug, Deserialize)]
pub struct LatLngDto {
    pub lat: f64,
    pub lng: f64,
}

/// `departAt`: `"HH:MM"` 文字列 か 0時からの秒数(整数)。
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum DepartAtDto {
    Clock(String),
    Seconds(i32),
}

#[derive(Debug, Deserialize)]
pub struct PlanRequestDto {
    pub origin: LatLngDto,
    pub destination: LatLngDto,
    #[serde(rename = "departAt")]
    pub depart_at: DepartAtDto,
    #[serde(rename = "serviceDate")]
    pub service_date: u32,
    pub mobility: String,
    /// arrive-by (到着時刻指定) フラグ。省略時は false (depart-at)。true のとき
    /// `departAt` は「到着締切時刻」を意味する (`otp_engine::RouteRequest::arrive_by`)。
    #[serde(rename = "arriveBy", default)]
    pub arrive_by: bool,
}

impl PlanRequestDto {
    /// `otp_engine::RouteRequest` へ変換する。`departAt` のパース失敗や未知の
    /// `mobility` は `Err(理由)` にする (呼び出し側が 400 に変換する)。
    pub fn into_route_request(self) -> Result<RouteRequest, String> {
        let depart_at = match self.depart_at {
            DepartAtDto::Seconds(s) => s,
            DepartAtDto::Clock(s) => parse_hhmm(&s)?,
        };
        let mobility = match self.mobility.as_str() {
            "solo" => Mobility::Solo,
            "stroller" => Mobility::Stroller,
            "wheelchair" => Mobility::Wheelchair,
            other => return Err(format!("不明な mobility: {other} (solo|stroller|wheelchair のいずれか)")),
        };
        Ok(RouteRequest {
            origin: LatLng::new(self.origin.lat, self.origin.lng),
            destination: LatLng::new(self.destination.lat, self.destination.lng),
            depart_at,
            service_date: self.service_date,
            mobility,
            arrive_by: self.arrive_by,
        })
    }
}

fn parse_hhmm(s: &str) -> Result<i32, String> {
    let (h, m) = s.split_once(':').ok_or_else(|| format!("departAt は \"HH:MM\" か秒数で指定してください: {s:?}"))?;
    let h: i32 = h.parse().map_err(|_| format!("departAt の時が不正です: {s:?}"))?;
    let m: i32 = m.parse().map_err(|_| format!("departAt の分が不正です: {s:?}"))?;
    Ok(h * 3600 + m * 60)
}

#[derive(Debug, Serialize)]
pub struct PlanResponseDto {
    pub itineraries: Vec<ItineraryDto>,
}

#[derive(Debug, Serialize)]
pub struct ItineraryDto {
    #[serde(rename = "totalDurationS")]
    pub total_duration_s: u32,
    pub transfers: u8,
    #[serde(rename = "fareYen")]
    pub fare_yen: Option<f64>,
    pub legs: Vec<LegDto>,
}

/// 座標の出力用 DTO。
#[derive(Debug, Serialize)]
pub struct CoordDto {
    pub lat: f64,
    pub lng: f64,
}

impl From<LatLng> for CoordDto {
    fn from(c: LatLng) -> Self {
        CoordDto { lat: c.lat, lng: c.lng }
    }
}

/// 乗車区間が通過する中間駅の出力用 DTO (途中下車提案用)。
///
/// REST `/plan` は service_date を DTO 層まで通していないため、GraphQL 側の ISO 時刻
/// (`arrivalTime`) の代わりに 0時からの生秒 (`arrivalSecondsSinceMidnight`) を出す。
/// 座標フィールドは GraphQL と揃えて `lat`/`lon`。
#[derive(Debug, Serialize)]
pub struct IntermediateStopDto {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    #[serde(rename = "arrivalSecondsSinceMidnight")]
    pub arrival_s: i32,
    #[serde(rename = "secondsFromBoard")]
    pub seconds_from_board: u32,
}

impl From<&IntermediateStop> for IntermediateStopDto {
    fn from(s: &IntermediateStop) -> Self {
        IntermediateStopDto {
            name: s.name.clone(),
            lat: s.coord.lat,
            lon: s.coord.lng,
            arrival_s: s.arrival_s,
            seconds_from_board: s.seconds_from_board,
        }
    }
}

/// `mode` タグで WALK/TRANSIT を判別する (フロント側が `switch(leg.mode)` できる形)。
#[derive(Debug, Serialize)]
#[serde(tag = "mode")]
pub enum LegDto {
    #[serde(rename = "WALK")]
    Walk {
        #[serde(rename = "fromName")]
        from_name: String,
        #[serde(rename = "toName")]
        to_name: String,
        #[serde(rename = "distanceM")]
        distance_m: f32,
        #[serde(rename = "durationS")]
        duration_s: u32,
        #[serde(rename = "hasStairs")]
        has_stairs: bool,
        /// 折れ線 (始点→終点)。
        geometry: Vec<CoordDto>,
        /// 徒歩は途中駅の概念が無いが、乗車 leg と形を揃えるため空配列を出す。
        #[serde(rename = "intermediateStops")]
        intermediate_stops: Vec<IntermediateStopDto>,
    },
    #[serde(rename = "TRANSIT")]
    Transit {
        #[serde(rename = "routeName")]
        route_name: String,
        #[serde(rename = "routeLongName")]
        route_long_name: String,
        /// SUBWAY/RAIL/TRAM/BUS。
        #[serde(rename = "transitMode")]
        transit_mode: String,
        #[serde(rename = "fromName")]
        from_name: String,
        #[serde(rename = "toName")]
        to_name: String,
        #[serde(rename = "durationS")]
        duration_s: u32,
        /// 乗車駅と降車駅の間に停車する中間駅 (途中下車提案用)。
        #[serde(rename = "intermediateStops")]
        intermediate_stops: Vec<IntermediateStopDto>,
    },
}

impl PlanResponseDto {
    pub fn from_itineraries(itineraries: &[Itinerary]) -> Self {
        Self { itineraries: itineraries.iter().map(ItineraryDto::from_itinerary).collect() }
    }
}

impl ItineraryDto {
    fn from_itinerary(it: &Itinerary) -> Self {
        Self {
            total_duration_s: it.total_duration_s,
            transfers: it.transfers,
            fare_yen: it.fare_yen,
            legs: it.legs.iter().map(LegDto::from_leg).collect(),
        }
    }
}

impl LegDto {
    fn from_leg(leg: &Leg) -> Self {
        match leg {
            Leg::Walk { from_name, to_name, distance_m, duration_s, has_stairs, geometry, .. } => LegDto::Walk {
                from_name: from_name.clone(),
                to_name: to_name.clone(),
                distance_m: *distance_m,
                duration_s: *duration_s,
                has_stairs: *has_stairs,
                geometry: geometry.iter().map(|c| (*c).into()).collect(),
                intermediate_stops: vec![],
            },
            Leg::Transit { route_short_name, route_long_name, mode, from_name, to_name, duration_s, intermediate_stops, .. } => LegDto::Transit {
                route_name: route_short_name.clone(),
                route_long_name: route_long_name.clone(),
                transit_mode: mode.to_string(),
                from_name: from_name.clone(),
                to_name: to_name.clone(),
                duration_s: *duration_s,
                intermediate_stops: intermediate_stops.iter().map(IntermediateStopDto::from).collect(),
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ErrorDto {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_request_deserializes_with_clock_string_depart_at() {
        let body = r#"{"origin":{"lat":35.69,"lng":139.70},"destination":{"lat":35.707,"lng":139.759},
                        "departAt":"08:15","serviceDate":20260713,"mobility":"stroller"}"#;
        let dto: PlanRequestDto = serde_json::from_str(body).expect("should deserialize");
        let req = dto.into_route_request().expect("should convert");
        assert_eq!(req.depart_at, 8 * 3600 + 15 * 60);
        assert_eq!(req.service_date, 20260713);
        assert_eq!(req.mobility, Mobility::Stroller);
        assert!((req.origin.lat - 35.69).abs() < 1e-9);
    }

    #[test]
    fn plan_request_deserializes_with_numeric_seconds_depart_at() {
        let body = r#"{"origin":{"lat":35.69,"lng":139.70},"destination":{"lat":35.707,"lng":139.759},
                        "departAt":29700,"serviceDate":20260713,"mobility":"solo"}"#;
        let dto: PlanRequestDto = serde_json::from_str(body).expect("should deserialize");
        let req = dto.into_route_request().expect("should convert");
        assert_eq!(req.depart_at, 29700);
        assert_eq!(req.mobility, Mobility::Solo);
    }

    #[test]
    fn arrive_by_defaults_false_and_parses_when_present() {
        // arriveBy 省略時は depart-at (false)。
        let body = r#"{"origin":{"lat":35.69,"lng":139.70},"destination":{"lat":35.707,"lng":139.759},
                        "departAt":"08:00","serviceDate":20260713,"mobility":"solo"}"#;
        let req = serde_json::from_str::<PlanRequestDto>(body).unwrap().into_route_request().unwrap();
        assert!(!req.arrive_by, "arriveBy 省略時は false");

        // arriveBy:true なら arrive-by (departAt は到着締切として扱われる)。
        let body = r#"{"origin":{"lat":35.69,"lng":139.70},"destination":{"lat":35.707,"lng":139.759},
                        "departAt":"09:00","serviceDate":20260713,"mobility":"solo","arriveBy":true}"#;
        let req = serde_json::from_str::<PlanRequestDto>(body).unwrap().into_route_request().unwrap();
        assert!(req.arrive_by, "arriveBy:true は arrive_by=true");
        assert_eq!(req.depart_at, 9 * 3600, "departAt は arrive-by でも時刻フィールド (到着締切)");
    }

    #[test]
    fn unknown_mobility_is_rejected() {
        let body = r#"{"origin":{"lat":35.69,"lng":139.70},"destination":{"lat":35.707,"lng":139.759},
                        "departAt":"08:00","serviceDate":20260713,"mobility":"bicycle"}"#;
        let dto: PlanRequestDto = serde_json::from_str(body).expect("should deserialize");
        let err = dto.into_route_request().expect_err("should reject unknown mobility");
        assert!(err.contains("bicycle"), "err was: {err}");
    }

    #[test]
    fn malformed_clock_depart_at_is_rejected() {
        let body = r#"{"origin":{"lat":35.69,"lng":139.70},"destination":{"lat":35.707,"lng":139.759},
                        "departAt":"not-a-time","serviceDate":20260713,"mobility":"solo"}"#;
        let dto: PlanRequestDto = serde_json::from_str(body).expect("should deserialize");
        assert!(dto.into_route_request().is_err());
    }

    #[test]
    fn itinerary_serializes_with_tagged_leg_modes() {
        let itineraries = vec![Itinerary {
            legs: vec![
                Leg::Walk {
                    from_name: "出発地".to_string(),
                    from_coord: LatLng::new(35.69, 139.70),
                    to_name: "新宿".to_string(),
                    to_coord: LatLng::new(35.691, 139.70),
                    distance_m: 120.5,
                    duration_s: 90,
                    has_stairs: false,
                    has_elevator: false,
                    geometry: vec![LatLng::new(35.69, 139.70), LatLng::new(35.691, 139.70)],
                },
                Leg::Transit {
                    route_short_name: "新宿線".to_string(),
                    route_long_name: "都営新宿線".to_string(),
                    mode: "SUBWAY",
                    agency_id: "toei".to_string(),
                    from_name: "新宿".to_string(),
                    from_coord: LatLng::new(35.691, 139.70),
                    to_name: "本郷三丁目".to_string(),
                    to_coord: LatLng::new(35.707, 139.759),
                    duration_s: 1200,
                    intermediate_stops: vec![IntermediateStop {
                        name: "市ヶ谷".to_string(),
                        coord: LatLng::new(35.693, 139.735),
                        arrival_s: 8 * 3600 + 600,
                        seconds_from_board: 600,
                    }],
                },
            ],
            total_duration_s: 1400,
            transfers: 0,
            fare_yen: Some(220.0),
            depart_s: 8 * 3600,
        }];
        let dto = PlanResponseDto::from_itineraries(&itineraries);
        let json = serde_json::to_string(&dto).expect("should serialize");
        assert!(json.contains("\"mode\":\"WALK\""), "json was: {json}");
        assert!(json.contains("\"mode\":\"TRANSIT\""), "json was: {json}");
        assert!(json.contains("\"totalDurationS\":1400"));
        assert!(json.contains("\"fareYen\":220.0"));
        assert!(json.contains("\"routeName\":\"新宿線\""));
        assert!(json.contains("\"routeLongName\":\"都営新宿線\""));
        // 乗車 leg の中間駅が REST DTO にも出る (ISO ではなく生秒 + 経過秒)。
        assert!(json.contains("\"intermediateStops\":[{"), "中間駅配列があるはず: {json}");
        assert!(json.contains("\"name\":\"市ヶ谷\""), "中間駅名: {json}");
        assert!(json.contains("\"arrivalSecondsSinceMidnight\":29400"), "中間駅の生秒: {json}");
        assert!(json.contains("\"secondsFromBoard\":600"), "乗車からの経過秒: {json}");
    }

    #[test]
    fn error_dto_serializes_as_error_field() {
        let dto = ErrorDto { error: "bad request".to_string() };
        let json = serde_json::to_string(&dto).unwrap();
        assert_eq!(json, r#"{"error":"bad request"}"#);
    }
}
