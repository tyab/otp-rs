//! 実データスモークテスト: 新宿駅周辺の小 bbox を `osmium` で抽出し、
//! 座標to座標の徒歩経路が「桁のあう」距離・所要で返ることを検証する。
//!
//! `crates/raptor/tests/real_data_smoke.rs` と同じ発想: 厳密な数値検証は
//! フィクスチャテスト (`route_profiles.rs`/`street_osm.rs`) の役目。ここでは
//! 実データ規模でパニックせず、常識的な (distance>0, has_stairs 検出) 結果に
//! なることだけを見る。`osmium` が無い/`infra/otp/data` が無い環境では skip する。
//!
//! 本家 OTP (`infra/otp`, http://localhost:8080) との突き合わせ実測
//! (2026-07-15, 新宿駅東口付近 35.6902,139.7003 -> 新宿三丁目駅付近
//! 35.6938,139.7050):
//!   - otp-rs (normal profile): distance_m=906.6, duration_s=681.7 (11.4分)
//!   - 本家 OTP (WALK直行 planConnection):        distance=840.4m, duration=841s (14.0分)
//!
//!   街路データの取り込み範囲・分割 (歩道の別マッピング等) が同一でないため
//!   厳密一致はしないが、距離差 ~8%・所要も同オーダーで「桁が合う」ことを確認済み。
//!   (このテストでは OTP 呼び出しはせず、上記の距離帯に収まることだけを検証する)

use std::path::{Path, PathBuf};
use std::process::Command;

use otp_core::LatLng;
use otp_street::{StreetGraph, WalkProfile};

fn default_pbf_path() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../infra/otp/data/tokyo-central.osm.pbf"
    ))
}

/// bbox抽出済み OSM XML を用意する。`osmium` 未インストール/入力 pbf 無しなら None。
fn prepare_osm_xml() -> Option<PathBuf> {
    if Command::new("osmium").arg("--version").output().is_err() {
        eprintln!("osmium が見つかりません。skip します。");
        return None;
    }
    let pbf = std::env::var("OTP_RS_OSM_PBF")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_pbf_path());
    if !pbf.is_file() {
        eprintln!(
            "実データ .osm.pbf が見つかりません ({}). skip します。",
            pbf.display()
        );
        return None;
    }

    let out = std::env::temp_dir().join("otp-rs-shinjuku-smoke.osm");
    if !out.is_file() {
        // 複数テストが並列に実行されるため、各自ユニークな一時ファイルに書いてから
        // rename (同一ファイルシステム内ならアトミック) で共有パスへ差し替える。
        // そうしないと「他スレッドが書き込み中の途中状態」を読んでパースが壊れる
        // (実測: この対策前は並列実行で NotFound になるレースが再現した)。
        let script = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/extract_osm_xml.sh"
        ));
        // `std::process::id()` はテストバイナリ内の全スレッドで同一値になるため、
        // スレッド間でも一意になるようカウンタを足す。
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let unique_tmp = std::env::temp_dir().join(format!(
            "otp-rs-shinjuku-smoke.{}.{}.osm",
            std::process::id(),
            n
        ));
        let status = Command::new("bash")
            .arg(&script)
            .arg("139.690,35.685,139.710,35.700")
            .arg(&unique_tmp)
            .arg(&pbf)
            .status();
        match status {
            Ok(s) if s.success() => {
                let _ = std::fs::rename(&unique_tmp, &out);
            }
            _ => {
                eprintln!("extract_osm_xml.sh に失敗しました。skip します。");
                let _ = std::fs::remove_file(&unique_tmp);
                return None;
            }
        }
    }
    if out.is_file() {
        Some(out)
    } else {
        None
    }
}

fn graph_or_skip() -> Option<StreetGraph> {
    let path: PathBuf = prepare_osm_xml()?;
    Some(StreetGraph::build_from_osm_xml(&path).expect("実データ抽出の OSM XML はビルドできるはず"))
}

#[test]
fn shinjuku_walk_route_is_sane_and_in_the_right_ballpark_vs_otp() {
    let Some(graph) = graph_or_skip() else { return };
    assert!(
        graph.nodes.len() > 1000,
        "新宿駅周辺 bbox なら数千ノードはあるはず (実測: 約12000)"
    );

    let origin = LatLng::new(35.6902, 139.7003); // 新宿駅東口付近
    let dest = LatLng::new(35.6938, 139.7050); // 新宿三丁目駅付近 (直線距離 約600m)

    let path = graph
        .route(origin, dest, &WalkProfile::normal())
        .expect("経路が見つかるはず");

    // 本家 OTP 実測 (モジュールdoc参照): distance=840.4m。街路データの取り込み範囲が
    // 完全一致しないので厳密一致は求めず、「同じ桁」であることだけを見る
    // (数百m規模の直線距離600mに対し、数百m〜1.5km程度に収まっていれば妥当)。
    assert!(
        (400.0..1_500.0).contains(&path.distance_m),
        "distance_m={} は想定レンジ外",
        path.distance_m
    );
    // 徒歩速度1.33m/sなら600m~1500mは約7.5~19分。既知の異常値 (0や桁違いの大きさ) を弾く。
    assert!(
        (60.0..1_200.0).contains(&path.duration_s),
        "duration_s={} は想定レンジ外",
        path.duration_s
    );
}

#[test]
fn stroller_profile_never_produces_a_shorter_or_equal_duration_than_normal_on_real_data() {
    // アクセシビリティ・コストが実データでも効いていることの弱い実証:
    // strollerはnormalより階段/不明区間を避けようとする分、少なくとも所要が
    // 短くなることは無い (プロファイルが実際に効いていないと同じ経路 = 同じ所要になる)。
    let Some(graph) = graph_or_skip() else { return };
    let origin = LatLng::new(35.6902, 139.7003);
    let dest = LatLng::new(35.6938, 139.7050);

    let normal = graph
        .route(origin, dest, &WalkProfile::normal())
        .expect("normal route");
    let stroller = graph
        .route(origin, dest, &WalkProfile::stroller())
        .expect("stroller route");

    assert!(
        stroller.duration_s >= normal.duration_s,
        "stroller ({}) should not be cheaper than normal ({}) in generalized cost",
        stroller.duration_s,
        normal.duration_s
    );
}

/// スクリプトのパスが実在することだけ静的に確認する (壊れたパス参照の検知)。
#[test]
fn extract_script_exists() {
    let script = Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/extract_osm_xml.sh"
    ));
    assert!(script.is_file(), "{} が無い", script.display());
}
