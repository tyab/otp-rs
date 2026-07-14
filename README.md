# otp-rs

OpenTripPlanner 2 (Java) の **Rust 移植**。
[Baby Mobility Commons](https://github.com/tyab/babymobi) の経路エンジンとして使う。

## なぜ移植するか

現状は OTP2 (JVM) を Cloudflare Containers で動かしているが、

- **コールドスタートが遅い**（JVM 起動 + グラフロードで数十秒〜）
- JVM のメモリ消費が大きい（東京グラフで実測 ~2.1GiB）
- Java は wasm32 を狙えないため、将来のサーバーレス(Worker/Wasm)化の道が閉じている

Rust 版なら **秒オーダー起動・数百MB級メモリ・単一ネイティブバイナリ**（ラズパイ/小VPS/小コンテナで動く）になり、
さらに **wasm32 ビルドの布石**になる（Worker 化は長期の選択肢として残す）。

## スコープ

- **長期目標: OTP2 の完全移植**（これが北極星）。
- **現在: 直近必要な分だけ**実装する。すなわち Baby Mobility Commons が使う
  「徒歩 + 鉄道/バスのマルチモーダル経路探索」「ベビーカー(車いす相当)アクセシビリティ・コスト」
  「運賃計算」「GTFS / OSM 取り込み」。
- NeTEx / SIRI / GraphQL 全面 / Flex transit / 運行情報リアルタイム等は**後段**（完全移植で埋める）。

## アーキテクチャ（OTP サブシステム対応のクレート分割）

| クレート | 役割 | OTP 対応 | 現況 |
|---|---|---|---|
| `otp-core` | 共通型（緯度経度・ID・時刻・エラー） | `org.opentripplanner.framework` 他 | 型定義済み |
| `otp-gtfs` | GTFS 取り込み + 交通モデル + **運賃データ** | `gtfs`, `model`, `transit.model` | ドメイン型定義済み・ローダは TODO |
| `otp-street` | OSM 街路グラフ + 歩行ルーティング + **アクセシビリティ・コスト** | `street`, `astar`, `WheelchairPreferences` | 型定義済み・探索は TODO |
| `otp-raptor` | RAPTOR マルチモーダル乗換探索 | `raptor`, `routing` | 型定義済み・探索は TODO |
| `otp-fares` | 運賃計算（GTFS-Fares v1、将来 v2 / JR 距離制） | `ext.fares` | 型定義済み・計算は TODO |
| `otp-engine` | 統合（アクセス徒歩→乗換→イグレス徒歩→運賃→結果） | `routing.algorithm` 統合層 | オーケストレーション骨格 |
| `otp-server` | ネイティブ経路サーバ（JVM OTP を置換） | `standalone`, GraphQL API | バナーのみ・API は TODO |

将来: `otp-wasm`（`otp-engine` の wasm32 境界）を追加してサーバーレス化を評価する。

## ビルド

```sh
cargo build          # 全クレート
cargo test           # テスト
cargo run -p otp-server -- --help
```

現時点では外部クレート依存ゼロ（std のみ）でコンパイルが通る土台。各スライスを実装する際に
`serde` / `csv` / `axum` 等を順次追加する。

## 検証方針（rigor）

各スライスは **本家 OTP の出力と突き合わせて**許容誤差に収める（「完全互換」を最終目標に、
まずは実用差分に収める）。babymobi の `infra/otp/`（OTP2 コンテナ）が突き合わせの基準。

## データ

GTFS / OSM の実体はこのリポジトリに含めない（`.gitignore`）。babymobi の
`infra/otp/data/`（都営地下鉄・東京メトロ・りんかい線・都営バス・京王・東武の GTFS、
自前頻度 GTFS、東京都心 OSM 抽出）から供給する。運賃は各事業者 GTFS の
`fare_attributes.txt` / `fare_rules.txt`（GTFS-Fares v1）に含まれることを実測確認済み。

## ライセンス

LGPL-3.0-or-later（OTP2 本体を踏襲）。
