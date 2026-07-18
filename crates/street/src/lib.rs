//! OSM 街路グラフと歩行ルーティング。**アクセシビリティ・コスト**がこのアプリの核。
//!
//! OTP の `street` / `astar` / `WheelchairPreferences` 相当。段差・エレベーター・勾配を
//! エッジ属性として持ち、プロファイル (通常/ベビーカー/車いす) ごとにコストを変える。

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::path::Path;

use otp_core::LatLng;

mod osm_xml;

/// 街路グラフの頂点添字 (CSR 配列のインデックス)。
pub type NodeId = u32;

/// 街路の頂点 (交差点・ノード)。
#[derive(Debug, Clone, Copy)]
pub struct StreetNode {
    pub coord: LatLng,
}

/// 街路のエッジ (歩行可能な区間)。アクセシビリティ属性を保持する。
#[derive(Debug, Clone)]
pub struct StreetEdge {
    pub from: NodeId,
    pub to: NodeId,
    pub length_m: f32,
    /// 階段を含む (OSM `highway=steps`)。
    pub has_stairs: bool,
    /// エレベーターを含む/経由 (OSM `highway=elevator` / `elevator=yes`)。
    pub has_elevator: bool,
    /// 最大勾配 (%)。不明は None。
    pub max_slope_pct: Option<f32>,
    /// 車いす通行可否 (OSM `wheelchair`)。不明は None。
    pub wheelchair: Option<bool>,
    /// 描画用ジオメトリはグラフには載せない (探索に不要)。確定経路の分だけ
    /// 外部 (babymobi 側 R2 等) から取得する想定。ここでは参照キーのみ持つ。
    pub geometry_ref: Option<u32>,
}

/// 移動プロファイル。OTP の wheelchairAccessibility チューニングに対応。
#[derive(Debug, Clone)]
pub struct WalkProfile {
    /// 徒歩速度 (m/秒)。
    pub speed_mps: f32,
    /// 階段の忌避度 (コスト倍率)。`forbid_stairs=false` のプロファイルにのみ効く
    /// (ハード除外時は倍率以前に走査から外れる)。
    pub stairs_reluctance: f32,
    /// 通行可否不明エッジへのペナルティ (OTP unknownCost 相当)。
    /// 不明を除外すると経路が出なくなるため「重み付きで通す」。
    pub unknown_cost: f32,
    /// 許容最大勾配 (%)。超過区間にペナルティ。
    pub max_slope_pct: f32,
    /// 階段エッジ (`has_stairs`) のハード除外。true なら探索の走査自体から外す
    /// (コスト無限大ではなく「存在しない」扱い)。
    ///
    /// 背景 (本番バグ): 忌避倍率 (stairs_reluctance=100) 方式では「代替が無い」場合に
    /// 階段経路がそのまま返っていた。プロダクト要件は「ベビーカー/車いすには通れる
    /// 道**だけ**を出す」なので、倍率ではなくハード制約にする。階段なしで到達できない
    /// 目的地は経路なし (None/Err) になり、呼び出し側 (otp-engine) は他の候補駅を使う
    /// か、正直に「経路なし」を返す (階段ありの道順をでっち上げない)。
    pub forbid_stairs: bool,
}

impl WalkProfile {
    /// 通常徒歩。
    pub fn normal() -> Self {
        Self { speed_mps: 1.33, stairs_reluctance: 1.0, unknown_cost: 1.0, max_slope_pct: 100.0, forbid_stairs: false }
    }
    /// ベビーカー: 階段はハード除外 (「担げば通れる」ではなく「通れる道だけ出す」が
    /// 本アプリの要件)。速度・不明区間・勾配の扱いは車いすより緩い。
    pub fn stroller() -> Self {
        Self { speed_mps: 1.2, stairs_reluctance: 10.0, unknown_cost: 1.5, max_slope_pct: 12.0, forbid_stairs: true }
    }
    /// 車いす: 階段はハード除外、勾配厳格。
    pub fn wheelchair() -> Self {
        Self { speed_mps: 1.0, stairs_reluctance: 100.0, unknown_cost: 2.0, max_slope_pct: 8.0, forbid_stairs: true }
    }
}

/// 街路グラフ (CSR 形式を想定した最小の器)。
#[derive(Debug, Default)]
pub struct StreetGraph {
    pub nodes: Vec<StreetNode>,
    pub edges: Vec<StreetEdge>,
    /// nodes[i] の出エッジは edges[adjacency_start[i]..adjacency_start[i+1]]。
    pub adjacency_start: Vec<u32>,
    /// 最近傍ノード探索用の一様グリッド索引 (`nearest_node`)。グラフ構築時に一度だけ
    /// 構築し、以後の全リクエストで使い回す。空グラフでは空 (`default`)。
    grid: SpatialGrid,
}

/// グリッドのセル辺長 (メートル)。1.45M ノードの都心グラフで、1セルあたりの
/// ノード数が小さく (数十以下) 収まり、かつ最近傍が数リングで確定する程度の粒度。
const GRID_CELL_M: f64 = 300.0;

/// 緯度1度あたりの距離 (メートル、WGS84 概算)。グリッドの投影に使う。
const M_PER_DEG_LAT: f64 = 111_320.0;

/// 多点スナップ: 端点座標の周囲からスナップ候補ノードを複数採る半径 (メートル)。
///
/// 単一最近傍スナップだと、端点が「階段でしか出入りできないノード」(駅ホーム重心の
/// 停留所座標が構内網に囲まれるケース) に固定され、階段回避経路が存在しても使えない
/// (本番バグ)。実データ診断 (`examples/diagnose_stairs.rs`, wide.osm) では:
/// - 新宿/秋葉原: 距離順 3〜5 番目の候補で階段なし経路が引けた。
/// - 上野: 階段なし網に繋がる最初の候補は約 34m 先・距離順 26 番目 (駅構内の
///   階段ロックされたノード群が手前を密に占める)。
/// 100m / 32 候補はこれらを余裕を持って覆い、かつグリッド走査 (3×3 セル) と
/// シード数の追加コストは無視できる。
const SNAP_RADIUS_M: f64 = 100.0;

/// 多点スナップの候補数上限。[`SNAP_RADIUS_M`] の根拠コメント参照 (上野で 26 番目
/// が必要だった実測に対する余裕)。
const SNAP_MAX_CANDIDATES: usize = 32;

/// スナップ候補選択時、端点→候補ノードの直線距離ぶんの歩行時間に掛ける重み。
///
/// 候補の選択は「直線オフセット時間 × この重み + グラフ上の経路コスト」の総和最小で
/// 行う。重み 1.0 (素の歩行時間) だと、エッジコストに `unknown_cost` (最大 2.0) 等の
/// 倍率が乗る一方で直線オフセットは倍率なしになり、「遠い候補へ直線でワープして
/// 街路網の序盤を踏み倒す」スナップが系統的に選ばれてしまう。通常エッジの最大倍率
/// (2.0) を明確に上回る 5.0 にすることで、最近傍候補が優先され、遠い候補は「近い
/// 候補では到達不能 (階段ロック等)」のときだけ選ばれる。
const SNAP_OFFSET_RELUCTANCE: f32 = 5.0;

/// 探索の一般化コスト上限 (秒相当)。徒歩 access/egress/乗換で現実にありえない
/// 探索球 (2時間相当超) を打ち切る保険。`forbid_stairs` の導入で「到達不能な
/// 候補ノード」が普通に発生する (駅構内の階段ロック島) ため、到達不能ターゲット
/// 待ちでグラフ全域 (1.4M ノード) を舐め切るのを防ぐ。
const MAX_SEARCH_COST_S: f32 = 7200.0;

/// リング停止条件に掛ける安全率。セルは等距円筒近似 (equirectangular) で投影する
/// ため、真のハバースイン距離との間に僅かな歪みが出る。1リング分の下限距離
/// `r * GRID_CELL_M` (投影メートル) に対し、真の距離がこの率まで小さくなりうると
/// 見て保守的に停止を遅らせる (2km スケールでの equirectangular 誤差 <0.5% に対し
/// 2% の余裕。1リング余分に広げても数十ノードの追加走査で済む)。
const GRID_SAFETY: f64 = 0.98;

/// 最近傍ノード探索用の一様グリッド索引。
///
/// ノード座標を等距円筒近似でメートル平面 (原点 = ノード群の南西端) に投影し、
/// `GRID_CELL_M` 角のセルにバケット化する。`nearest` はクエリ座標のセルから
/// リング状に外へ広げながら候補を集め、「探索済み領域より近いノードが在り得ない」
/// と保証できた時点で停止する (グリッド最近傍の標準手法)。
///
/// 返すノードは線形走査 (`min_by`) と完全一致させる: 距離が同点なら NodeId が
/// 小さい方 (= 元の反復順で先に現れる方) を採る。
#[derive(Debug, Default)]
struct SpatialGrid {
    origin_lat: f64,
    origin_lng: f64,
    /// 経度1度あたりの距離 (メートル)。参照緯度での cos 補正済み。
    m_per_deg_lng: f64,
    /// リング拡張の上限 (グリッドの広がり)。到達不能な無限ループを防ぐ保険。
    max_ring: i32,
    /// (セル列, セル行) → そのセルに属す NodeId 群。
    cells: HashMap<(i32, i32), Vec<NodeId>>,
}

impl SpatialGrid {
    /// ノード群からグリッドを構築する。
    fn build(nodes: &[StreetNode]) -> SpatialGrid {
        if nodes.is_empty() {
            return SpatialGrid::default();
        }
        let mut min_lat = f64::INFINITY;
        let mut min_lng = f64::INFINITY;
        let mut max_lat = f64::NEG_INFINITY;
        let mut max_lng = f64::NEG_INFINITY;
        for n in nodes {
            min_lat = min_lat.min(n.coord.lat);
            min_lng = min_lng.min(n.coord.lng);
            max_lat = max_lat.max(n.coord.lat);
            max_lng = max_lng.max(n.coord.lng);
        }
        let ref_lat = (min_lat + max_lat) / 2.0;
        let m_per_deg_lng = M_PER_DEG_LAT * ref_lat.to_radians().cos();

        let mut grid = SpatialGrid {
            origin_lat: min_lat,
            origin_lng: min_lng,
            m_per_deg_lng,
            max_ring: 0,
            cells: HashMap::new(),
        };
        let (mut min_cx, mut min_cy, mut max_cx, mut max_cy) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for (i, node) in nodes.iter().enumerate() {
            let cell = grid.cell_of(node.coord);
            min_cx = min_cx.min(cell.0);
            max_cx = max_cx.max(cell.0);
            min_cy = min_cy.min(cell.1);
            max_cy = max_cy.max(cell.1);
            grid.cells.entry(cell).or_default().push(i as NodeId);
        }
        // グリッド全体を覆い切るリング数 + 予備。best が見つからない事態 (空でない限り
        // 起きない) でも必ず終端させる。
        grid.max_ring = (max_cx - min_cx).max(max_cy - min_cy) + 2;
        grid
    }

    /// 座標を投影メートル平面のセル添字へ変換する。
    fn cell_of(&self, coord: LatLng) -> (i32, i32) {
        let x = (coord.lng - self.origin_lng) * self.m_per_deg_lng;
        let y = (coord.lat - self.origin_lat) * M_PER_DEG_LAT;
        ((x / GRID_CELL_M).floor() as i32, (y / GRID_CELL_M).floor() as i32)
    }

    /// クエリ座標に最も近いノードを返す (グリッド最近傍)。線形走査と同一の結果
    /// (同点は NodeId 昇順) を保証する。空グリッドでは None。
    fn nearest(&self, nodes: &[StreetNode], coord: LatLng) -> Option<NodeId> {
        if self.cells.is_empty() {
            return None;
        }
        let (cx, cy) = self.cell_of(coord);
        let mut best: Option<(f64, NodeId)> = None;
        let mut r = 0i32;
        loop {
            // チェビシェフ距離 = r のセル (リング) を走査する。
            let visit = |cell: (i32, i32), best: &mut Option<(f64, NodeId)>| {
                if let Some(ids) = self.cells.get(&cell) {
                    for &id in ids {
                        let d = nodes[id as usize].coord.haversine_m(&coord);
                        match *best {
                            None => *best = Some((d, id)),
                            // 距離が小さい方を採り、同点は NodeId が小さい方 (線形 min_by が
                            // 反復順で先に現れる = 添字が小さい方を残すのと一致させる)。
                            Some((bd, bid)) if d < bd || (d == bd && id < bid) => *best = Some((d, id)),
                            _ => {}
                        }
                    }
                }
            };
            if r == 0 {
                visit((cx, cy), &mut best);
            } else {
                for dx in -r..=r {
                    visit((cx + dx, cy - r), &mut best);
                    visit((cx + dx, cy + r), &mut best);
                }
                for dy in (-r + 1)..r {
                    visit((cx - r, cy + dy), &mut best);
                    visit((cx + r, cy + dy), &mut best);
                }
            }
            // リング r まで走査し終えた時点で、未走査セル (チェビシェフ距離 r+1 以上) に
            // 属すノードの投影距離は必ず r*GRID_CELL_M 以上。真の距離がこれを (安全率
            // 込みで) 下回れないなら、これ以上近いノードは存在しないので停止する。
            if let Some((bd, _)) = best {
                if bd <= (r as f64) * GRID_CELL_M * GRID_SAFETY {
                    break;
                }
            }
            r += 1;
            if r > self.max_ring {
                break;
            }
        }
        best.map(|(_, id)| id)
    }

    /// クエリ座標から `radius_m` 以内のノードを距離昇順 (同点は NodeId 昇順) で
    /// 最大 `max` 件返す (多点スナップ用)。
    ///
    /// `nearest` と同じリング走査だが、停止条件は「未走査セルの下限距離が半径を
    /// 超えた時点」。半径 (100m) はセル辺長 (300m) より小さいので実際は 2 リング
    /// (3×3 セル) で必ず停止する。
    fn within_radius(
        &self,
        nodes: &[StreetNode],
        coord: LatLng,
        radius_m: f64,
        max: usize,
    ) -> Vec<(NodeId, f64)> {
        let mut found: Vec<(NodeId, f64)> = Vec::new();
        if self.cells.is_empty() {
            return found;
        }
        let (cx, cy) = self.cell_of(coord);
        let mut r = 0i32;
        loop {
            let mut visit = |cell: (i32, i32)| {
                if let Some(ids) = self.cells.get(&cell) {
                    for &id in ids {
                        let d = nodes[id as usize].coord.haversine_m(&coord);
                        if d <= radius_m {
                            found.push((id, d));
                        }
                    }
                }
            };
            if r == 0 {
                visit((cx, cy));
            } else {
                for dx in -r..=r {
                    visit((cx + dx, cy - r));
                    visit((cx + dx, cy + r));
                }
                for dy in (-r + 1)..r {
                    visit((cx - r, cy + dy));
                    visit((cx + r, cy + dy));
                }
            }
            // リング r まで走査済み。未走査 (チェビシェフ距離 r+1 以上) のノードの
            // 真の距離は r*GRID_CELL_M*GRID_SAFETY 以上なので、それが半径を超えたら
            // これ以上候補は増えない。
            if (r as f64) * GRID_CELL_M * GRID_SAFETY > radius_m {
                break;
            }
            r += 1;
            if r > self.max_ring {
                break;
            }
        }
        found.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        found.truncate(max);
        found
    }
}

/// 歩行可能な `highway=*` 値。OTP の `StreetTraversalPermission` 判定の簡易版。
/// motorway/trunk 等の自動車専用道は含めない (歩道が別 way で分離マッピングされる前提)。
const WALKABLE_HIGHWAY: &[&str] = &[
    "footway",
    "path",
    "pedestrian",
    "steps",
    "living_street",
    "residential",
    "service",
    "track",
    "unclassified",
    "tertiary",
    "secondary",
    "primary",
    "elevator",
];

/// この way が歩行者にとって通行可能か。
///
/// - `highway` が [`WALKABLE_HIGHWAY`] のいずれかであること。
/// - `foot=no` (明示的な歩行者通行禁止) は highway の種別に関わらず除外する。
///   例: 実データの甲州街道/青梅街道 (`highway=trunk`/`primary` + `foot=no`)。
///
/// 対応範囲外 (将来の課題): `access=private/no` の一般規則、`sidewalk:*` タグに
/// よる歩道の分離指定、`foot=private` 等の細かい値。MVP では `foot=no` の
/// ハード除外のみで十分な精度が出ることを実データで確認済み。
fn is_walkable(way: &osm_xml::OsmWay) -> bool {
    let Some(highway) = way.tag("highway") else {
        return false;
    };
    if !WALKABLE_HIGHWAY.contains(&highway) {
        return false;
    }
    if way.tag("foot") == Some("no") {
        return false;
    }
    true
}

/// アクセシビリティ属性を way タグから読み取る。
fn accessibility_attrs(way: &osm_xml::OsmWay) -> (bool, bool, Option<bool>, Option<f32>) {
    let has_stairs = way.tag("highway") == Some("steps");
    let has_elevator = way.tag("highway") == Some("elevator") || way.tag("elevator") == Some("yes");
    let wheelchair = match way.tag("wheelchair") {
        Some("yes") => Some(true),
        Some("no") => Some(false),
        // "limited" 等の中間値は Option<bool> で表現できないため、素性が
        // 不明な場合と同様に扱う (profile.unknown_cost が緩めに効く)。
        _ => None,
    };
    // incline は "8%" / "-10%" のような百分率表記、または "up"/"down" のような
    // 定性的表記がある。数値表記のみ解釈し、符号は無視 (上りも下りも勾配としては
    // 同じペナルティ対象)。
    let max_slope_pct = way
        .tag("incline")
        .and_then(|s| s.strip_suffix('%'))
        .and_then(|s| s.parse::<f32>().ok())
        .map(|v| v.abs());
    (has_stairs, has_elevator, wheelchair, max_slope_pct)
}

impl StreetGraph {
    /// OSM XML (`.osm`) から歩行グラフを構築する。
    ///
    /// 入力は `.osm.pbf` ではなく前処理済みの OSM XML を想定する
    /// (`scripts/extract_osm_xml.sh` で `osmium` により bbox 抽出 + `highway`
    /// タグフィルタ + XML 変換したもの)。理由: `.osm.pbf` は Protocol Buffers +
    /// zlib で、std のみで自前パースするのはコストに見合わない。std の
    /// テキスト処理だけで読める OSM XML を選び、外部クレート依存ゼロを維持する。
    pub fn build_from_osm_xml(path: &Path) -> otp_core::Result<StreetGraph> {
        let content = std::fs::read_to_string(path)?;
        Ok(Self::build_from_osm_xml_str(&content))
    }

    /// [`build_from_osm_xml`] のファイル非依存版 (テスト用に公開)。
    pub fn build_from_osm_xml_str(xml: &str) -> StreetGraph {
        let doc = osm_xml::parse(xml);
        let coord_by_id: HashMap<i64, LatLng> = doc
            .nodes
            .iter()
            .map(|n| (n.id, LatLng::new(n.lat, n.lon)))
            .collect();

        // 使う way だけ抽出し、参照される node に NodeId (CSR インデックス) を
        // 初出順で割り当てる (決定的な結果にするため)。
        let mut node_index: HashMap<i64, NodeId> = HashMap::new();
        let mut nodes: Vec<StreetNode> = Vec::new();
        let mut raw_edges: Vec<StreetEdge> = Vec::new();

        for way in &doc.ways {
            if !is_walkable(way) || way.nodes.len() < 2 {
                continue;
            }
            let (has_stairs, has_elevator, wheelchair, max_slope_pct) = accessibility_attrs(way);

            let mut resolved: Vec<NodeId> = Vec::with_capacity(way.nodes.len());
            for &osm_id in &way.nodes {
                let Some(coord) = coord_by_id.get(&osm_id).copied() else {
                    // 参照ノードが見つからない (抽出範囲の境界等)。この way は諦める。
                    resolved.clear();
                    break;
                };
                let id = *node_index.entry(osm_id).or_insert_with(|| {
                    let id = nodes.len() as NodeId;
                    nodes.push(StreetNode { coord });
                    id
                });
                resolved.push(id);
            }
            if resolved.len() < 2 {
                continue;
            }

            for pair in resolved.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                let length_m = nodes[a as usize]
                    .coord
                    .haversine_m(&nodes[b as usize].coord) as f32;
                // 歩行は基本双方向 (oneway の車両規制は歩行者に及ばない前提)。
                raw_edges.push(StreetEdge {
                    from: a,
                    to: b,
                    length_m,
                    has_stairs,
                    has_elevator,
                    max_slope_pct,
                    wheelchair,
                    geometry_ref: None,
                });
                raw_edges.push(StreetEdge {
                    from: b,
                    to: a,
                    length_m,
                    has_stairs,
                    has_elevator,
                    max_slope_pct,
                    wheelchair,
                    geometry_ref: None,
                });
            }
        }

        // CSR 化: `from` でソートし、adjacency_start で範囲を引けるようにする。
        raw_edges.sort_by_key(|e| e.from);
        let mut adjacency_start = vec![0u32; nodes.len() + 1];
        for edge in &raw_edges {
            adjacency_start[edge.from as usize + 1] += 1;
        }
        for i in 1..adjacency_start.len() {
            adjacency_start[i] += adjacency_start[i - 1];
        }

        // 最近傍スナップ用のグリッド索引を一度だけ構築する (以後の全リクエストで再利用)。
        let grid = SpatialGrid::build(&nodes);

        StreetGraph {
            nodes,
            edges: raw_edges,
            adjacency_start,
            grid,
        }
    }

    /// nodes[i] の出エッジのスライス。
    fn out_edges(&self, node: NodeId) -> &[StreetEdge] {
        let start = self.adjacency_start[node as usize] as usize;
        let end = self.adjacency_start[node as usize + 1] as usize;
        &self.edges[start..end]
    }

    /// 座標に最も近い頂点を探す。
    ///
    /// グラフ構築時に張った一様グリッド索引 (`SpatialGrid`) でクエリ座標のセル周辺
    /// だけを走査する。1.45M ノードの広域グラフでも全ノードを舐めず、線形走査と
    /// 完全に同一のノード (同点は NodeId 昇順) を返す。
    fn nearest_node(&self, coord: LatLng) -> Option<NodeId> {
        self.grid.nearest(&self.nodes, coord)
    }

    /// 旧実装の線形最近傍 (テストで `nearest_node` との一致を検証するための参照)。
    #[cfg(test)]
    fn nearest_node_linear(&self, coord: LatLng) -> Option<NodeId> {
        self.nodes
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                a.coord
                    .haversine_m(&coord)
                    .partial_cmp(&b.coord.haversine_m(&coord))
                    .unwrap_or(Ordering::Equal)
            })
            .map(|(i, _)| i as NodeId)
    }

    /// プロファイルに応じたエッジの一般化コスト (秒相当)。探索の重み。
    pub fn edge_cost(&self, edge: &StreetEdge, profile: &WalkProfile) -> f32 {
        let base = edge.length_m / profile.speed_mps;
        let mut cost = base;
        if edge.has_stairs {
            cost *= profile.stairs_reluctance;
        }
        if edge.wheelchair == Some(false) {
            cost *= profile.stairs_reluctance.max(2.0);
        } else if edge.wheelchair.is_none() {
            cost *= profile.unknown_cost;
        }
        if let Some(slope) = edge.max_slope_pct {
            if slope > profile.max_slope_pct {
                cost *= 1.0 + (slope - profile.max_slope_pct) / 10.0;
            }
        }
        cost
    }

    /// 端点座標の多点スナップ候補を返す。
    ///
    /// 半径 [`SNAP_RADIUS_M`] 以内のノードを距離昇順に最大 [`SNAP_MAX_CANDIDATES`]
    /// 件。半径内に 1 つも無い場合は従来挙動 (無制限の最近傍 1 点) にフォールバック
    /// する (孤立地点からの探索を「候補なし」で即諦めないため。従来の
    /// `nearest_node` スナップと同じ到達性を保つ)。
    ///
    /// `offset_s` は端点→候補の直線距離ぶんの歩行時間 × [`SNAP_OFFSET_RELUCTANCE`]。
    /// 候補間の優先順位付け (近い候補ほど有利) にのみ使い、返す `WalkPath` の
    /// 距離・所要には含めない (経路はスナップ先ノードから始まる、という従来仕様を
    /// 変えない)。
    fn snap_candidates(&self, coord: LatLng, profile: &WalkProfile) -> Vec<SnapCandidate> {
        let mut cands = self
            .grid
            .within_radius(&self.nodes, coord, SNAP_RADIUS_M, SNAP_MAX_CANDIDATES);
        if cands.is_empty() {
            if let Some(id) = self.nearest_node(coord) {
                let d = self.nodes[id as usize].coord.haversine_m(&coord);
                cands.push((id, d));
            }
        }
        cands
            .into_iter()
            .map(|(node, d)| SnapCandidate {
                node,
                offset_s: (d as f32 / profile.speed_mps) * SNAP_OFFSET_RELUCTANCE,
            })
            .collect()
    }

    /// 2点間の歩行経路探索 (多点スナップ + A*)。
    ///
    /// `from`/`to` それぞれの多点スナップ候補 ([`Self::snap_candidates`]) の全組合せ
    /// から「直線オフセット + 経路コスト」総和が最小の組を 1 回の探索で選ぶ
    /// ([`Self::route_multi_snap`] のマルチソース探索)。最近傍 1 点が階段ロック
    /// された出入口でも、少し離れた別の入口から階段なしで入れるならそちらを使う。
    ///
    /// `profile.forbid_stairs` が真なら階段エッジは走査自体から除外され、階段なしで
    /// 到達できない場合は Err (階段ありの経路で誤魔化さない)。
    pub fn route(
        &self,
        from: LatLng,
        to: LatLng,
        profile: &WalkProfile,
    ) -> otp_core::Result<WalkPath> {
        self.route_multi_snap(from, std::slice::from_ref(&to), profile, false, true)
            .pop()
            .flatten()
            .ok_or_else(|| {
                otp_core::Error::NotFound(
                    "no walking path between origin and destination".to_string(),
                )
            })
    }

    /// 1点 → 複数点の歩行経路をまとめて引く (アクセス用)。
    ///
    /// `from` の多点スナップ候補群から **単一のマルチソース Dijkstra** を走らせ、
    /// 各 `targets[i]` に対し `route(from, targets[i], profile)` と同一の
    /// [`WalkPath`] を返す (到達不能・スナップ不能なら `None`)。route() の A*
    /// ヒューリスティックを外しても同じ最短経路が出るため、各要素は `route` の
    /// 結果とフィールド単位で一致する (最短経路が一意な限り。同コストの別経路が
    /// 複数ある同点ケースでは候補・親選択が探索順に依存しうるが、f32 の
    /// ハバースイン距離では実質起きない)。
    ///
    /// N 個の A* を 1 回の探索に畳むのが狙い。全ターゲットの最適候補が確定した
    /// 時点で打ち切るため、最遠ターゲットのコスト球しか展開しない。
    pub fn route_one_to_many(
        &self,
        from: LatLng,
        targets: &[LatLng],
        profile: &WalkProfile,
    ) -> Vec<Option<WalkPath>> {
        self.route_multi_snap(from, targets, profile, false, false)
    }

    /// 複数点 → 1点の歩行経路をまとめて引く (イグレス用)。
    ///
    /// このグラフは無向 (各 way を (a→b)/(b→a) 両方向で張り、エッジ属性も左右対称)
    /// かつ `edge_cost` も方向に依存しないため、`route(source, to)` は `to` から
    /// `source` への最短経路の逆順に等しい。よって `to` の多点スナップ候補群から
    /// **単一のマルチソース Dijkstra** を走らせ、各 `sources[i]` への経路のノード列を
    /// 反転して返せば、`route(sources[i], to, profile)` とフィールド単位で一致する
    /// (候補選択の目的関数「両端の直線オフセット + 経路コスト」も方向対称)。
    pub fn route_many_to_one(
        &self,
        sources: &[LatLng],
        to: LatLng,
        profile: &WalkProfile,
    ) -> Vec<Option<WalkPath>> {
        self.route_multi_snap(to, sources, profile, true, false)
    }

    /// [`route`]/[`route_one_to_many`]/[`route_many_to_one`] の共通実装
    /// (多点スナップ + マルチソース探索)。
    ///
    /// `from` の全スナップ候補を初期コスト = 直線オフセットでシードした 1 回の
    /// 探索で、各ターゲットの全スナップ候補のうち総コスト
    /// (from側オフセット + 経路コスト + ターゲット側オフセット) 最小のものを選ぶ。
    /// 各ターゲットへの経路はその最良候補から `came_from` を辿って復元する
    /// (チェーンの終端が「選ばれた from 側候補」になる)。
    ///
    /// - `reverse`: イグレス用。探索は目的地起点だが、返すノード列は source→to 向き。
    /// - `goal_directed`: ターゲットが 1 点のときだけ使える A* ヒューリスティック
    ///   (`route()` 用)。h(n) = max(0, (直線距離(n, ターゲット座標) − 候補最大
    ///   オフセット距離)) / speed。全エッジで cost ≥ length/speed かつターゲット
    ///   候補上で h=0 なので admissible & consistent であり、Dijkstra (h=0) と同じ
    ///   経路・同じ打ち切り判定が成立する (= one_to_many とのパリティ維持)。
    ///
    /// 打ち切り: 探索は f (settle コスト) 単調なので、あるターゲットの暫定最良総
    /// コストが現在の f 以下になったらそのターゲットは確定 (以後の settle で改善
    /// しない)。全ターゲット確定で終了。到達不能候補を待ち続けないよう、経路コスト
    /// [`MAX_SEARCH_COST_S`] 超は展開しない (`forbid_stairs` で普通に発生する
    /// 「階段ロック島」の候補にグラフ全域走査で付き合わないための上限)。
    fn route_multi_snap(
        &self,
        from: LatLng,
        targets: &[LatLng],
        profile: &WalkProfile,
        reverse: bool,
        goal_directed: bool,
    ) -> Vec<Option<WalkPath>> {
        let mut result: Vec<Option<WalkPath>> = vec![None; targets.len()];
        if targets.is_empty() {
            return result;
        }
        let origin_cands = self.snap_candidates(from, profile);
        if origin_cands.is_empty() {
            return result; // 空グラフ
        }
        let target_cands: Vec<Vec<SnapCandidate>> = targets
            .iter()
            .map(|&t| self.snap_candidates(t, profile))
            .collect();

        // A* ヒューリスティック (単一ターゲット時のみ)。ターゲット候補の最大直線
        // オフセット距離を引いておくことで、全候補上で h=0 になる (打ち切り判定と
        // 整合し、admissible 性も保つ)。
        let h_target: Option<(LatLng, f32)> = if goal_directed && targets.len() == 1 {
            let tc = targets[0];
            let max_off_m = target_cands[0]
                .iter()
                .map(|c| self.nodes[c.node as usize].coord.haversine_m(&tc))
                .fold(0.0f64, f64::max);
            Some((tc, max_off_m as f32))
        } else {
            None
        };
        let h = |node: NodeId| -> f32 {
            match h_target {
                Some((tc, max_off_m)) => {
                    let d = self.nodes[node as usize].coord.haversine_m(&tc) as f32;
                    ((d - max_off_m).max(0.0)) / profile.speed_mps
                }
                None => 0.0,
            }
        };

        let n = self.nodes.len();
        let mut g_score = vec![f32::INFINITY; n];
        let mut came_from: Vec<Option<(NodeId, usize)>> = vec![None; n];
        let mut closed = vec![false; n];
        let mut open = BinaryHeap::new();

        // マルチソースのシード。g にはスナップの直線オフセットを載せる (候補間の
        // 優先順位付け)。コスト上限はオフセット込みの絶対値で判定するため、最小
        // オフセットを基準に嵩上げする (遠隔地への単独フォールバック候補が上限を
        // 食い潰して探索できなくなるのを防ぐ)。
        let mut min_seed_offset = f32::INFINITY;
        for c in &origin_cands {
            if c.offset_s < g_score[c.node as usize] {
                g_score[c.node as usize] = c.offset_s;
                open.push(HeapItem { f: c.offset_s + h(c.node), node: c.node });
            }
            min_seed_offset = min_seed_offset.min(c.offset_s);
        }
        let cost_cap = MAX_SEARCH_COST_S + min_seed_offset;

        // ターゲット候補ノード → (ターゲット添字, ターゲット側オフセット) の逆引き。
        let mut cand_of_node: HashMap<NodeId, Vec<(usize, f32)>> = HashMap::new();
        for (ti, cands) in target_cands.iter().enumerate() {
            for c in cands {
                cand_of_node.entry(c.node).or_default().push((ti, c.offset_s));
            }
        }

        // ターゲットごとの暫定最良 (総コスト, 選ばれた候補ノード, 実経路か)。
        //
        // 縮退 (エッジ 0 本 = 経路長 0) の解は「両端点の**最近傍**候補が同一ノード」
        // の場合 (= 従来の start == goal ゼロ経路と同じ状況) に限り、探索前にここで
        // 直接登録する。多点スナップでは端点同士が 200m 以内だと候補集合が重なり
        // うるが、離れた 2 点を「共有候補で距離 0」の偽経路で結ぶと、実際には街路
        // 経路が無いのに 0m の徒歩として報告してしまう (階段ハード除外で経路が
        // 無い場合に特に危険)。そのケースは正直に「経路なし」に落とす。
        let mut best: Vec<Option<(f32, NodeId, bool)>> = vec![None; targets.len()];
        for (ti, cands) in target_cands.iter().enumerate() {
            if let (Some(o0), Some(t0)) = (origin_cands.first(), cands.first()) {
                if o0.node == t0.node {
                    best[ti] = Some((o0.offset_s + t0.offset_s, t0.node, false));
                }
            }
        }
        let mut resolved: Vec<bool> = target_cands.iter().map(|c| c.is_empty()).collect();
        let mut remaining = resolved.iter().filter(|&&r| !r).count();

        // 候補ノードへの「実経路 (エッジ 1 本以上) での最良到達」。g_score はシードの
        // 直線オフセット (テレポート相当) と実到達の min になるため、シードが先に
        // settle して閉じた候補ノードには実経路の到達が記録されない。そこで候補
        // ノードに限り、settle 済みノードから緩和する瞬間に (g 改善の有無・closed に
        // かかわらず) 実到達を別途記録する。値: (実到達コスト, 直前ノード, エッジ添字)。
        let mut real_arrival: HashMap<NodeId, (f32, NodeId, usize)> = HashMap::new();

        while let Some(HeapItem { f, node: current }) = open.pop() {
            if closed[current as usize] {
                continue;
            }
            closed[current as usize] = true;

            // 確定判定: settle の f は単調非減少で、ターゲット候補上では h=0、また
            // 実到達の更新は「settle 済みノード + エッジコスト ≥ f」でしか起きない
            // ため、best 総コスト ≤ 現在の f のターゲットはもう改善しない。
            if remaining > 0 {
                for (ti, r) in resolved.iter_mut().enumerate() {
                    if !*r {
                        if let Some((bt, _, _)) = best[ti] {
                            if bt <= f {
                                *r = true;
                                remaining -= 1;
                            }
                        }
                    }
                }
                if remaining == 0 {
                    break; // 全ターゲット確定。
                }
            }

            let start_idx = self.adjacency_start[current as usize] as usize;
            for (offset, edge) in self.out_edges(current).iter().enumerate() {
                // 階段ハード除外: コスト無限大ではなく走査から外す (プロダクト要件
                // 「通れる道だけ出す」。倍率方式だと代替が無いとき階段経路が漏れる)。
                if profile.forbid_stairs && edge.has_stairs {
                    continue;
                }
                let neighbor = edge.to;
                let tentative = g_score[current as usize] + self.edge_cost(edge, profile);
                if tentative > cost_cap {
                    continue;
                }
                // 候補ノードへの実到達を記録する (closed でも・g 非改善でも)。
                if let Some(entries) = cand_of_node.get(&neighbor) {
                    let improved = real_arrival
                        .get(&neighbor)
                        .map(|&(cur_best, _, _)| tentative < cur_best)
                        .unwrap_or(true);
                    if improved {
                        real_arrival.insert(neighbor, (tentative, current, start_idx + offset));
                        for &(ti, off) in entries {
                            let total = tentative + off;
                            if best[ti].map(|(bt, _, _)| total < bt).unwrap_or(true) {
                                best[ti] = Some((total, neighbor, true));
                            }
                        }
                    }
                }
                if closed[neighbor as usize] {
                    continue;
                }
                if tentative < g_score[neighbor as usize] {
                    g_score[neighbor as usize] = tentative;
                    came_from[neighbor as usize] = Some((current, start_idx + offset));
                    open.push(HeapItem { f: tentative + h(neighbor), node: neighbor });
                }
            }
        }

        for (i, b) in best.iter().enumerate() {
            let Some((_, cand, via_real)) = *b else { continue };
            let first_hop = if via_real {
                // 実経路: 記録済みの最終ホップから入り、その先は came_from を辿る。
                let &(_, u, ei) = real_arrival.get(&cand).expect("via_real なら記録があるはず");
                Some((u, ei))
            } else {
                None // 縮退 (経路長 0)。ノード 1 点の WalkPath になる。
            };
            result[i] = Some(self.assemble_path(cand, first_hop, &came_from, profile, reverse));
        }
        result
    }

    /// `WalkPath` のノード列を座標列に変換する (地図表示用の折れ線ジオメトリ)。
    /// 始点→終点の順。ノードが1点以下なら空/単点になる。
    pub fn path_coords(&self, path: &WalkPath) -> Vec<LatLng> {
        path.nodes.iter().map(|&n| self.nodes[n as usize].coord).collect()
    }

    /// 選ばれた候補ノードから経路を復元して [`WalkPath`] を組む
    /// ([`Self::route_multi_snap`] 専用)。
    ///
    /// チェーン収集は探索ゴール側 (`cand`) → 探索始点側。`first_hop` は候補ノードへの
    /// 実到達の最終ホップ (`real_arrival` 由来)。None なら縮退 (経路長 0、ノード 1 点)。
    ///
    /// `duration_s` は g_score から取らない: マルチソースシードの g にはスナップの
    /// 直線オフセットが混ざっているため、エッジ列を畳み込んで「経路そのものの
    /// コスト」を出す。浮動小数の加算は非結合的なので、畳み込み順は**経路の向き**
    /// (from→target) に対して旧実装と同一に揃え、アクセス方向 (`route`/
    /// `route_one_to_many`) とイグレス方向 (`route_many_to_one`) がフィールド単位で
    /// ビット一致するようにする:
    /// - `duration_s`: 経路の始点→終点の左畳み込み (旧 g_score 累積と同順)。
    /// - `distance_m`/フラグ: 経路の終点→始点の左畳み込み (旧 reconstruct_path と同順)。
    ///
    /// `reverse` (イグレス) では探索が目的地起点なのでチェーン順 = 経路の始点→終点、
    /// アクセスではチェーン順 = 経路の終点→始点になる。上記の畳み込み順は
    /// チェーン順ではなく経路の向き基準で選ぶ。
    fn assemble_path(
        &self,
        cand: NodeId,
        first_hop: Option<(NodeId, usize)>,
        came_from: &[Option<(NodeId, usize)>],
        profile: &WalkProfile,
        reverse: bool,
    ) -> WalkPath {
        let mut nodes = vec![cand];
        let mut edge_idxs: Vec<usize> = Vec::new(); // 探索ゴール側→探索始点側の順
        let mut cur = cand;
        if let Some((prev, ei)) = first_hop {
            edge_idxs.push(ei);
            nodes.push(prev);
            cur = prev;
        }
        while let Some((parent, edge_idx)) = came_from[cur as usize] {
            edge_idxs.push(edge_idx);
            nodes.push(parent);
            cur = parent;
        }

        // 経路の向き (始点→終点) に対する畳み込み順へ変換する。
        // - アクセス (reverse=false): チェーンは終点→始点 → duration は逆順、distance は正順。
        // - イグレス (reverse=true): チェーンは始点→終点 → duration は正順、distance は逆順。
        // チェーン順を「経路の始点→終点」順に正規化してから畳み込む。
        // アクセス (reverse=false) はチェーンが終点→始点なので反転、
        // イグレス (reverse=true) はチェーンがそのまま始点→終点。
        let path_order: Vec<usize> = if reverse {
            edge_idxs
        } else {
            nodes.reverse();
            edge_idxs.into_iter().rev().collect()
        };
        let mut duration_s = 0.0f32;
        for &ei in &path_order {
            duration_s += self.edge_cost(&self.edges[ei], profile);
        }
        let mut distance_m = 0.0f32;
        let mut has_stairs = false;
        let mut has_elevator = false;
        for &ei in path_order.iter().rev() {
            let edge = &self.edges[ei];
            distance_m += edge.length_m;
            has_stairs |= edge.has_stairs;
            has_elevator |= edge.has_elevator;
        }
        WalkPath {
            nodes,
            distance_m,
            duration_s,
            physical_duration_s: distance_m / profile.speed_mps,
            has_stairs,
            has_elevator,
        }
    }
}

/// A* の優先度キュー要素。f 値が小さいほど優先 (`BinaryHeap` は max-heap なので反転)。
struct HeapItem {
    f: f32,
    node: NodeId,
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.f == other.f
    }
}
impl Eq for HeapItem {}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        other.f.total_cmp(&self.f) // 反転 = 最小値が pop される
    }
}

/// 多点スナップの候補 1 件。`offset_s` は端点座標→候補ノードの直線距離ぶんの
/// 歩行時間 × [`SNAP_OFFSET_RELUCTANCE`] (候補間の優先順位付け専用。返す経路の
/// 距離・所要には含めない)。
#[derive(Debug, Clone, Copy)]
struct SnapCandidate {
    node: NodeId,
    offset_s: f32,
}

/// 歩行経路の結果。
#[derive(Debug, Clone)]
pub struct WalkPath {
    pub nodes: Vec<NodeId>,
    pub distance_m: f32,
    /// 探索用の一般化コスト (秒相当)。階段忌避・不明ペナルティ・勾配ペナルティの
    /// 乗数が織り込まれており、実際の壁時計時間ではない (`edge_cost` 参照)。
    /// A* の最適性判定・経路選択にはこちらを使う。
    pub duration_s: f32,
    /// 実際の壁時計所要時間 (秒) = `distance_m / profile.speed_mps`。
    /// UI表示や otp-engine の RAPTOR access/egress 秒数 (`StreetLink::duration_s`)
    /// にはこちらを使う (`duration_s` はペナルティ込みで実時間より長く出るため
    /// そのまま使うと乗り遅れ判定等がずれる)。
    pub physical_duration_s: f32,
    /// 経路に階段が含まれるか (UI の警告用)。
    pub has_stairs: bool,
    /// 経路がエレベーターを経由するか (UI のアクセシビリティ明示用)。
    pub has_elevator: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(stairs: bool, wheelchair: Option<bool>) -> StreetEdge {
        StreetEdge {
            from: 0,
            to: 1,
            length_m: 100.0,
            has_stairs: stairs,
            has_elevator: false,
            max_slope_pct: None,
            wheelchair,
            geometry_ref: None,
        }
    }

    #[test]
    fn stroller_avoids_stairs_more_than_normal() {
        let g = StreetGraph::default();
        let stairs = edge(true, None);
        let normal_cost = g.edge_cost(&stairs, &WalkProfile::normal());
        let stroller_cost = g.edge_cost(&stairs, &WalkProfile::stroller());
        assert!(stroller_cost > normal_cost * 5.0, "stroller should heavily avoid stairs");
    }

    #[test]
    fn wheelchair_penalizes_more_than_stroller_on_stairs() {
        let g = StreetGraph::default();
        let stairs = edge(true, None);
        let stroller = g.edge_cost(&stairs, &WalkProfile::stroller());
        let wheelchair = g.edge_cost(&stairs, &WalkProfile::wheelchair());
        assert!(wheelchair > stroller);
    }

    /// 最近傍探索・一対多探索のパリティ検証用フィクスチャ。
    ///
    /// n1..n8 が 1 本の連結木 (閉路なし = 任意2点間の最短経路が一意 → 浮動小数の
    /// 同点による経路分岐が起きない) を成し、n9-n10 が離れた非連結成分。座標は
    /// 都心 (~35.68N,139.76E) 帯に散らし、複数グリッドセルに跨るようにする。
    /// w2 (n3-n4-n5) は `highway=steps` にして段差フィールドの伝播も検証する。
    fn parity_fixture() -> StreetGraph {
        let xml = r#"<osm version="0.6">
            <node id="1" lat="35.6800" lon="139.7600"/>
            <node id="2" lat="35.6805" lon="139.7600"/>
            <node id="3" lat="35.6810" lon="139.7600"/>
            <node id="4" lat="35.6810" lon="139.7607"/>
            <node id="5" lat="35.6810" lon="139.7614"/>
            <node id="6" lat="35.6815" lon="139.7614"/>
            <node id="7" lat="35.6820" lon="139.7614"/>
            <node id="8" lat="35.6800" lon="139.7607"/>
            <node id="9" lat="35.7000" lon="139.8000"/>
            <node id="10" lat="35.7005" lon="139.8000"/>
            <way id="1"><nd ref="1"/><nd ref="2"/><nd ref="3"/><tag k="highway" v="footway"/></way>
            <way id="2"><nd ref="3"/><nd ref="4"/><nd ref="5"/><tag k="highway" v="steps"/></way>
            <way id="3"><nd ref="5"/><nd ref="6"/><nd ref="7"/><tag k="highway" v="footway"/></way>
            <way id="4"><nd ref="3"/><nd ref="8"/><tag k="highway" v="footway"/></way>
            <way id="5"><nd ref="9"/><nd ref="10"/><tag k="highway" v="footway"/></way>
        </osm>"#;
        StreetGraph::build_from_osm_xml_str(xml)
    }

    fn assert_walk_path_eq(a: &WalkPath, b: &WalkPath, ctx: &str) {
        assert_eq!(a.nodes, b.nodes, "{ctx}: nodes");
        assert_eq!(a.distance_m.to_bits(), b.distance_m.to_bits(), "{ctx}: distance_m");
        assert_eq!(a.duration_s.to_bits(), b.duration_s.to_bits(), "{ctx}: duration_s");
        assert_eq!(
            a.physical_duration_s.to_bits(),
            b.physical_duration_s.to_bits(),
            "{ctx}: physical_duration_s"
        );
        assert_eq!(a.has_stairs, b.has_stairs, "{ctx}: has_stairs");
        assert_eq!(a.has_elevator, b.has_elevator, "{ctx}: has_elevator");
    }

    /// グリッド索引の `nearest_node` が旧線形走査と完全一致する (同点は NodeId 昇順)。
    #[test]
    fn grid_nearest_node_matches_linear_scan() {
        let g = parity_fixture();
        assert!(!g.nodes.is_empty());
        // ノード近傍・中間・成分外・格子外れなど多様なクエリ座標。
        let samples = [
            LatLng::new(35.6800, 139.7600),   // n1 直上
            LatLng::new(35.680001, 139.76001), // n1 のわずかずれ
            LatLng::new(35.6810, 139.7614),   // n5 直上
            LatLng::new(35.6812, 139.7610),   // n4/n5/n6 の中間
            LatLng::new(35.6803, 139.7603),   // 内部の隙間
            LatLng::new(35.7002, 139.8000),   // 非連結成分 n9/n10 付近
            LatLng::new(35.6900, 139.7700),   // どのノードからも遠い中間
            LatLng::new(35.6820, 139.7614),   // n7 直上 (枝の端)
            LatLng::new(35.6799, 139.7599),   // 南西の外側
            LatLng::new(35.7100, 139.8100),   // 北東の外側
        ];
        for (i, &c) in samples.iter().enumerate() {
            assert_eq!(
                g.nearest_node(c),
                g.nearest_node_linear(c),
                "sample {i} ({c:?}): grid nearest must equal linear nearest"
            );
        }
    }

    /// アクセス: `route_one_to_many` が各ターゲットへの `route()` とフィールド単位で一致。
    /// 到達不能ターゲットは None (= route() の Err) になることも確認する。
    #[test]
    fn route_one_to_many_matches_per_target_route_access() {
        let g = parity_fixture();
        let origin = LatLng::new(35.68001, 139.76001); // n1 近傍
        let targets = [
            LatLng::new(35.6810, 139.7614),  // n5
            LatLng::new(35.6820, 139.7614),  // n7
            LatLng::new(35.6800, 139.7607),  // n8
            LatLng::new(35.68001, 139.76001), // n1 (start == goal → ゼロ経路)
            LatLng::new(35.7002, 139.8000),  // n9 (非連結 → 到達不能)
        ];
        for profile in [WalkProfile::normal(), WalkProfile::stroller(), WalkProfile::wheelchair()] {
            let batch = g.route_one_to_many(origin, &targets, &profile);
            assert_eq!(batch.len(), targets.len());
            for (i, t) in targets.iter().enumerate() {
                match g.route(origin, *t, &profile) {
                    Ok(single) => {
                        let got = batch[i].as_ref().unwrap_or_else(|| panic!("target {i}: batch None but route() Ok"));
                        assert_walk_path_eq(got, &single, &format!("access target {i}"));
                    }
                    Err(_) => assert!(batch[i].is_none(), "target {i}: route() Err → batch None"),
                }
            }
        }
    }

    /// 多点スナップの回帰フィクスチャ: 駅停留所座標の最近傍ノード n3 (idx2) が
    /// 「階段でしか出入りできない」構造。少し離れた n4 (idx3) は階段なしで街路網に
    /// つながる (本番の新宿/秋葉原/上野で確認したスナップ固定バグの最小再現)。
    ///
    /// ```text
    ///  n1(origin) --footway-- n2 --steps--   n3 (駅座標の最近傍・階段ロック)
    ///                          |--footway--  n4 (駅座標から~14m・階段なし入口)
    ///                          |--footway--  n6 --steps-- n5 (S2: 候補が n5 しかない駅)
    /// ```
    fn station_entrance_fixture() -> StreetGraph {
        let xml = r#"<osm version="0.6">
            <node id="1" lat="35.69900" lon="139.75000"/>
            <node id="2" lat="35.69950" lon="139.75000"/>
            <node id="3" lat="35.70000" lon="139.75000"/>
            <node id="4" lat="35.69999" lon="139.75015"/>
            <node id="5" lat="35.70500" lon="139.75000"/>
            <node id="6" lat="35.70400" lon="139.75000"/>
            <way id="1"><nd ref="1"/><nd ref="2"/><tag k="highway" v="footway"/></way>
            <way id="2"><nd ref="2"/><nd ref="3"/><tag k="highway" v="steps"/></way>
            <way id="3"><nd ref="2"/><nd ref="4"/><tag k="highway" v="footway"/></way>
            <way id="4"><nd ref="6"/><nd ref="5"/><tag k="highway" v="steps"/></way>
            <way id="5"><nd ref="2"/><nd ref="6"/><tag k="highway" v="footway"/></way>
        </osm>"#;
        StreetGraph::build_from_osm_xml_str(xml)
    }

    /// 駅座標 (n3 直上)。最近傍 n3 は階段ロック、~14m 先の n4 が階段なし入口。
    const STATION: LatLng = LatLng::new(35.70000, 139.75000);
    /// 候補が階段ロックの n5 しか無い駅座標 (n6 は 111m 先で候補半径 100m の外)。
    const LOCKED_STATION: LatLng = LatLng::new(35.70500, 139.75000);
    /// 出発地 (n1 のすぐ横)。
    const FIX_ORIGIN: LatLng = LatLng::new(35.69900, 139.75001);

    /// ハード除外プロファイル (ベビーカー/車いす) では、最近傍スナップが階段ロック
    /// でも別候補 (n4) から階段なしで駅に入れる。Solo は従来通り最近傍 (n3) へ
    /// 階段経由で直行する (階段回避は Solo には課さない)。
    #[test]
    fn multi_snap_picks_stair_free_entrance_when_nearest_snap_is_stairs_locked() {
        let g = station_entrance_fixture();
        for (name, profile) in [("stroller", WalkProfile::stroller()), ("wheelchair", WalkProfile::wheelchair())] {
            let p = g.route(FIX_ORIGIN, STATION, &profile)
                .unwrap_or_else(|e| panic!("{name}: 階段なし入口 (n4) 経由で経路が引けるはず: {e}"));
            assert!(!p.has_stairs, "{name}: forbid_stairs で階段は含まれないはず");
            assert_eq!(p.nodes.first(), Some(&0), "{name}: 経路は n1 から始まるはず");
            assert_eq!(p.nodes.last(), Some(&3), "{name}: 経路は階段なし入口 n4 で終わるはず");
        }
        let solo = g.route(FIX_ORIGIN, STATION, &WalkProfile::normal()).expect("solo route");
        assert!(solo.has_stairs, "solo は最短の階段経路を使えるはず");
        assert_eq!(solo.nodes.last(), Some(&2), "solo は最近傍スナップ n3 に着くはず");
    }

    /// 階段なしで到達できる候補が 1 つも無い駅は None/Err になり (経路をでっち上げ
    /// ない)、他の駅は生き残る。Solo は両方とも階段経由で到達できる。
    #[test]
    fn stairs_locked_stop_yields_none_while_alternatives_survive() {
        let g = station_entrance_fixture();
        let targets = [STATION, LOCKED_STATION];
        for (name, profile) in [("stroller", WalkProfile::stroller()), ("wheelchair", WalkProfile::wheelchair())] {
            let batch = g.route_one_to_many(FIX_ORIGIN, &targets, &profile);
            assert!(batch[0].as_ref().is_some_and(|p| !p.has_stairs), "{name}: 階段なし入口のある駅は生き残るはず");
            assert!(batch[1].is_none(), "{name}: 階段ロック駅は None になるはず");
            assert!(g.route(FIX_ORIGIN, LOCKED_STATION, &profile).is_err(), "{name}: route() も Err のはず");
            // イグレス方向もパリティが保たれる (route と同一の組合せ選択)。
            let egress = g.route_many_to_one(&targets, FIX_ORIGIN, &profile);
            let single = g.route(STATION, FIX_ORIGIN, &profile).expect("egress route");
            assert_walk_path_eq(egress[0].as_ref().unwrap(), &single, &format!("{name} egress"));
            assert!(egress[1].is_none(), "{name}: egress でも階段ロック駅は None");
        }
        let solo = g.route_one_to_many(FIX_ORIGIN, &targets, &WalkProfile::normal());
        assert!(solo[0].as_ref().is_some_and(|p| p.has_stairs), "solo は階段経由で最近傍に到達");
        assert!(solo[1].as_ref().is_some_and(|p| p.has_stairs), "solo は階段ロック駅にも到達できる");
    }

    /// forbid_stairs はプロファイル既定で stroller/wheelchair のみ true。
    #[test]
    fn forbid_stairs_defaults_per_profile() {
        assert!(!WalkProfile::normal().forbid_stairs);
        assert!(WalkProfile::stroller().forbid_stairs);
        assert!(WalkProfile::wheelchair().forbid_stairs);
    }

    /// 多点スナップでも「離れた 2 点を共有候補の距離 0 経路で結ぶ」でっち上げは
    /// しない。候補集合が重なる近距離ペアの挙動 3 態を固定する:
    /// 1. 実経路 (階段なし) があるならそれを返す (共有候補のゼロ経路より優先)。
    /// 2. 実経路が階段のみなら forbid_stairs では正直に Err (共有候補があっても
    ///    距離 0 の偽経路にしない)。
    /// 3. 両端の最近傍が同一ノードなら従来どおり距離 0 のゼロ経路。
    #[test]
    fn no_fake_zero_length_path_between_distinct_points() {
        let g = station_entrance_fixture();
        // 1. n2 直上 → n3 直上 (55m)。n3 は階段ロックだが、n3 から ~14m の n4 に
        //    footway (n2-n4) で届く → 実経路が返る。
        let near_n2 = LatLng::new(35.69950, 139.75001);
        let near_n3 = LatLng::new(35.70000, 139.75001);
        let p = g
            .route(near_n2, near_n3, &WalkProfile::wheelchair())
            .expect("階段なし入口 n4 への実経路が引けるはず");
        assert!(!p.has_stairs);
        assert!(p.nodes.len() >= 2, "共有候補の距離0経路ではなく実経路のはず: {:?}", p.nodes);
        assert_eq!(p.nodes.last(), Some(&3), "n4 (idx3) で終わるはず");

        // 2. n5 直上 → n5/n6 の中間 (n6 寄り)。唯一の実経路は steps (n6-n5) で、
        //    候補集合は n5 を共有するが、距離 0 の偽経路を返してはいけない。
        let at_n5 = LatLng::new(35.70500, 139.75000);
        let between = LatLng::new(35.70445, 139.75000); // n5 から 61m, n6 から 50m
        assert!(
            g.route(at_n5, between, &WalkProfile::wheelchair()).is_err(),
            "階段しかない区間は wheelchair では正直に Err のはず (距離0のでっち上げ禁止)"
        );
        let solo = g.route(at_n5, between, &WalkProfile::normal()).expect("solo は階段経由で到達");
        assert!(solo.has_stairs);

        // 3. 両端とも n1 が最近傍 → 従来どおりのゼロ経路。
        let p = g
            .route(LatLng::new(35.69900, 139.75001), LatLng::new(35.699005, 139.75002), &WalkProfile::wheelchair())
            .expect("同一最近傍へのスナップはゼロ経路");
        assert_eq!(p.nodes, vec![0]);
        assert_eq!(p.distance_m, 0.0);
    }

    /// グリッドの `within_radius` が線形走査と完全一致する (距離昇順・同点 NodeId 昇順)。
    #[test]
    fn grid_within_radius_matches_linear_scan() {
        let g = parity_fixture();
        let samples = [
            LatLng::new(35.6800, 139.7600),
            LatLng::new(35.6812, 139.7610),
            LatLng::new(35.7002, 139.8000),
            LatLng::new(35.6900, 139.7700),
            LatLng::new(35.6799, 139.7599),
        ];
        for (i, &c) in samples.iter().enumerate() {
            for radius in [30.0, 100.0, 250.0] {
                let got = g.grid.within_radius(&g.nodes, c, radius, 32);
                let mut want: Vec<(NodeId, f64)> = g
                    .nodes
                    .iter()
                    .enumerate()
                    .map(|(id, n)| (id as NodeId, n.coord.haversine_m(&c)))
                    .filter(|&(_, d)| d <= radius)
                    .collect();
                want.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap().then(a.0.cmp(&b.0)));
                want.truncate(32);
                assert_eq!(got, want, "sample {i} radius {radius}");
            }
        }
    }

    /// イグレス: `route_many_to_one` (目的地からの単一 Dijkstra) が各駅からの
    /// `route(source, destination)` とフィールド単位で一致 (ノード列も source→dest 向き)。
    #[test]
    fn route_many_to_one_matches_per_source_route_egress() {
        let g = parity_fixture();
        let destination = LatLng::new(35.68001, 139.76001); // n1 近傍
        let sources = [
            LatLng::new(35.6810, 139.7614),  // n5 (steps 経由 → has_stairs)
            LatLng::new(35.6820, 139.7614),  // n7 (3+エッジ経路: 距離畳み込み順の検証)
            LatLng::new(35.6800, 139.7607),  // n8
            LatLng::new(35.68001, 139.76001), // n1 (start == goal)
            LatLng::new(35.7002, 139.8000),  // n9 (非連結)
        ];
        for profile in [WalkProfile::normal(), WalkProfile::stroller(), WalkProfile::wheelchair()] {
            let batch = g.route_many_to_one(&sources, destination, &profile);
            assert_eq!(batch.len(), sources.len());
            for (i, s) in sources.iter().enumerate() {
                match g.route(*s, destination, &profile) {
                    Ok(single) => {
                        let got = batch[i].as_ref().unwrap_or_else(|| panic!("source {i}: batch None but route() Ok"));
                        assert_walk_path_eq(got, &single, &format!("egress source {i}"));
                    }
                    Err(_) => assert!(batch[i].is_none(), "source {i}: route() Err → batch None"),
                }
            }
        }
    }
}
