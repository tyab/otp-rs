//! ネイティブ経路サーバのエントリ。JVM OTP の置換を狙う (高速起動・低メモリ)。
//!
//! OTP の `standalone` / GraphQL API 相当。最終形は babymobi の Workers API が
//! 叩く HTTP エンドポイント (`/routes/search` 互換) を提供する。
//!
//! 現況: バナー表示のみ。TODO(移植) の順:
//!   1. グラフ/時刻表/運賃モデルのロード (バイナリ形式・memmap で秒オーダー起動)
//!   2. HTTP サーバ (axum 等) で /plan を提供
//!   3. babymobi の Route スキーマへの変換

const VERSION: &str = env!("CARGO_PKG_VERSION");

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

    eprintln!("otp-server {VERSION} — OpenTripPlanner の Rust 移植 (開発中)");
    eprintln!("経路サーバ本体は未実装です。--help を参照してください。");
    // TODO(移植): Engine のロード → HTTP サーバ起動。
    std::process::exit(0);
}

fn print_help() {
    println!(
        "otp-server {VERSION}\n\
         OpenTripPlanner の Rust 移植 — ネイティブ経路サーバ\n\n\
         USAGE:\n    otp-server [OPTIONS]\n\n\
         OPTIONS:\n\
         \x20   --graph <PATH>   構築済みグラフ (未実装)\n\
         \x20   --port <PORT>    待受ポート (未実装)\n\
         \x20   --version, -V    バージョン表示\n\
         \x20   --help, -h       このヘルプ\n"
    );
}
