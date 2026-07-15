//! ネイティブ経路サーバのエントリ。JVM OTP の置換を狙う (高速起動・低メモリ)。
//!
//! OTP の `standalone` / GraphQL API 相当。babymobi の Workers API が叩く
//! HTTP エンドポイントを提供する。現況: `POST /plan` (座標to座標のドアtoドア経路探索,
//! `otp_engine::Engine::plan` をそのまま公開) と `GET /health`。
//!
//! 起動時に GTFS フィード群 (複数可, 名前空間化して読み込む) と OSM XML
//! (`scripts/extract_osm_xml.sh` の出力) から `otp_engine::Engine` を構築し、
//! 以降はメモリ上に保持したままリクエストに応答する (JVM OTP のようにグラフを
//! ディスクから毎回読み直すことはしない)。
//!
//! HTTP/JSON の実装方針: crates/server は依存方針の例外として実績クレートを使う
//! (README.md 参照)。crates.io からのフェッチが可能だったため、軽量 HTTP は
//! `tiny_http`、JSON は `serde`/`serde_json` を採用した (手書きパーサへの
//! フォールバックは不要だった)。コアクレート (core/gtfs/street/raptor/fares/engine)
//! は引き続き std のみ。
//!
//! TODO(移植): バイナリ形式でのグラフ直列化 (memmap 起動)・babymobi の Route
//! スキーマへの完全準拠・バスモード。README.md の「未達」節を参照。

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use otp_engine::Engine;
use otp_fares::FareModel;
use otp_gtfs::Feed;
use otp_raptor::{ModeFilter, Timetable};
use otp_street::StreetGraph;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_PORT: u16 = 8080;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("otp-server {VERSION}");
        return;
    }

    let config = match Config::parse(&args[1..]) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("引数エラー: {e}\n");
            print_help();
            std::process::exit(1);
        }
    };

    let engine = match load_engine(&config) {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("起動失敗 (ロード): {e}");
            std::process::exit(1);
        }
    };

    run_server(&config, &engine);
}

fn print_help() {
    println!(
        "otp-server {VERSION}\n\
         OpenTripPlanner の Rust 移植 — ネイティブ経路サーバ\n\n\
         USAGE:\n    otp-server --gtfs <spec>[,<spec>...] --osm <path> [--port <port>]\n\n\
         OPTIONS:\n\
         \x20   --gtfs <spec>[,<spec>...]\n\
         \x20                     GTFS フィードのディレクトリ (展開済み, stops.txt等を直接含む)。\n\
         \x20                     複数フィードはカンマ区切り。各要素は \"<prefix>=<dir>\" 形式で\n\
         \x20                     フィード名前空間 prefix を明示できる (運賃計算のフィード判定に\n\
         \x20                     使う。省略時は1始まりの連番)。例:\n\
         \x20                       --gtfs 3=/data/tokyometro,6=/data/toei\n\
         \x20   --osm <path>     OSM XML (scripts/extract_osm_xml.sh の出力)\n\
         \x20   --port <port>    待受ポート (既定 {DEFAULT_PORT})\n\
         \x20   --bus            バス路線も時刻表に含める (既定は鉄道のみ)。停留所・便数が\n\
         \x20                    大きく増えるため起動時間とメモリが増える点に注意\n\
         \x20   --version, -V    バージョン表示\n\
         \x20   --help, -h       このヘルプ\n\n\
         ENDPOINTS:\n\
         \x20   GET  /health     疎通確認 (200 {{\"status\":\"ok\"}})\n\
         \x20   POST /plan       経路探索。リクエスト/レスポンスの形は README.md 参照\n"
    );
}

/// CLI 引数から組み立てた起動設定。
struct Config {
    /// (フィード名前空間 prefix, GTFS 展開済みディレクトリ)。
    gtfs_specs: Vec<(String, PathBuf)>,
    osm_path: PathBuf,
    port: u16,
    /// 時刻表に含める交通モード (`--bus` でバスも含める。既定は鉄道のみ)。
    modes: ModeFilter,
}

impl Config {
    fn parse(args: &[String]) -> Result<Config, String> {
        let mut gtfs_arg: Option<String> = None;
        let mut osm_arg: Option<String> = None;
        let mut port = DEFAULT_PORT;
        let mut modes = ModeFilter::RailOnly;

        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--gtfs" => {
                    i += 1;
                    gtfs_arg = Some(args.get(i).ok_or("--gtfs には値が必要です")?.clone());
                }
                "--osm" => {
                    i += 1;
                    osm_arg = Some(args.get(i).ok_or("--osm には値が必要です")?.clone());
                }
                "--port" => {
                    i += 1;
                    let v = args.get(i).ok_or("--port には値が必要です")?;
                    port = v.parse::<u16>().map_err(|_| format!("--port の値が不正です: {v:?}"))?;
                }
                "--bus" => {
                    modes = ModeFilter::RailAndBus;
                }
                other => return Err(format!("不明な引数: {other:?}")),
            }
            i += 1;
        }

        let gtfs_arg = gtfs_arg.ok_or("--gtfs は必須です")?;
        let osm_arg = osm_arg.ok_or("--osm は必須です")?;
        Ok(Config { gtfs_specs: parse_gtfs_specs(&gtfs_arg), osm_path: PathBuf::from(osm_arg), port, modes })
    }
}

/// `"3=/data/tokyometro,6=/data/toei"` のようなカンマ区切りを (prefix, dir) の列に分解する。
/// `=` が無い要素は 1始まりの連番を prefix として補う (単一/少数フィードでの手軽な起動用)。
fn parse_gtfs_specs(arg: &str) -> Vec<(String, PathBuf)> {
    arg.split(',')
        .filter(|s| !s.is_empty())
        .enumerate()
        .map(|(idx, item)| match item.split_once('=') {
            Some((prefix, path)) => (prefix.to_string(), PathBuf::from(path)),
            None => ((idx + 1).to_string(), PathBuf::from(item)),
        })
        .collect()
}

/// GTFS フィード群 + OSM 街路グラフから `Engine` を構築する。ロード所要をログする
/// (「移植の狙い(高速起動)を数値で実証する」ため, README.md の起動時間実測の元データ)。
fn load_engine(config: &Config) -> otp_core::Result<Engine> {
    let overall_start = Instant::now();

    let t_graph = Instant::now();
    let street = StreetGraph::build_from_osm_xml(&config.osm_path)?;
    let graph_elapsed = t_graph.elapsed();

    let t_timetable = Instant::now();
    let mut feeds = Vec::with_capacity(config.gtfs_specs.len());
    let mut fares: HashMap<String, FareModel> = HashMap::new();
    for (prefix, dir) in &config.gtfs_specs {
        let feed = Feed::load_from_dir_namespaced(dir, prefix)?;
        fares.insert(prefix.clone(), FareModel::from_gtfs(&feed));
        feeds.push(feed);
    }
    let timetable = Timetable::build_with_modes(&feeds, config.modes)?;
    let timetable_elapsed = t_timetable.elapsed();

    let total_elapsed = overall_start.elapsed();
    eprintln!(
        "起動: グラフ構築 {:.3}s, 時刻表 {:.3}s, 合計 {:.3}s ({}街路ノード/{}エッジ, {}停留所, {}フィード)",
        graph_elapsed.as_secs_f64(),
        timetable_elapsed.as_secs_f64(),
        total_elapsed.as_secs_f64(),
        street.nodes.len(),
        street.edges.len(),
        timetable.stop_ids.len(),
        feeds.len(),
    );

    Ok(Engine::new(street, timetable, fares))
}

fn run_server(config: &Config, engine: &Engine) {
    let addr = format!("0.0.0.0:{}", config.port);
    let server = match tiny_http::Server::http(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("待受開始に失敗しました ({addr}): {e}");
            std::process::exit(1);
        }
    };
    eprintln!("otp-server {VERSION} 待受開始: http://{addr}  (GET /health, POST /plan)");

    for request in server.incoming_requests() {
        handle_request(request, engine);
    }
}

/// 1リクエストを配線する。JSON のパース/変換失敗は [`otp_server::handler`] 側が
/// 4xx/5xx に変換済みなのでそのまま応答するだけでよいが、ハンドラ自体が万一
/// パニックした場合に備えて `catch_unwind` で包み、プロセス全体を落とさない
/// (「パニックさせない」要件)。
fn handle_request(mut request: tiny_http::Request, engine: &Engine) {
    let method = request.method().clone();
    let url = request.url().to_string();

    let (status, body) = match (method, url.as_str()) {
        (tiny_http::Method::Get, "/health") => (200u16, otp_server::handler::health_json()),
        (tiny_http::Method::Post, "/plan") => {
            let mut buf = Vec::new();
            match request.as_reader().read_to_end(&mut buf) {
                Ok(_) => match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| otp_server::handler::handle_plan(engine, &buf))) {
                    Ok(Ok(json)) => (200u16, json),
                    Ok(Err(err)) => (err.status, err.to_json()),
                    Err(_) => (500u16, otp_server::handler::error_json("internal error (panic) while handling /plan")),
                },
                Err(e) => (400u16, otp_server::handler::error_json(&format!("failed to read request body: {e}"))),
            }
        }
        // OTP2 GraphQL 互換 (babymobi の apps/api/src/otp/client.ts が叩く planConnection)。
        // JVM OTP をこのサーバに差し替えても babymobi 側を無改修にするためのドロップイン。
        (tiny_http::Method::Post, "/otp/gtfs/v1") => {
            let mut buf = Vec::new();
            match request.as_reader().read_to_end(&mut buf) {
                Ok(_) => match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| otp_server::handler::handle_gtfs_graphql(engine, &buf))) {
                    Ok(Ok(json)) => (200u16, json),
                    Ok(Err(err)) => (err.status, err.to_json()),
                    Err(_) => (500u16, otp_server::handler::error_json("internal error (panic) while handling /otp/gtfs/v1")),
                },
                Err(e) => (400u16, otp_server::handler::error_json(&format!("failed to read request body: {e}"))),
            }
        }
        _ => (404u16, otp_server::handler::error_json("not found")),
    };

    respond_json(request, status, body);
}

fn respond_json(request: tiny_http::Request, status: u16, body: String) {
    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..]).expect("static header is valid");
    let response = tiny_http::Response::from_string(body).with_status_code(status).with_header(header);
    if let Err(e) = request.respond(response) {
        eprintln!("応答送信に失敗しました: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gtfs_specs_with_explicit_prefix() {
        let specs = parse_gtfs_specs("3=/data/tokyometro,6=/data/toei");
        assert_eq!(specs, vec![("3".to_string(), PathBuf::from("/data/tokyometro")), ("6".to_string(), PathBuf::from("/data/toei"))]);
    }

    #[test]
    fn parse_gtfs_specs_falls_back_to_sequential_prefix() {
        let specs = parse_gtfs_specs("/data/a,/data/b");
        assert_eq!(specs, vec![("1".to_string(), PathBuf::from("/data/a")), ("2".to_string(), PathBuf::from("/data/b"))]);
    }

    #[test]
    fn config_parse_requires_gtfs_and_osm() {
        let args: Vec<String> = vec!["--osm".to_string(), "x.osm".to_string()];
        assert!(Config::parse(&args).is_err());
    }

    #[test]
    fn config_parse_defaults_port() {
        let args: Vec<String> =
            vec!["--gtfs".to_string(), "1=/data/a".to_string(), "--osm".to_string(), "x.osm".to_string()];
        let config = Config::parse(&args).expect("should parse");
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.gtfs_specs, vec![("1".to_string(), PathBuf::from("/data/a"))]);
    }

    #[test]
    fn config_parse_rejects_bad_port() {
        let args: Vec<String> =
            vec!["--gtfs".to_string(), "1=/data/a".to_string(), "--osm".to_string(), "x.osm".to_string(), "--port".to_string(), "notaport".to_string()];
        assert!(Config::parse(&args).is_err());
    }
}
