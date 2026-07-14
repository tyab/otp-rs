//! otp-rs 全体で共有する基本型。
//!
//! OTP の `org.opentripplanner.framework` / `...transit.model.basic` 相当。

use std::fmt;

/// WGS84 緯度経度。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatLng {
    pub lat: f64,
    pub lng: f64,
}

impl LatLng {
    pub const fn new(lat: f64, lng: f64) -> Self {
        Self { lat, lng }
    }

    /// 2点間のハバースイン距離 (メートル)。徒歩距離の下限見積り等に使う。
    pub fn haversine_m(&self, other: &LatLng) -> f64 {
        const R: f64 = 6_371_000.0; // 地球平均半径 (m)
        let (lat1, lat2) = (self.lat.to_radians(), other.lat.to_radians());
        let dlat = (other.lat - self.lat).to_radians();
        let dlng = (other.lng - self.lng).to_radians();
        let a = (dlat / 2.0).sin().powi(2)
            + lat1.cos() * lat2.cos() * (dlng / 2.0).sin().powi(2);
        2.0 * R * a.sqrt().asin()
    }
}

/// その日の 00:00 からの経過秒。GTFS は 24:00 超 (翌日跨ぎ) も許容するため i32。
pub type SecondsSinceMidnight = i32;

/// 文字列 ID の newtype をまとめて定義する。
macro_rules! string_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_id!(/// 停留所/駅 ID
    StopId);
string_id!(/// 路線 ID
    RouteId);
string_id!(/// 便 ID
    TripId);
string_id!(/// 運行日サービス ID
    ServiceId);
string_id!(/// 事業者 ID
    AgencyId);
string_id!(/// 運賃 ID
    FareId);

/// otp-rs 共通エラー。
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// 入力データのパース失敗 (どのファイル/行かを含める)。
    Parse(String),
    /// 参照先が見つからない (未知の stop_id 等)。
    NotFound(String),
    /// 未実装のコードパスに到達 (移植途上のスライス)。
    Unimplemented(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Parse(s) => write!(f, "parse error: {s}"),
            Error::NotFound(s) => write!(f, "not found: {s}"),
            Error::Unimplemented(s) => write!(f, "unimplemented: {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haversine_shinjuku_to_tokyo_is_about_6km() {
        let shinjuku = LatLng::new(35.690, 139.700);
        let tokyo = LatLng::new(35.681, 139.767);
        let d = shinjuku.haversine_m(&tokyo);
        // 実距離はおよそ 6.1km。ハバースイン(直線)なので 5.5〜6.5km に入れば妥当。
        assert!((5_500.0..=6_500.0).contains(&d), "distance was {d}");
    }

    #[test]
    fn id_roundtrips() {
        let s = StopId::new("odpt.Station:Toei.Oedo.Shinjuku");
        assert_eq!(s.as_str(), "odpt.Station:Toei.Oedo.Shinjuku");
    }
}
