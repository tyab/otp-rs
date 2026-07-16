//! OSM 街路グラフと歩行ルーティング。**アクセシビリティ・コスト**がこのアプリの核。
//!
//! OTP の `street` / `astar` / `WheelchairPreferences` 相当。段差・エレベーター・勾配を
//! エッジ属性として持ち、プロファイル (通常/ベビーカー/車いす) ごとにコストを変える。

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
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
    /// 階段の忌避度 (コスト倍率)。車いす=極大, ベビーカー=中, 通常=1。
    pub stairs_reluctance: f32,
    /// 通行可否不明エッジへのペナルティ (OTP unknownCost 相当)。
    /// 不明を除外すると経路が出なくなるため「重み付きで通す」。
    pub unknown_cost: f32,
    /// 許容最大勾配 (%)。超過区間にペナルティ。
    pub max_slope_pct: f32,
}

impl WalkProfile {
    /// 通常徒歩。
    pub fn normal() -> Self {
        Self { speed_mps: 1.33, stairs_reluctance: 1.0, unknown_cost: 1.0, max_slope_pct: 100.0 }
    }
    /// ベビーカー: 階段は強く避けるが車いすよりは許容 (担いで数段は可)。
    pub fn stroller() -> Self {
        Self { speed_mps: 1.2, stairs_reluctance: 10.0, unknown_cost: 1.5, max_slope_pct: 12.0 }
    }
    /// 車いす: 階段は事実上不可、勾配厳格。
    pub fn wheelchair() -> Self {
        Self { speed_mps: 1.0, stairs_reluctance: 100.0, unknown_cost: 2.0, max_slope_pct: 8.0 }
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

    /// 2点間の歩行経路探索 (A*)。
    ///
    /// `from`/`to` をそれぞれ最近傍ノードへスナップし、`edge_cost` (秒相当) を
    /// 重みにした A* で最小コスト経路を求める。ヒューリスティックは残り区間の
    /// 直線距離 / `profile.speed_mps` (= 段差・不明ペナルティ無しの理想コスト)。
    /// `edge_cost` の乗数は全て 1.0 以上 (階段忌避・不明ペナルティ・勾配ペナルティは
    /// コストを増やす方向にしか働かない) なので、このヒューリスティックは
    /// admissible かつ consistent。
    pub fn route(
        &self,
        from: LatLng,
        to: LatLng,
        profile: &WalkProfile,
    ) -> otp_core::Result<WalkPath> {
        let start = self.nearest_node(from).ok_or(otp_core::Error::NotFound(
            "no street node near origin".to_string(),
        ))?;
        let goal = self.nearest_node(to).ok_or(otp_core::Error::NotFound(
            "no street node near destination".to_string(),
        ))?;
        let goal_coord = self.nodes[goal as usize].coord;

        if start == goal {
            return Ok(WalkPath {
                nodes: vec![start],
                distance_m: 0.0,
                duration_s: 0.0,
                physical_duration_s: 0.0,
                has_stairs: false,
                has_elevator: false,
            });
        }

        let n = self.nodes.len();
        let mut g_score = vec![f32::INFINITY; n];
        let mut came_from: Vec<Option<(NodeId, usize)>> = vec![None; n]; // (親ノード, edges中のインデックス)
        let mut open = BinaryHeap::new();
        let mut closed = vec![false; n];

        g_score[start as usize] = 0.0;
        open.push(HeapItem {
            f: heuristic(self.nodes[start as usize].coord, goal_coord, profile),
            node: start,
        });

        while let Some(HeapItem { node: current, .. }) = open.pop() {
            if current == goal {
                let mut path = self.reconstruct_path(goal, &came_from, &g_score);
                path.physical_duration_s = path.distance_m / profile.speed_mps;
                return Ok(path);
            }
            if closed[current as usize] {
                continue;
            }
            closed[current as usize] = true;

            let start_idx = self.adjacency_start[current as usize] as usize;
            for (offset, edge) in self.out_edges(current).iter().enumerate() {
                let neighbor = edge.to;
                if closed[neighbor as usize] {
                    continue;
                }
                let tentative = g_score[current as usize] + self.edge_cost(edge, profile);
                if tentative < g_score[neighbor as usize] {
                    g_score[neighbor as usize] = tentative;
                    came_from[neighbor as usize] = Some((current, start_idx + offset));
                    let f = tentative
                        + heuristic(self.nodes[neighbor as usize].coord, goal_coord, profile);
                    open.push(HeapItem { f, node: neighbor });
                }
            }
        }

        Err(otp_core::Error::NotFound(
            "no walking path between origin and destination".to_string(),
        ))
    }

    /// 1点 → 複数点の歩行経路をまとめて引く (アクセス用)。
    ///
    /// `from` の最近傍ノードから **単一の Dijkstra** を走らせ、各 `targets[i]` に対し
    /// `route(from, targets[i], profile)` と同一の [`WalkPath`] を返す (到達不能・
    /// スナップ不能なら `None`)。A* のヒューリスティックを外した Dijkstra は同じ
    /// g_score・同じ最短経路を与えるため、各要素は `route` の結果とフィールド単位で
    /// 一致する (最短経路が一意な限り。同コストの別経路が複数ある同点ケースでは
    /// `came_from` の親選択が探索順に依存しうるが、f32 のハバースイン距離では実質
    /// 起きない)。
    ///
    /// N 個の A* を 1 回の探索に畳むのが狙い。全ターゲットが確定 (settle) した時点で
    /// 打ち切るため、最遠ターゲットまでのコスト球しか展開せず、グラフ全体は舐めない。
    pub fn route_one_to_many(
        &self,
        from: LatLng,
        targets: &[LatLng],
        profile: &WalkProfile,
    ) -> Vec<Option<WalkPath>> {
        self.route_multi_target(from, targets, profile, false)
    }

    /// 複数点 → 1点の歩行経路をまとめて引く (イグレス用)。
    ///
    /// このグラフは無向 (各 way を (a→b)/(b→a) 両方向で張り、エッジ属性も左右対称)
    /// かつ `edge_cost` も方向に依存しないため、`route(source, to)` は `to` から
    /// `source` への最短経路の逆順に等しい。よって `to` から **単一の Dijkstra** を
    /// 走らせ、各 `sources[i]` への経路のノード列を反転して返せば、
    /// `route(sources[i], to, profile)` とフィールド単位で一致する
    /// (距離・段差・所要時間は方向対称なのでそのまま、ノード列だけ source→to 向きに反転)。
    pub fn route_many_to_one(
        &self,
        sources: &[LatLng],
        to: LatLng,
        profile: &WalkProfile,
    ) -> Vec<Option<WalkPath>> {
        self.route_multi_target(to, sources, profile, true)
    }

    /// [`route_one_to_many`]/[`route_many_to_one`] の共通実装。
    ///
    /// `from` の最近傍ノードから Dijkstra を走らせ、各ターゲットの最近傍ノードへの
    /// 経路を復元する。`reverse` が真ならノード列を反転する (イグレス: 探索は
    /// 目的地起点だが、返す経路は駅→目的地向き)。
    fn route_multi_target(
        &self,
        from: LatLng,
        targets: &[LatLng],
        profile: &WalkProfile,
        reverse: bool,
    ) -> Vec<Option<WalkPath>> {
        let mut result: Vec<Option<WalkPath>> = vec![None; targets.len()];
        let Some(start) = self.nearest_node(from) else {
            return result;
        };
        // 各ターゲットを最近傍ノードへスナップ (route() と同じ前処理)。
        let target_nodes: Vec<Option<NodeId>> =
            targets.iter().map(|&t| self.nearest_node(t)).collect();

        let n = self.nodes.len();
        let mut g_score = vec![f32::INFINITY; n];
        let mut came_from: Vec<Option<(NodeId, usize)>> = vec![None; n];
        let mut closed = vec![false; n];

        // 確定させたいターゲットノードの集合。全て確定したら探索を打ち切る。
        let needed: HashSet<NodeId> = target_nodes.iter().flatten().copied().collect();
        let mut remaining = needed.len();

        g_score[start as usize] = 0.0;
        // Dijkstra なのでヒューリスティックは 0 (f = g)。`HeapItem` をそのまま流用する。
        let mut open = BinaryHeap::new();
        open.push(HeapItem { f: 0.0, node: start });

        while let Some(HeapItem { node: current, .. }) = open.pop() {
            if closed[current as usize] {
                continue;
            }
            closed[current as usize] = true;
            if needed.contains(&current) {
                remaining -= 1;
                if remaining == 0 {
                    break; // 全ターゲット確定。最遠ターゲットのコスト球で打ち切り。
                }
            }

            let start_idx = self.adjacency_start[current as usize] as usize;
            for (offset, edge) in self.out_edges(current).iter().enumerate() {
                let neighbor = edge.to;
                if closed[neighbor as usize] {
                    continue;
                }
                let tentative = g_score[current as usize] + self.edge_cost(edge, profile);
                if tentative < g_score[neighbor as usize] {
                    g_score[neighbor as usize] = tentative;
                    came_from[neighbor as usize] = Some((current, start_idx + offset));
                    open.push(HeapItem { f: tentative, node: neighbor });
                }
            }
        }

        for (i, target_node) in target_nodes.iter().enumerate() {
            let Some(goal) = *target_node else { continue };
            if goal == start {
                // route() の start == goal 分岐と同一のゼロ経路。
                result[i] = Some(WalkPath {
                    nodes: vec![start],
                    distance_m: 0.0,
                    duration_s: 0.0,
                    physical_duration_s: 0.0,
                    has_stairs: false,
                    has_elevator: false,
                });
                continue;
            }
            if !closed[goal as usize] {
                // 未到達 (非連結)。route() は Err を返し collect 側で捨てられる = None 相当。
                continue;
            }
            let path = if reverse {
                self.reconstruct_reverse(goal, &came_from, profile)
            } else {
                // アクセス: from→target。route(from, target) と同一方向の探索・復元なので
                // reconstruct_path をそのまま使う (g_score・距離の畳み込み順も一致)。
                let mut path = self.reconstruct_path(goal, &came_from, &g_score);
                path.physical_duration_s = path.distance_m / profile.speed_mps;
                path
            };
            result[i] = Some(path);
        }
        result
    }

    /// イグレス用の経路復元。探索は目的地 (`to`) 起点だが、返す `WalkPath` は
    /// `route(source, to, profile)` と**フィールド単位で完全一致**させる (source→to 向き)。
    ///
    /// 浮動小数の加算は非結合的なので、単にノード列を反転しただけでは `distance_m`/
    /// `duration_s` が畳み込み順の違いで 1ULP ずれうる。そこで `route()` と同じ順序で
    /// 畳み込む:
    /// - `duration_s` (= g_score) は探索方向 (source→to) の左畳み込み。
    /// - `distance_m` は `reconstruct_path` と同じ goal (=to) 側からの左畳み込み
    ///   (`came_from` を辿る自然な順の逆)。
    /// - `has_stairs`/`has_elevator` は論理和なので順序非依存。
    ///
    /// 無向グラフ・方向非依存の `edge_cost`/`length_m` により、逆向きエッジ複製の属性は
    /// 順方向と同一なので、これで `route(source, to)` を厳密に再現できる。
    fn reconstruct_reverse(
        &self,
        goal: NodeId, // = source (探索の goal)。返す経路の始点。
        came_from: &[Option<(NodeId, usize)>],
        profile: &WalkProfile,
    ) -> WalkPath {
        // came_from を source(goal) から to(start) へ辿り、ノード列 (source→to) と
        // エッジ列 (source→to 順) を集める。
        let mut nodes = vec![goal];
        let mut edge_idxs: Vec<usize> = Vec::new();
        let mut cur = goal;
        while let Some((parent, edge_idx)) = came_from[cur as usize] {
            edge_idxs.push(edge_idx);
            nodes.push(parent);
            cur = parent;
        }
        // duration_s: source→to の左畳み込み (route() の g_score 累積と同順)。
        let mut duration_s = 0.0f32;
        for &ei in &edge_idxs {
            duration_s += self.edge_cost(&self.edges[ei], profile);
        }
        // distance_m: to 側からの左畳み込み (route() の reconstruct_path と同順 = 逆順)。
        let mut distance_m = 0.0f32;
        let mut has_stairs = false;
        let mut has_elevator = false;
        for &ei in edge_idxs.iter().rev() {
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

    /// A* の `came_from` チェーンを辿って [`WalkPath`] を組み立てる。
    /// `WalkPath` のノード列を座標列に変換する (地図表示用の折れ線ジオメトリ)。
    /// 始点→終点の順。ノードが1点以下なら空/単点になる。
    pub fn path_coords(&self, path: &WalkPath) -> Vec<LatLng> {
        path.nodes.iter().map(|&n| self.nodes[n as usize].coord).collect()
    }

    fn reconstruct_path(
        &self,
        goal: NodeId,
        came_from: &[Option<(NodeId, usize)>],
        g_score: &[f32],
    ) -> WalkPath {
        let mut nodes = vec![goal];
        let mut distance_m = 0.0f32;
        let mut has_stairs = false;
        let mut has_elevator = false;
        let mut cur = goal;
        while let Some((parent, edge_idx)) = came_from[cur as usize] {
            let edge = &self.edges[edge_idx];
            distance_m += edge.length_m;
            has_stairs |= edge.has_stairs;
            has_elevator |= edge.has_elevator;
            nodes.push(parent);
            cur = parent;
        }
        nodes.reverse();
        WalkPath {
            nodes,
            distance_m,
            duration_s: g_score[goal as usize],
            physical_duration_s: 0.0, // route() が呼び出し後に埋める (ここでは profile を持たない)
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

/// 残り区間の直線距離ヒューリスティック (秒相当)。
fn heuristic(from: LatLng, goal: LatLng, profile: &WalkProfile) -> f32 {
    (from.haversine_m(&goal) as f32) / profile.speed_mps
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
