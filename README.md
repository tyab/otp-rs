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
| `otp-gtfs` | GTFS 取り込み + 交通モデル + **運賃データ** | `gtfs`, `model`, `transit.model` | ドメイン型定義済み・std CSV ローダ実装済み（名前空間化ロード） |
| `otp-street` | OSM 街路グラフ + 歩行ルーティング + **アクセシビリティ・コスト** | `street`, `astar`, `WheelchairPreferences` | OSM XML 取り込み + A* 探索 実装済み |
| `otp-raptor` | RAPTOR マルチモーダル乗換探索 | `raptor`, `routing` | RAPTOR 探索実装済み・本家 OTP と一致確認済み。既定は鉄道のみ、`ModeFilter::RailAndBus` でバスも含める |
| `otp-fares` | 運賃計算（GTFS-Fares v1、将来 v2 / JR 距離制） | `ext.fares` | GTFS-Fares v1 (route/origin/destination/contains の一致規則) 実装済み・本家 OTP と数値突き合わせ済み。`transfers` を尊重した貪欲サブグループ化で距離制鉄道（通し運賃）と均一バス（1乗車1運賃）の両方に対応・JR 距離制は TODO |
| `otp-engine` | 統合（アクセス徒歩→乗換→イグレス徒歩→運賃→結果） | `routing.algorithm` 統合層 | 座標to座標の `Engine::plan` 実装済み（アクセス/イグレス徒歩+RAPTOR統合+フィード横断の運賃合算・バス運賃対応） |
| `otp-server` | ネイティブ経路サーバ（JVM OTP を置換） | `standalone`, GraphQL API | 起動時ロード + `GET /health` / `POST /plan` 実装済み・`--bus` でバス含む・起動時間を実測（下記） |

将来: `otp-wasm`（`otp-engine` の wasm32 境界）を追加してサーバーレス化を評価する。

## ビルド

```sh
cargo build          # 全クレート
cargo test           # テスト
cargo run -p otp-server -- --help
```

コアクレート（core/gtfs/street/raptor/fares/engine）は外部クレート依存ゼロ（std のみ）を
維持している。`otp-server` だけは HTTP/JSON のために `tiny_http` / `serde` / `serde_json`
を使う（crates.io からのフェッチが可能だったため。手書き実装へのフォールバックは不要だった。
詳細は次節）。

## otp-server（ネイティブ経路サーバ）

`Engine::plan` を HTTP で公開する。JVM OTP の `standalone` 相当だが、**起動が秒未満**な点が
最大の差分（下記実測）。

### 起動方法

```sh
cargo build --release -p otp-server

# GTFS は事業者ごとに展開済みディレクトリで渡す。複数フィードはカンマ区切りで
# "<prefix>=<dir>"（prefix は運賃計算のフィード名前空間、省略時は1始まりの連番）。
# OSM XML は scripts/extract_osm_xml.sh の出力。
./target/release/otp-server \
  --gtfs 1=/data/jr,2=/data/twr,3=/data/tokyometro,4=/data/keio,5=/data/tobu,6=/data/toei \
  --osm /data/shinjuku-hongo.osm \
  --port 8080

curl http://127.0.0.1:8080/health
curl -X POST http://127.0.0.1:8080/plan -d '{
  "origin": {"lat": 35.690, "lng": 139.700},
  "destination": {"lat": 35.707, "lng": 139.759},
  "departAt": "08:00",
  "serviceDate": 20260713,
  "mobility": "stroller"
}'
```

`departAt` は `"HH:MM"` 文字列でも 0時からの秒数（整数）でも受け付ける。`mobility` は
`solo`/`stroller`/`wheelchair`。応答は `{"itineraries": [{"totalDurationS", "transfers",
"fareYen", "legs": [{"mode": "WALK"|"TRANSIT", ...}]}]}`。

### 起動時間の実測（2026-07-15, M系Mac, release ビルド, ローカル）

実データ（都営地下鉄+東京メトロ+りんかい線+京王+東武+自前頻度JR、6フィード・802停留所、
新宿〜本郷三丁目 bbox の OSM 抽出・街路 74,426ノード/180,682エッジ）で計測:

| | 起動〜`/health` 200 (外側実測, プロセス起動込み) | 内部ロード計測 (`起動:` ログ) |
|---|---|---|
| otp-server (Rust, release) | **約0.4〜0.6秒**（3回実測: 0.38s/0.40s/0.39s、2フィード構成。6フィード構成では0.60s） | グラフ構築0.10s + 時刻表0.27〜0.45s = 合計0.35〜0.54s |
| JVM OTP (Cloudflare Containers, standard-3) | **約25〜85秒**（本番実測: グラフロード完走 約25秒 + R2からのグラフ取得 約30〜60秒。`git log` 063eca3参照） | — |

**約50〜200倍の起動短縮**（0.4秒 vs 25〜85秒）を実データで確認した。移植の狙い（コールド
スタート解消）を数値で実証できている。プロセスのメモリ常駐量は未計測（次段の課題）。

### /plan の実クエリ結果（新宿→本郷三丁目, stroller, 08:00発, 2026-07-13(月)）

6フィード構成で `crates/engine/examples/door_to_door.rs`（前スライスの座標to座標検証 CLI）
と同一 OD・同一パラメータで突き合わせ、**完全一致**を確認した:

```
[0] total=31m (1907s) transfers=1 fare_yen=398円
  WALK 377.5m -> TRANSIT 新宿線 6:301->6:307 (780s) -> WALK(乗換, 0m)
  -> TRANSIT 丸ノ内線 3:222->3:224 (240s) -> WALK 200.6m
[1] total=55m (3302s) transfers=0 fare_yen=220円
  WALK 466.0m -> TRANSIT 大江戸線 6:428->6:409 (2640s) -> WALK 218.9m
```

（2フィード構成 = 都営+メトロのみだと、access/egress の近傍駅候補が変わり
`大江戸線 6:402->6:409` という別の駅入口を使う経路になる。近傍駅探索が実際に読み込んだ
フィード集合に依存する既知の挙動で、バグではない。）

## 検証方針（rigor）

各スライスは **本家 OTP の出力と突き合わせて**許容誤差に収める（「完全互換」を最終目標に、
まずは実用差分に収める）。babymobi の `infra/otp/`（OTP2 コンテナ）が突き合わせの基準。

### 本家 OTP との意図的な差異：バス運賃

バス経路・運賃は 2026-07-15 にローカル OTP（`infra/otp/`, バス込みグラフ）と突き合わせた。
経路選択は一致する（例: 水天宮→秋葉原で両者とも都営バス「秋26」を採用）が、**本家 OTP は
この構成で都営バス便に運賃 product を一切付けない**（GraphQL `legs.fareProducts` が空。
運賃を持つ都営地下鉄では `6:220` 等が付くのと対照的。OTP の GTFS-Fares v1 処理が route_id
基準の均一運賃行を落としているとみられる）。

「OTP に厳密一致」を貫くとバス運賃が不明になり、`Engine::plan` が運賃不明の itinerary を
捨てる方針のためバス経路自体が消える。BabyMobi の狙い（ノンステップバスを運賃込みで提示）
に反するので、ここは **意図的に OTP から乖離**し、GTFS-Fares v1 仕様どおり
`fare_attributes.transfers=0`（乗継不可＝1乗車1運賃）を尊重して 210円/乗車を計算する
（実測: 秋26 単独=210円、秋26+日比谷線=388円=210+178。`otp-fares` の
`total_fare_charges_flat_bus_per_boarding_not_collapsed` 他で単体テスト固定）。

## データ

GTFS / OSM の実体はこのリポジトリに含めない（`.gitignore`）。babymobi の
`infra/otp/data/`（都営地下鉄・東京メトロ・りんかい線・都営バス・京王・東武の GTFS、
自前頻度 GTFS、東京都心 OSM 抽出）から供給する。運賃は各事業者 GTFS の
`fare_attributes.txt` / `fare_rules.txt`（GTFS-Fares v1）に含まれることを実測確認済み。

### OSM 取り込み方式（otp-street）

`.osm.pbf`（Protocol Buffers + zlib）を std のみで自前パースするのはコストに
見合わないため、**前処理は `osmium`（外部コマンド、`brew install osmium-tool`）
に任せ、otp-street は前処理済みの OSM XML（`.osm`）だけを読む**方針にした。
XML は `<tag k=".." v=".."/>` 中心の単純な構造なので、std の文字列走査だけで
手書きパーサ（`crates/street/src/osm_xml.rs`）が書ける。これで otp-street も
外部クレート依存ゼロ（std のみ）を維持している。

`scripts/extract_osm_xml.sh <bbox> [output.osm] [input.pbf]` が
`osmium extract`（bbox 抽出）→ `osmium tags-filter w/highway`（歩行関連 way +
参照ノードに絞る）→ `osmium cat`（XML 変換）の3段パイプラインを実行する
（既定入力は `../infra/otp/data/tokyo-central.osm.pbf`）。生成物は `/data`
配下に置けば `.gitignore` 済み。テスト用の小フィクスチャは
`crates/street/tests/fixtures/*.osm` に自作してコミットしてある。

## ライセンス

LGPL-3.0-or-later（OTP2 本体を踏襲）。
