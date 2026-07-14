//! `otp-server` のライブラリ本体。
//!
//! JSON DTO ([`dto`]) と HTTP 非依存のハンドラ ([`handler`]) をここに置き、
//! tiny_http への配線 (`main.rs`) から切り離してユニットテストできるようにする。
//! バイナリ本体 (CLI 引数解析・GTFS/OSM ロード・HTTP 待受ループ) は `main.rs` の役目。

pub mod dto;
pub mod handler;
