//! 本番バグ「車いすプロファイルでも access/egress 徒歩に階段が残る」の診断用 CLI。
//!
//! 仮説の切り分け:
//! - (A) スナップ先ノード間に階段回避経路がグラフ上に存在しない (データ欠落/エレベーター非接続)
//! - (B) 単一最近傍スナップが「階段でしか出入りできないノード」に固定してしまう
//! - (C) has_stairs の過剰報告 (経路自体は妥当なのにフラグだけ立つ)
//!
//! 診断結果 (wide.osm, 2026-07): 3駅とも (B)。新宿/秋葉原は距離順 3〜5 番目、
//! 上野は 26 番目の候補で階段なし経路が存在した。この結果を受けて otp-street に
//! 多点スナップ (SNAP_RADIUS_M/SNAP_MAX_CANDIDATES) + 階段ハード除外
//! (WalkProfile::forbid_stairs) を実装済み。本 CLI の route() 出力は修正後の
//! 本番挙動、単一スナップの Dijkstra セクションは修正前の再現 (回帰確認用)。
//!
//! 使い方:
//! ```sh
//! cargo run --release -p otp-street --example diagnose_stairs -- <wide.osm>
//! ```
//! プローブ (新宿/秋葉原/上野) はソース内に固定。各プローブで
//! 1. 修正前スナップ (最近傍1点) + 車いすコストでの最短経路と全エッジ属性を表示
//! 2. 階段エッジを完全除外した探索で「階段回避経路がそもそも在るか」を表示
//! 3. 両端それぞれ最近傍K点 (150m 以内) の全組合せで階段回避経路の有無を表示
//! し、最後に A/B/C の判定を出す。

use std::path::PathBuf;

use otp_core::LatLng;
use otp_street::{NodeId, StreetGraph, WalkProfile};

/// 診断プローブ: (名称, 出発/到着地点, 駅停留所座標)。
/// 座標は本番バグ報告のクエリと JR feed の stops.txt から採った実値。
fn probes() -> Vec<(&'static str, LatLng, LatLng)> {
    vec![
        (
            "新宿 (origin→station)",
            LatLng::new(35.690921, 139.700258), // バグ報告の出発地
            LatLng::new(35.689547, 139.700819), // JR 新宿 (stops.txt)
        ),
        (
            "秋葉原 (station→dest)",
            LatLng::new(35.698535, 139.773045), // JR 秋葉原 (stops.txt)
            LatLng::new(35.698383, 139.773072), // バグ報告の到着地
        ),
        (
            "上野 (station→park)",
            LatLng::new(35.713771, 139.776780), // JR 上野 (stops.txt)
            LatLng::new(35.712000, 139.774500), // 上野公園側の任意地点 (報告「上野も同様」の再現用)
        ),
    ]
}

/// 最近傍K点 (半径 radius_m 以内) を距離昇順で返す。診断用の線形走査。
fn k_nearest(graph: &StreetGraph, coord: LatLng, k: usize, radius_m: f64) -> Vec<(NodeId, f64)> {
    let mut cands: Vec<(NodeId, f64)> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (i as NodeId, n.coord.haversine_m(&coord)))
        .filter(|&(_, d)| d <= radius_m)
        .collect();
    cands.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap().then(a.0.cmp(&b.0)));
    cands.truncate(k);
    cands
}

/// 診断用 Dijkstra。`exclude_stairs` で階段エッジをハード除外できる。
/// 戻り値はエッジ索引列 (start→goal 順) と総コスト。
fn dijkstra(
    graph: &StreetGraph,
    profile: &WalkProfile,
    start: NodeId,
    goal: NodeId,
    exclude_stairs: bool,
) -> Option<(f32, Vec<usize>)> {
    use std::cmp::Ordering;
    use std::collections::BinaryHeap;

    struct Item {
        f: f32,
        node: NodeId,
    }
    impl PartialEq for Item {
        fn eq(&self, other: &Self) -> bool {
            self.f == other.f
        }
    }
    impl Eq for Item {}
    impl PartialOrd for Item {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }
    impl Ord for Item {
        fn cmp(&self, other: &Self) -> Ordering {
            other.f.total_cmp(&self.f)
        }
    }

    let n = graph.nodes.len();
    let mut g = vec![f32::INFINITY; n];
    let mut came: Vec<Option<(NodeId, usize)>> = vec![None; n];
    let mut closed = vec![false; n];
    let mut open = BinaryHeap::new();
    g[start as usize] = 0.0;
    open.push(Item { f: 0.0, node: start });
    while let Some(Item { node: cur, .. }) = open.pop() {
        if cur == goal {
            break;
        }
        if closed[cur as usize] {
            continue;
        }
        closed[cur as usize] = true;
        let s = graph.adjacency_start[cur as usize] as usize;
        let e = graph.adjacency_start[cur as usize + 1] as usize;
        for (idx, edge) in graph.edges[s..e].iter().enumerate() {
            if exclude_stairs && edge.has_stairs {
                continue;
            }
            if closed[edge.to as usize] {
                continue;
            }
            let t = g[cur as usize] + graph.edge_cost(edge, profile);
            if t < g[edge.to as usize] {
                g[edge.to as usize] = t;
                came[edge.to as usize] = Some((cur, s + idx));
                open.push(Item { f: t, node: edge.to });
            }
        }
    }
    if g[goal as usize].is_infinite() {
        return None;
    }
    let mut edges = Vec::new();
    let mut cur = goal;
    while let Some((parent, ei)) = came[cur as usize] {
        edges.push(ei);
        cur = parent;
    }
    edges.reverse();
    Some((g[goal as usize], edges))
}

/// エッジ列を属性つきで表示する。階段エッジは座標を出して現地特定できるようにする。
fn print_path_edges(graph: &StreetGraph, edge_idxs: &[usize], verbose: bool) {
    let mut dist = 0.0f32;
    let mut stair_edges = 0usize;
    let mut stair_len = 0.0f32;
    for &ei in edge_idxs {
        let e = &graph.edges[ei];
        dist += e.length_m;
        if e.has_stairs {
            stair_edges += 1;
            stair_len += e.length_m;
        }
        if verbose || e.has_stairs || e.has_elevator {
            let a = graph.nodes[e.from as usize].coord;
            let b = graph.nodes[e.to as usize].coord;
            println!(
                "    edge#{ei} {:.6},{:.6} -> {:.6},{:.6} len={:.1}m stairs={} elevator={} wheelchair={:?}",
                a.lat, a.lng, b.lat, b.lng, e.length_m, e.has_stairs, e.has_elevator, e.wheelchair
            );
        }
    }
    println!(
        "    total: {} edges, {:.1}m, stairs edges={} ({:.1}m)",
        edge_idxs.len(),
        dist,
        stair_edges,
        stair_len
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let osm_path = PathBuf::from(
        args.get(1)
            .map(String::as_str)
            .unwrap_or("/Users/tyabe/work/babymobi/infra/otp-rs-container/data/wide.osm"),
    );
    let t0 = std::time::Instant::now();
    let graph = StreetGraph::build_from_osm_xml(&osm_path).expect("グラフ構築に失敗");
    eprintln!(
        "graph built: {} nodes, {} edges ({:?})",
        graph.nodes.len(),
        graph.edges.len(),
        t0.elapsed()
    );
    let profile = WalkProfile::wheelchair();
    bench_one_to_many(&graph);

    for (name, from, to) in probes() {
        println!("\n================ {name} ================");
        println!("from={:.6},{:.6} to={:.6},{:.6}", from.lat, from.lng, to.lat, to.lng);

        // --- 1. 現行の単一最近傍スナップ ---
        let from_cands = k_nearest(&graph, from, 5, 150.0);
        let to_cands = k_nearest(&graph, to, 5, 150.0);
        let Some(&(start, sd)) = from_cands.first() else {
            println!("  no snap candidate near origin");
            continue;
        };
        let Some(&(goal, gd)) = to_cands.first() else {
            println!("  no snap candidate near destination");
            continue;
        };
        let sc = graph.nodes[start as usize].coord;
        let gc = graph.nodes[goal as usize].coord;
        println!(
            "  snap(current): start=n{start} {:.6},{:.6} ({sd:.1}m off) goal=n{goal} {:.6},{:.6} ({gd:.1}m off)",
            sc.lat, sc.lng, gc.lat, gc.lng
        );

        // route() の結果 (本番と同じ入口)。多点スナップ導入後は選ばれたスナップ先
        // (経路の両端ノード) も表示する。
        for (pname, p) in [
            ("wheelchair", WalkProfile::wheelchair()),
            ("stroller", WalkProfile::stroller()),
            ("normal", WalkProfile::normal()),
        ] {
            match graph.route(from, to, &p) {
                Ok(path) => {
                    let a = graph.nodes[*path.nodes.first().unwrap() as usize].coord;
                    let b = graph.nodes[*path.nodes.last().unwrap() as usize].coord;
                    println!(
                        "  route({pname}): distance={:.1}m cost={:.1} has_stairs={} has_elevator={} start={:.6},{:.6} end={:.6},{:.6}",
                        path.distance_m, path.duration_s, path.has_stairs, path.has_elevator,
                        a.lat, a.lng, b.lat, b.lng
                    );
                }
                Err(e) => println!("  route({pname}): ERROR {e}"),
            }
        }
        match dijkstra(&graph, &profile, start, goal, false) {
            Some((cost, edges)) => {
                println!("  [wheelchair cost, stairs allowed] cost={cost:.1}");
                print_path_edges(&graph, &edges, false);
            }
            None => println!("  [stairs allowed] UNREACHABLE"),
        }

        // --- 2. 階段エッジ完全除外で経路が在るか ---
        match dijkstra(&graph, &profile, start, goal, true) {
            Some((cost, edges)) => {
                println!("  [stairs EXCLUDED, same snap nodes] cost={cost:.1} -> stair-free path EXISTS");
                print_path_edges(&graph, &edges, false);
            }
            None => {
                println!("  [stairs EXCLUDED, same snap nodes] NO stair-free path between snapped nodes");
            }
        }

        // --- 3. 別スナップ候補の組合せで階段回避経路が在るか ---
        println!("  origin candidates:");
        for &(nid, d) in &from_cands {
            let c = graph.nodes[nid as usize].coord;
            println!("    n{nid} {:.6},{:.6} ({d:.1}m)", c.lat, c.lng);
        }
        println!("  dest candidates:");
        for &(nid, d) in &to_cands {
            let c = graph.nodes[nid as usize].coord;
            println!("    n{nid} {:.6},{:.6} ({d:.1}m)", c.lat, c.lng);
        }
        let mut best: Option<(f32, NodeId, NodeId)> = None;
        for &(s, sdist) in &from_cands {
            for &(t, tdist) in &to_cands {
                if let Some((cost, _)) = dijkstra(&graph, &profile, s, t, true) {
                    // 直線距離ぶんの歩行時間を足して「近いスナップ優先」の総コストで比較。
                    let total = cost + (sdist + tdist) as f32 / profile.speed_mps;
                    println!("    pair n{s}->n{t}: stair-free cost={cost:.1} (+offsets => {total:.1})");
                    if best.map(|(bc, _, _)| total < bc).unwrap_or(true) {
                        best = Some((total, s, t));
                    }
                }
            }
        }
        match best {
            Some((total, s, t)) => println!(
                "  VERDICT: stair-free path exists via snap pair n{s}->n{t} (total {total:.1}) => root cause (B) snap-locked (or route already stair-free => (C))"
            ),
            None => {
                println!(
                    "  VERDICT: NO stair-free path for ANY candidate pair (K=5, 150m) => likely (A). Locating the cut..."
                );
                // 追加分析: 半径を広げても繋がらないか + 階段なし到達圏が相手側に
                // どこまで迫れるか (= 切断箇所) を出す。
                let wide_from = k_nearest(&graph, from, 12, 300.0);
                let wide_to = k_nearest(&graph, to, 12, 300.0);
                let mut found = None;
                'outer: for &(s, _) in &wide_from {
                    for &(t, _) in &wide_to {
                        if dijkstra(&graph, &profile, s, t, true).is_some() {
                            found = Some((s, t));
                            break 'outer;
                        }
                    }
                }
                match found {
                    Some((s, t)) => println!("  wide search (K=12, 300m): stair-free pair n{s}->n{t} EXISTS => (B) with wider radius"),
                    None => println!("  wide search (K=12, 300m): still no stair-free pair => (A) confirmed at this scale"),
                }
                // どこまで K を増やせば「階段ロック島の外」の候補に届くかを測る
                // (多点スナップの K/半径の設計根拠)。
                for (label, coord) in [("origin", from), ("dest", to)] {
                    let ranked = k_nearest(&graph, coord, 40, 100.0);
                    print!("  {label} candidate ranks (dist, stair-free out-degree):");
                    for (rank, &(nid, d)) in ranked.iter().enumerate() {
                        let s = graph.adjacency_start[nid as usize] as usize;
                        let e = graph.adjacency_start[nid as usize + 1] as usize;
                        let free = graph.edges[s..e].iter().filter(|ed| !ed.has_stairs).count();
                        if rank < 40 {
                            print!(" [{rank}]n{nid}:{d:.0}m/{free}");
                        }
                    }
                    println!();
                }
                for (label, side, other) in [("origin", start, to), ("dest", goal, from)] {
                    let (n_reach, best_node, best_d) = stair_free_reach(&graph, &profile, side, other);
                    if let Some(b) = best_node {
                        let c = graph.nodes[b as usize].coord;
                        println!(
                            "  stair-free reachable set from {label} snap n{side}: {n_reach} nodes; closest approach to other end: n{b} {:.6},{:.6} ({best_d:.1}m away)",
                            c.lat, c.lng
                        );
                    }
                }
            }
        }
    }
}

/// access 相当の一対多探索 (wheelchair, 6駅) のレイテンシ計測。多点スナップ +
/// forbid_stairs 導入後の本番負荷の目安。ターゲットには「階段なしでは到達できない
/// 可能性のある」駅ホーム座標も混ぜ、コスト上限打ち切りの実測も兼ねる。
fn bench_one_to_many(graph: &StreetGraph) {
    let profile = WalkProfile::wheelchair();
    let origin = LatLng::new(35.690921, 139.700258); // 新宿の出発地
    let targets = [
        LatLng::new(35.689547, 139.700819), // JR新宿
        LatLng::new(35.690921, 139.699550), // 西口方面
        LatLng::new(35.693825, 139.699500), // 西武新宿方面
        LatLng::new(35.686355, 139.699500), // 南側
        LatLng::new(35.689500, 139.703000), // 東側
        LatLng::new(35.692000, 139.702000), // 北東
    ];
    let t = std::time::Instant::now();
    let paths = graph.route_one_to_many(origin, &targets, &profile);
    let dt = t.elapsed();
    let found = paths.iter().flatten().count();
    println!("\n[bench] route_one_to_many wheelchair x{}: {found} routed, {dt:?}", targets.len());
}

/// 階段除外 Dijkstra を打ち切りなしで走らせ、到達集合のサイズと「相手側座標に
/// 最も近づける到達ノード」を返す (切断箇所の特定用)。コスト上限 3600 秒相当で打ち切る。
fn stair_free_reach(
    graph: &StreetGraph,
    profile: &WalkProfile,
    start: NodeId,
    other: LatLng,
) -> (usize, Option<NodeId>, f64) {
    use std::cmp::Ordering;
    use std::collections::BinaryHeap;
    struct Item {
        f: f32,
        node: NodeId,
    }
    impl PartialEq for Item {
        fn eq(&self, other: &Self) -> bool {
            self.f == other.f
        }
    }
    impl Eq for Item {}
    impl PartialOrd for Item {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }
    impl Ord for Item {
        fn cmp(&self, other: &Self) -> Ordering {
            other.f.total_cmp(&self.f)
        }
    }
    let n = graph.nodes.len();
    let mut g = vec![f32::INFINITY; n];
    let mut closed = vec![false; n];
    let mut open = BinaryHeap::new();
    g[start as usize] = 0.0;
    open.push(Item { f: 0.0, node: start });
    let mut count = 0usize;
    let mut best: Option<(f64, NodeId)> = None;
    while let Some(Item { f, node: cur }) = open.pop() {
        if closed[cur as usize] {
            continue;
        }
        if f > 3600.0 {
            break;
        }
        closed[cur as usize] = true;
        count += 1;
        let d = graph.nodes[cur as usize].coord.haversine_m(&other);
        if best.map(|(bd, _)| d < bd).unwrap_or(true) {
            best = Some((d, cur));
        }
        let s = graph.adjacency_start[cur as usize] as usize;
        let e = graph.adjacency_start[cur as usize + 1] as usize;
        for edge in &graph.edges[s..e] {
            if edge.has_stairs || closed[edge.to as usize] {
                continue;
            }
            let t = g[cur as usize] + graph.edge_cost(edge, profile);
            if t < g[edge.to as usize] {
                g[edge.to as usize] = t;
                open.push(Item { f: t, node: edge.to });
            }
        }
    }
    match best {
        Some((d, b)) => (count, Some(b), d),
        None => (count, None, f64::INFINITY),
    }
}
