//! flowrun-gui — desktop native (egui). Aplikasi mandiri: buka flow dari dalam
//! app (picker + recent), edit koneksi/token di UI, canvas flowmaid (pan/zoom/
//! follow, panah, label wrap, pulse), log riwayat, JSON tree, re-run per node.
//!
//! Pembagian peran: flowmaid = layout graf · egui = render + kontrol ·
//! engine flowrun = eksekusi HTTP (worker thread, UI tetap responsif).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use clap::Parser;
use eframe::egui;
use serde::{Deserialize, Serialize};

use flowrun::config::{self, EnvConfig};
use flowrun::engine::{Ctx, Next, Outcome, choose_next, run_step};
use flowrun::flow::{self, FlowEdge, FlowStep};

// ============================= CLI =============================

#[derive(Parser)]
#[command(
    name = "flowrun-gui",
    about = "flowrun desktop — visual flow runner (egui)"
)]
struct Args {
    /// Opsional — tanpa argumen, app mulai di layar "Buka flow".
    #[arg(short = 'f', long)]
    flow: Option<PathBuf>,
    #[arg(short = 'c', long)]
    config: Option<PathBuf>,
    #[arg(short = 'e', long)]
    env: Option<PathBuf>,
    #[arg(long, default_value_t = 20)]
    timeout: u64,
    /// Curl di panel node memuat token ASLI (default disamarkan ${TOKEN_*}).
    #[arg(long)]
    reveal_tokens: bool,
}

// ======================= recent files ==========================

#[derive(Clone, Serialize, Deserialize, PartialEq)]
struct RecentEntry {
    flow: PathBuf,
    cfg: PathBuf,
    env: Option<PathBuf>,
}

fn recent_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/flowrun/recent.json"))
}

fn load_recent() -> Vec<RecentEntry> {
    recent_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn push_recent(entry: RecentEntry) {
    let mut list = load_recent();
    list.retain(|e| e != &entry);
    list.insert(0, entry);
    list.truncate(8);
    if let Some(p) = recent_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, serde_json::to_string_pretty(&list).unwrap_or_default());
    }
}

// ==================== worker <-> GUI protocol ===================

enum Cmd {
    Next,
    Skip,
    Auto,
    Reset,
    /// Re-run node tertentu (mis. re-timbang utk skenario B1). Tidak memajukan
    /// cursor kecuali node itu memang node saat ini dan lolos.
    RunAt(usize),
    /// Jawaban user atas cabang ambigu: target step berikutnya.
    Choose(usize),
    Quit,
}

#[derive(Clone)]
struct StepResult {
    idx: usize,
    state: NodeState,
    status: Option<u16>,
    ms: u128,
    msg: String,
    request_line: Option<String>,
    request_body: Option<serde_json::Value>,
    curl: Option<String>,
    auth_info: Option<String>,
    notes: Vec<String>,
    body: Option<serde_json::Value>,
    vars: Vec<(String, String)>,
}

enum Evt {
    Started(usize),
    Done(StepResult),
    /// Cabang ambigu — user harus memilih (target, label-kondisi).
    NeedChoice(Vec<(usize, String)>),
    /// Jalur mencapai ujung graf.
    FlowDone,
    AutoDone,
    Reset,
}

#[derive(Clone, Copy, PartialEq)]
enum NodeState {
    Idle,
    Current,
    Ok,
    Fail,
    Skip,
    Manual,
}

/// Hasil penentuan langkah berikutnya di worker.
enum Moved {
    Advanced,
    Ended,
    NeedPick,
}

struct Walker {
    cur: usize,
    start: usize,
    finished: bool,
    pending: bool,
    adjacency: Vec<Vec<FlowEdge>>,
}

impl Walker {
    /// Evaluasi outgoing edges dari `cur` atas vars → maju / selesai / tanya.
    fn resolve_next(&mut self, ctx: &Ctx, tx: &Sender<Evt>) -> Moved {
        let outs: Vec<&FlowEdge> = self.adjacency[self.cur].iter().collect();
        match choose_next(&outs, &ctx.vars) {
            Ok(Next::Advance(nx)) => {
                self.cur = nx;
                Moved::Advanced
            }
            Ok(Next::End) => {
                self.finished = true;
                let _ = tx.send(Evt::FlowDone);
                Moved::Ended
            }
            Ok(Next::Pick(opts)) => {
                self.pending = true;
                let _ = tx.send(Evt::NeedChoice(opts));
                Moved::NeedPick
            }
            Err(_) => {
                // Ekspresi kondisi rusak → perlakukan sbg ambigu (user memilih).
                self.pending = true;
                let opts = self.adjacency[self.cur]
                    .iter()
                    .map(|e| (e.to, e.label.clone().unwrap_or_default()))
                    .collect();
                let _ = tx.send(Evt::NeedChoice(opts));
                Moved::NeedPick
            }
        }
    }
}

fn worker(
    mut ctx: Ctx,
    steps: Vec<FlowStep>,
    adjacency: Vec<Vec<FlowEdge>>,
    start: usize,
    timeout: u64,
    rx: Receiver<Cmd>,
    tx: Sender<Evt>,
) {
    let initial = ctx.vars.clone();
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout))
        .build()
        .expect("build http client");
    let mut w = Walker {
        cur: start,
        start,
        finished: false,
        pending: false,
        adjacency,
    };

    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Quit => break,
            Cmd::Reset => {
                ctx.vars = initial.clone();
                w.cur = w.start;
                w.finished = false;
                w.pending = false;
                let _ = tx.send(Evt::Reset);
            }
            Cmd::Choose(target) => {
                if w.pending {
                    w.pending = false;
                    w.cur = target;
                }
            }
            Cmd::Skip => {
                if !w.finished && !w.pending {
                    let _ = tx.send(Evt::Done(skip_result(w.cur, &ctx)));
                    w.resolve_next(&ctx, &tx);
                }
            }
            Cmd::Next => {
                if !w.finished && !w.pending && exec(&steps, &mut ctx, &client, &tx, w.cur) {
                    w.resolve_next(&ctx, &tx);
                }
            }
            Cmd::RunAt(i) => {
                if i < steps.len() {
                    let ok = exec(&steps, &mut ctx, &client, &tx, i);
                    if ok && i == w.cur && !w.finished && !w.pending {
                        w.resolve_next(&ctx, &tx);
                    }
                }
            }
            Cmd::Auto => {
                while !w.finished && !w.pending {
                    if !exec(&steps, &mut ctx, &client, &tx, w.cur) {
                        break; // stop-on-fail
                    }
                    match w.resolve_next(&ctx, &tx) {
                        Moved::Advanced => std::thread::sleep(Duration::from_millis(250)),
                        _ => break,
                    }
                }
                let _ = tx.send(Evt::AutoDone);
            }
        }
    }
}

fn snapshot_vars(ctx: &Ctx) -> Vec<(String, String)> {
    ctx.vars
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn skip_result(idx: usize, ctx: &Ctx) -> StepResult {
    StepResult {
        idx,
        state: NodeState::Skip,
        status: None,
        ms: 0,
        msg: "skipped (manual)".into(),
        request_line: None,
        request_body: None,
        curl: None,
        auth_info: None,
        notes: vec![],
        body: None,
        vars: snapshot_vars(ctx),
    }
}

fn exec(
    steps: &[FlowStep],
    ctx: &mut Ctx,
    client: &reqwest::blocking::Client,
    tx: &Sender<Evt>,
    i: usize,
) -> bool {
    let _ = tx.send(Evt::Started(i));
    let step = &steps[i];

    if step.cfg.manual || step.cfg.request.is_none() {
        let _ = tx.send(Evt::Done(StepResult {
            idx: i,
            state: NodeState::Manual,
            status: None,
            ms: 0,
            msg: "manual/external step".into(),
            request_line: None,
            request_body: None,
            curl: None,
            auth_info: None,
            notes: vec![],
            body: None,
            vars: snapshot_vars(ctx),
        }));
        return true;
    }

    let rep = run_step(step, ctx, client);
    let (state, msg, cont) = match &rep.outcome {
        Outcome::Passed => (NodeState::Ok, String::new(), true),
        Outcome::Skipped(r) => (NodeState::Skip, r.clone(), true),
        Outcome::Manual => (NodeState::Manual, String::new(), true),
        Outcome::Failed(m) => (NodeState::Fail, m.clone(), false),
    };
    let _ = tx.send(Evt::Done(StepResult {
        idx: i,
        state,
        status: rep.http_status,
        ms: rep.ms,
        msg,
        request_line: rep.request_line,
        request_body: rep.request_body,
        curl: rep.curl,
        auth_info: rep.auth_info,
        notes: rep.notes,
        body: rep.body,
        vars: snapshot_vars(ctx),
    }));
    cont
}

// ========================= geometry ============================

struct SceneNodeG {
    step: usize,
    center: egui::Pos2,
    size: egui::Vec2,
    label: String,
}
struct Geometry {
    nodes: Vec<SceneNodeG>,
    edges: Vec<(Vec<egui::Pos2>, usize)>,
    w: f32,
    h: f32,
}

#[derive(Clone, Copy, PartialEq)]
enum LayoutDir {
    /// Serpentine: rantai linear dilipat beberapa baris zig-zag agar seluruh
    /// flow muat layar tanpa scroll (layered layout — flowmaid/mermaid — akan
    /// merender rantai linear sebagai satu pita panjang; ini penyempurnaan
    /// presentasi milik flowrun, flowmaid tetap dipakai utk parse + ukuran node).
    Snake,
    LR,
    TD,
}

fn build_geometry(
    mermaid_src: &str,
    node_to_step: &HashMap<String, usize>,
    dir: LayoutDir,
) -> anyhow::Result<Geometry> {
    let mut graph = match flowmaid::parser::parse_document(mermaid_src)
        .map_err(|e| anyhow::anyhow!("parse: {e:?}"))?
    {
        flowmaid::model::Document::Flowchart(g) | flowmaid::model::Document::State(g) => g,
        _ => anyhow::bail!("bukan flowchart"),
    };
    graph.direction = match dir {
        LayoutDir::LR | LayoutDir::Snake => flowmaid::model::Direction::LR,
        LayoutDir::TD => flowmaid::model::Direction::TD,
    };
    let sc = flowmaid::scene::scene(&graph);

    if dir == LayoutDir::Snake {
        return Ok(snake_layout(&sc, node_to_step));
    }

    // Peta by-id: flowmaid ≥0.18 menaruh `id` di SceneNode dan `from`/`to` di
    // SceneEdge, jadi tak lagi bergantung `sc.nodes[i]` sejajar `graph.nodes[i]`.
    let nodes = sc
        .nodes
        .iter()
        .map(|n| SceneNodeG {
            step: *node_to_step.get(&n.id).unwrap_or(&0),
            center: egui::pos2(n.x as f32, n.y as f32),
            size: egui::vec2(n.w as f32, n.h as f32),
            label: n.label.clone(),
        })
        .collect();
    let edges = sc
        .edges
        .iter()
        .map(|e| {
            let pts: Vec<egui::Pos2> = if e.waypoints.len() >= 2 {
                e.waypoints
                    .iter()
                    .map(|&(x, y)| egui::pos2(x as f32, y as f32))
                    .collect()
            } else {
                sample_bezier(&e.bezier, 18)
            };
            let src_step = node_to_step.get(&e.from).copied().unwrap_or(usize::MAX);
            (pts, src_step)
        })
        .collect();
    Ok(Geometry {
        nodes,
        edges,
        w: sc.width as f32,
        h: sc.height as f32,
    })
}

/// Layout ular untuk rantai linear: grid serpentine (baris genap →, baris
/// ganjil ←) + konektor siku antar-baris. Ukuran node diambil dari flowmaid
/// scene (intrinsic size), posisi dihitung di sini.
fn snake_layout(
    sc: &flowmaid::scene::Scene,
    node_to_step: &HashMap<String, usize>,
) -> Geometry {
    let n = sc.nodes.len();
    // Urutan eksekusi: step index → indeks scene-node (peta by-id, ≥0.18).
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| {
        node_to_step
            .get(&sc.nodes[i].id)
            .copied()
            .unwrap_or(usize::MAX)
    });

    // Kolom: mendekati aspek layar lebar; 14 node → 5 kolom (3 baris).
    let cols = ((n as f32 * 1.6).sqrt().ceil() as usize).max(2);
    let max_w = sc.nodes.iter().map(|s| s.w).fold(60.0, f64::max) as f32;
    let max_h = sc.nodes.iter().map(|s| s.h).fold(28.0, f64::max) as f32;
    let (gap_x, gap_y) = (56.0f32, 64.0f32);
    let (cell_w, cell_h) = (max_w + gap_x, max_h + gap_y);

    let mut nodes: Vec<SceneNodeG> = Vec::with_capacity(n);
    let mut centers: Vec<egui::Pos2> = Vec::with_capacity(n); // urut step
    for (k, &si) in order.iter().enumerate() {
        let row = k / cols;
        let col_in = k % cols;
        // Serpentine: baris ganjil dibalik arah kolomnya.
        let col = if row.is_multiple_of(2) {
            col_in
        } else {
            cols - 1 - col_in
        };
        let c = egui::pos2(
            col as f32 * cell_w + cell_w / 2.0,
            row as f32 * cell_h + cell_h / 2.0,
        );
        centers.push(c);
        let s = &sc.nodes[si];
        nodes.push(SceneNodeG {
            step: *node_to_step.get(&s.id).unwrap_or(&0),
            center: c,
            size: egui::vec2(s.w as f32, s.h as f32),
            label: s.label.clone(),
        });
    }

    // Konektor: sesama baris = garis horizontal antar tepi box; pindah baris
    // (posisi kolom sama) = garis vertikal turun.
    let mut edges: Vec<(Vec<egui::Pos2>, usize)> = Vec::new();
    for k in 0..n.saturating_sub(1) {
        let (a, b) = (centers[k], centers[k + 1]);
        let (sa, sb) = (nodes[k].size, nodes[k + 1].size);
        let pts = if (a.y - b.y).abs() < 1.0 {
            let dirx = (b.x - a.x).signum();
            vec![
                egui::pos2(a.x + dirx * sa.x / 2.0, a.y),
                egui::pos2(b.x - dirx * sb.x / 2.0, b.y),
            ]
        } else {
            vec![
                egui::pos2(a.x, a.y + sa.y / 2.0),
                egui::pos2(b.x, b.y - sb.y / 2.0),
            ]
        };
        edges.push((pts, k));
    }

    let rows = n.div_ceil(cols);
    Geometry {
        nodes,
        edges,
        w: cols as f32 * cell_w,
        h: rows as f32 * cell_h,
    }
}

fn sample_bezier(b: &[(f64, f64); 4], n: usize) -> Vec<egui::Pos2> {
    (0..=n)
        .map(|i| {
            let t = i as f32 / n as f32;
            let mt = 1.0 - t;
            let x = mt.powi(3) * b[0].0 as f32
                + 3.0 * mt.powi(2) * t * b[1].0 as f32
                + 3.0 * mt * t * t * b[2].0 as f32
                + t.powi(3) * b[3].0 as f32;
            let y = mt.powi(3) * b[0].1 as f32
                + 3.0 * mt.powi(2) * t * b[1].1 as f32
                + 3.0 * mt * t * t * b[2].1 as f32
                + t.powi(3) * b[3].1 as f32;
            egui::pos2(x, y)
        })
        .collect()
}

// ========================== session ============================

#[derive(Clone)]
struct StepMeta {
    node_id: String,
    title: String,
    role: Role,
    endpoint: String,
    note: Option<String>,
}
#[derive(Clone, Copy, PartialEq)]
enum Role {
    Customer,
    Owner,
    Neutral,
}

fn role_of(auth: Option<&str>) -> Role {
    match auth {
        Some("customer") => Role::Customer,
        Some("owner") => Role::Owner,
        _ => Role::Neutral,
    }
}

struct LogLine {
    t: f64,
    text: String,
    /// Detail multi-baris (payload/response/auth). None = baris ringkas tanpa
    /// lipatan. Default tertutup (`expanded=false`) agar log ringkas.
    detail: Option<String>,
    expanded: bool,
    color: egui::Color32,
}

struct Session {
    base_url: String,
    meta: Vec<StepMeta>,
    geo: Geometry,
    mermaid_src: String,
    node_to_step: HashMap<String, usize>,
    dir: LayoutDir,
    states: Vec<NodeState>,
    results: Vec<Option<StepResult>>,
    vars: Vec<(String, String)>,
    log: Vec<LogLine>,
    selected: Option<usize>,
    total: usize,
    auto_running: bool,
    /// Cabang ambigu menunggu pilihan user: (target, label kondisi).
    pending: Option<Vec<(usize, String)>>,
    /// Jalur sudah mencapai ujung graf.
    finished: bool,
    /// Graf rantai murni? (menentukan kelayakan layout Ular.)
    linear: bool,
    started: Instant,
    tx: Sender<Cmd>,
    rx: Receiver<Evt>,
    hitboxes: Vec<(usize, egui::Rect)>,
    zoom: f32,
    pan: egui::Vec2,
    fitted: bool,
    follow: bool,
    center_on: Option<usize>,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
    }
}

impl Session {
    fn start(
        flow_path: &Path,
        cfg_path: &Path,
        env: EnvConfig,
        timeout: u64,
        reveal_tokens: bool,
    ) -> anyhow::Result<Session> {
        let flow_cfg = config::load_flow_config(cfg_path)?;
        for p in &flow_cfg.auth_profiles {
            match env.tokens.get(p) {
                Some(t) if !t.trim().is_empty() => {}
                _ => anyhow::bail!(
                    "token for profile '{p}' is empty — set it in the connection panel"
                ),
            }
        }
        let base_url = env.base_url.trim_end_matches('/').to_string();
        let mut ctx = Ctx::build(&flow_cfg, env, &[]);
        ctx.reveal_tokens = reveal_tokens;
        let parsed = flow::load(flow_path, flow_cfg)?;
        let node_to_step: HashMap<String, usize> = parsed
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| (s.node_id.clone(), i))
            .collect();
        // Ular hanya utk rantai linear; graf bercabang lebih terbaca TD.
        let dir = if parsed.linear {
            LayoutDir::Snake
        } else {
            LayoutDir::TD
        };
        let geo = build_geometry(&parsed.mermaid_src, &node_to_step, dir)?;
        let meta: Vec<StepMeta> = parsed
            .steps
            .iter()
            .map(|s| StepMeta {
                node_id: s.node_id.clone(),
                title: s.title.clone(),
                role: role_of(s.cfg.auth.as_deref()),
                endpoint: s.cfg.request.clone().unwrap_or_else(|| "(manual)".into()),
                note: s.cfg.note.clone(),
            })
            .collect();
        let total = parsed.steps.len();

        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel::<Evt>();
        let steps = parsed.steps.clone();
        // Adjacency per node utk graph-walk di worker.
        let mut adjacency: Vec<Vec<FlowEdge>> = vec![Vec::new(); total];
        for e in &parsed.edges {
            adjacency[e.from].push(e.clone());
        }
        let start = parsed.start;
        let linear = parsed.linear;
        std::thread::spawn(move || worker(ctx, steps, adjacency, start, timeout, cmd_rx, evt_tx));

        Ok(Session {
            base_url,
            meta,
            geo,
            mermaid_src: parsed.mermaid_src,
            node_to_step,
            dir,
            states: vec![NodeState::Idle; total],
            results: vec![None; total],
            vars: Vec::new(),
            log: Vec::new(),
            selected: None,
            total,
            auto_running: false,
            pending: None,
            finished: false,
            linear,
            started: Instant::now(),
            tx: cmd_tx,
            rx: evt_rx,
            hitboxes: Vec::new(),
            zoom: 1.0,
            pan: egui::Vec2::ZERO,
            fitted: false,
            follow: true,
            center_on: None,
        })
    }

    fn logln(&mut self, text: String, color: egui::Color32) {
        let t = self.started.elapsed().as_secs_f64();
        self.log.push(LogLine {
            t,
            text,
            detail: None,
            expanded: false,
            color,
        });
    }

    /// Log dengan detail terlipat: `text` = ringkasan satu baris, `detail` =
    /// blok multi-baris yang tampil hanya saat entri dibuka.
    fn logln_detail(&mut self, text: String, detail: String, color: egui::Color32) {
        let t = self.started.elapsed().as_secs_f64();
        self.log.push(LogLine {
            t,
            text,
            detail: Some(detail),
            expanded: false,
            color,
        });
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Evt::Started(i) => {
                    self.states[i] = NodeState::Current;
                    self.selected = Some(i);
                    if self.follow {
                        self.center_on = Some(i);
                    }
                    let m = &self.meta[i];
                    self.logln(
                        format!("→ {} {}  {}", m.node_id, m.title, m.endpoint),
                        rgb(0x9ca3af),
                    );
                }
                Evt::Done(r) => {
                    let idx = r.idx;
                    self.states[idx] = r.state;
                    self.vars = r.vars.clone();
                    self.selected = Some(idx);
                    let (sym, col) = match r.state {
                        NodeState::Ok => ("✅", rgb(0x22c55e)),
                        NodeState::Fail => ("❌", rgb(0xef4444)),
                        NodeState::Skip => ("⏭", rgb(0x94a3b8)),
                        NodeState::Manual => ("✋", rgb(0xa78bfa)),
                        _ => ("·", rgb(0x9ca3af)),
                    };
                    let http = r
                        .status
                        .map(|c| format!("HTTP {c} · {} ms", r.ms))
                        .unwrap_or_default();
                    let mut line = format!("{sym} {} {}  {}", self.meta[idx].node_id, http, r.msg);
                    if let Some(rl) = &r.request_line {
                        line.push_str(&format!("\n      \u{2192} {rl}"));
                    }
                    if let Some(a) = &r.auth_info {
                        line.push_str(&format!("\n      \u{1f511} auth    : {a}"));
                    }
                    if let Some(b) = &r.request_body {
                        line.push_str(&pretty_block("\u{21e2} payload ", b));
                    }
                    if let Some(b) = &r.body {
                        line.push_str(&pretty_block("\u{21e0} response", b));
                    }
                    for n in &r.notes {
                        line.push_str(&format!("\n      {n}"));
                    }
                    // Baris pertama = ringkasan (selalu tampil); sisanya = detail
                    // yang terlipat (default tertutup, buka saat entri diklik).
                    match line.split_once('\n') {
                        Some((summary, detail)) => {
                            self.logln_detail(summary.to_string(), detail.to_string(), col)
                        }
                        None => self.logln(line, col),
                    }
                    self.results[idx] = Some(r);
                }
                Evt::NeedChoice(opts) => {
                    let daftar = opts
                        .iter()
                        .map(|(t, l)| format!("{} ({l})", self.meta[*t].title))
                        .collect::<Vec<_>>()
                        .join(" · ");
                    self.logln(format!("🔀 choose branch: {daftar}"), rgb(0xeab308));
                    self.pending = Some(opts);
                }
                Evt::FlowDone => {
                    self.finished = true;
                    self.logln(
                        "🎉 path complete — untraversed nodes dimmed".into(),
                        rgb(0x22c55e),
                    );
                }
                Evt::AutoDone => {
                    self.auto_running = false;
                    self.logln("auto stopped".into(), rgb(0x9ca3af));
                }
                Evt::Reset => {
                    self.states.iter_mut().for_each(|s| *s = NodeState::Idle);
                    self.results.iter_mut().for_each(|r| *r = None);
                    self.selected = None;
                    self.vars.clear();
                    self.auto_running = false;
                    self.pending = None;
                    self.finished = false;
                    self.logln("reset — ready from the start".into(), rgb(0x9ca3af));
                }
            }
        }
    }

    fn set_dir(&mut self, dir: LayoutDir) {
        if dir == self.dir || (dir == LayoutDir::Snake && !self.linear) {
            return; // Ular hanya utk rantai linear
        }
        self.dir = dir;
        if let Ok(g) = build_geometry(&self.mermaid_src, &self.node_to_step, self.dir) {
            self.geo = g;
            self.fitted = false;
        }
    }
}

// ====================== picker (layar buka) ====================

#[derive(Default)]
struct Picker {
    flow: Option<PathBuf>,
    cfg: Option<PathBuf>,
    env: Option<PathBuf>,
    base_url: String,
    tokens: Vec<(String, String)>,
    vars: Vec<(String, String)>,
    show_tokens: bool,
    error: Option<String>,
    recent: Vec<RecentEntry>,
    new_var_k: String,
    new_var_v: String,
    vars_open: bool,
}

impl Picker {
    fn new() -> Self {
        Picker {
            recent: load_recent(),
            vars_open: true,
            ..Default::default()
        }
    }

    /// Auto-saran cfg/env dari folder flow + muat draft koneksi.
    fn on_flow_picked(&mut self, p: PathBuf) {
        let dir = p.parent().map(Path::to_path_buf).unwrap_or_default();
        if self.cfg.is_none() {
            let c = dir.join("flow.yaml");
            if c.exists() {
                self.cfg = Some(c);
            }
        }
        if self.env.is_none() {
            for cand in ["dev.yaml", "env.yaml", "env.sample.yaml"] {
                let e = dir.join(cand);
                if e.exists() {
                    self.env = Some(e);
                    break;
                }
            }
        }
        self.flow = Some(p);
        self.reload_draft();
    }

    fn reload_draft(&mut self) {
        self.error = None;
        // Profil auth dari flow.yaml (urutan token mengikuti ini).
        let profiles: Vec<String> = self
            .cfg
            .as_ref()
            .and_then(|c| config::load_flow_config(c).ok())
            .map(|fc| fc.auth_profiles)
            .unwrap_or_default();
        let envc = self
            .env
            .as_ref()
            .and_then(|e| config::load_env_config(e).ok())
            .unwrap_or_default();
        self.base_url = envc.base_url.clone();
        let mut tokens: Vec<(String, String)> = Vec::new();
        for p in &profiles {
            tokens.push((p.clone(), envc.tokens.get(p).cloned().unwrap_or_default()));
        }
        for (k, v) in &envc.tokens {
            if !profiles.contains(k) {
                tokens.push((k.clone(), v.clone()));
            }
        }
        self.tokens = tokens;
        self.vars = envc
            .vars
            .iter()
            .map(|(k, v)| (k.clone(), config::yaml_to_var_string(v)))
            .collect();
    }

    fn to_env_config(&self) -> EnvConfig {
        let mut env = EnvConfig {
            base_url: self.base_url.trim().to_string(),
            ..Default::default()
        };
        for (k, v) in &self.tokens {
            env.tokens.insert(k.clone(), v.trim().to_string());
        }
        for (k, v) in &self.vars {
            env.vars
                .insert(k.clone(), serde_yaml::Value::String(v.clone()));
        }
        env
    }
}

// ============================ app ==============================

struct App {
    picker: Picker,
    session: Option<Session>,
    timeout: u64,
    reveal_tokens: bool,
}

/// Latar kanvas premium ala design-tool: gradien vertikal halus, dot-grid
/// yang ikut pan/zoom dengan LOD (menjarang saat zoom-out, titik mayor tiap
/// 4 langkah), dan vignette tipis di tepi supaya fokus jatuh ke tengah.
fn draw_premium_bg(painter: &egui::Painter, rect: egui::Rect, pan: egui::Vec2, zoom: f32) {
    // 1) gradien dasar (atas sedikit lebih terang → bawah lebih pekat).
    let c_top = egui::Color32::from_rgb(0x1b, 0x1e, 0x28);
    let c_bot = egui::Color32::from_rgb(0x0f, 0x11, 0x16);
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), c_top);
    mesh.colored_vertex(rect.right_top(), c_top);
    mesh.colored_vertex(rect.right_bottom(), c_bot);
    mesh.colored_vertex(rect.left_bottom(), c_bot);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(egui::Shape::mesh(mesh));

    // 2) dot-grid dunia (bergerak bersama pan, berskala bersama zoom).
    //    LOD: jaga jarak layar antar titik di 24..96 px dengan kelipatan 2,
    //    lalu alpha di-fade mengikuti kepadatan supaya transisi LOD mulus.
    let mut step = 32.0f32;
    while step * zoom < 24.0 {
        step *= 2.0;
    }
    while step * zoom > 96.0 {
        step /= 2.0;
    }
    let sp = step * zoom;
    let t = ((sp - 24.0) / 72.0).clamp(0.0, 1.0);
    let minor_a = (10.0 + t * 16.0) as u8;
    let major_a = (26.0 + t * 26.0) as u8;
    let origin = rect.min + pan; // titik dunia (0,0) di layar
    let i0 = ((rect.min.x - origin.x) / sp).floor() as i64;
    let i1 = ((rect.max.x - origin.x) / sp).ceil() as i64;
    let j0 = ((rect.min.y - origin.y) / sp).floor() as i64;
    let j1 = ((rect.max.y - origin.y) / sp).ceil() as i64;
    if (i1 - i0 + 1) * (j1 - j0 + 1) <= 12_000 {
        let minor = egui::Color32::from_rgba_unmultiplied(0x94, 0xa3, 0xb8, minor_a);
        let major = egui::Color32::from_rgba_unmultiplied(0xb6, 0xc2, 0xd4, major_a);
        for i in i0..=i1 {
            for j in j0..=j1 {
                let pos = origin + egui::vec2(i as f32 * sp, j as f32 * sp);
                if i.rem_euclid(4) == 0 && j.rem_euclid(4) == 0 {
                    painter.circle_filled(pos, 1.7, major);
                } else {
                    painter.circle_filled(pos, 1.1, minor);
                }
            }
        }
    }

    // 3) vignette tipis: strip gradasi gelap di keempat tepi.
    let v = egui::Color32::from_rgba_unmultiplied(0, 0, 0, 64);
    let clear = egui::Color32::TRANSPARENT;
    let d = 90.0f32.min(rect.height() / 4.0);
    let strips = [
        (rect.left_top(), rect.right_top(), egui::vec2(0.0, d)), // atas
        (rect.left_bottom(), rect.right_bottom(), egui::vec2(0.0, -d)), // bawah
    ];
    for (a, b, dir) in strips {
        let mut m = egui::Mesh::default();
        m.colored_vertex(a, v);
        m.colored_vertex(b, v);
        m.colored_vertex(b + dir, clear);
        m.colored_vertex(a + dir, clear);
        m.add_triangle(0, 1, 2);
        m.add_triangle(0, 2, 3);
        painter.add(egui::Shape::mesh(m));
    }
}

/// Blok JSON pretty utk log: label + tiap baris ter-indentasi; dipangkas
/// bila raksasa (lengkapnya selalu ada di panel kanan / copy JSON).
fn pretty_block(label: &str, v: &serde_json::Value) -> String {
    let pretty = serde_json::to_string_pretty(v).unwrap_or_default();
    let mut out = format!("\n      {label}:");
    for (i, l) in pretty.lines().enumerate() {
        if i >= 40 {
            out.push_str("\n        \u{2026} (dipangkas \u{2014} lengkap di panel kanan)");
            break;
        }
        out.push_str("\n        ");
        out.push_str(l);
    }
    out
}

fn rgb(hex: u32) -> egui::Color32 {
    egui::Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// Aksen utama flowrun — kuning-emas listrik (⚡). Dipakai konsisten di seluruh
/// tema: seleksi, fokus, tombol utama, garis hero.
fn accent() -> egui::Color32 {
    rgb(0xf5b83d)
}
/// Teks di atas tombol beraksen (kontras gelap agar tetap terbaca).
fn on_accent() -> egui::Color32 {
    rgb(0x1a1205)
}

/// Pasang font fallback bercakupan-luas agar simbol/panah (→ ◇ ⟳ …) yang
/// tak ada di font default egui tak muncul sebagai kotak tofu (□). Font
/// default tetap prioritas utama untuk teks Latin — fallback hanya dipakai
/// saat glyph tak ditemukan. Cari kandidat lintas-OS; diam bila tak ada.
fn apply_fonts(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf", // macOS — cakupan penuh BMP
        "/System/Library/Fonts/Apple Symbols.ttf",              // macOS — arrow/geometric
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",      // Linux
        "C:/Windows/Fonts/arial.ttf",                           // Windows
    ];
    // Muat SEMUA kandidat yang ada, bukan berhenti di font pertama: cakupan glyph =
    // GABUNGAN semua font. Di macOS, Arial Unicode (BMP luas) sendiri tak punya
    // sebagian panah (mis. ⤢ U+2922 tombol Fit) — Apple Symbols menutup celah itu.
    let mut fonts = egui::FontDefinitions::default();
    let mut keys = Vec::new();
    for (i, p) in CANDIDATES.iter().enumerate() {
        if let Ok(b) = std::fs::read(p) {
            let key = format!("flowrun_fallback_{i}");
            fonts
                .font_data
                .insert(key.clone(), egui::FontData::from_owned(b));
            keys.push(key);
        }
    }
    if keys.is_empty() {
        return;
    }
    // Tempel semua sebagai fallback TERAKHIR (urut kandidat) untuk kedua family.
    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let list = fonts.families.entry(fam).or_default();
        for key in &keys {
            list.push(key.clone());
        }
    }
    ctx.set_fonts(fonts);
}

/// Tema premium global (gelap, sudut membulat, aksen emas). Dipanggil sekali
/// saat app dibuat sehingga picker & layar run seragam.
fn apply_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let mut v = egui::Visuals::dark();
    let a = accent();
    let round = egui::Rounding::same(9.0);

    v.panel_fill = rgb(0x0d0f15);
    v.window_fill = rgb(0x141824);
    v.window_rounding = egui::Rounding::same(12.0);
    v.window_stroke = egui::Stroke::new(1.0, rgb(0x272e40));
    v.extreme_bg_color = rgb(0x090b10); // background TextEdit
    v.faint_bg_color = rgb(0x161b27);
    v.override_text_color = Some(rgb(0xdbe1ea));
    v.hyperlink_color = a;
    v.selection.bg_fill = egui::Color32::from_rgba_unmultiplied(0xf5, 0xb8, 0x3d, 60);
    v.selection.stroke = egui::Stroke::new(1.0, a);

    v.widgets.noninteractive.rounding = round;
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, rgb(0x1e2536));

    v.widgets.inactive.rounding = round;
    v.widgets.inactive.bg_fill = rgb(0x1b2130);
    v.widgets.inactive.weak_bg_fill = rgb(0x1b2130);
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, rgb(0x2a3244));
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, rgb(0xc4ccd8));

    v.widgets.hovered.rounding = round;
    v.widgets.hovered.bg_fill = rgb(0x232b3c);
    v.widgets.hovered.weak_bg_fill = rgb(0x232b3c);
    v.widgets.hovered.bg_stroke = egui::Stroke::new(
        1.2,
        egui::Color32::from_rgba_unmultiplied(0xf5, 0xb8, 0x3d, 150),
    );
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, rgb(0xf0f4fa));

    v.widgets.active.rounding = round;
    v.widgets.active.bg_fill = rgb(0x2b3446);
    v.widgets.active.weak_bg_fill = rgb(0x2b3446);
    v.widgets.active.bg_stroke = egui::Stroke::new(1.4, a);

    style.visuals = v;
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.spacing.interact_size.y = 26.0;

    use egui::{FontFamily, FontId, TextStyle};
    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(24.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(14.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(12.5, FontFamily::Monospace),
        ),
        (
            TextStyle::Small,
            FontId::new(11.0, FontFamily::Proportional),
        ),
    ]
    .into();
    ctx.set_style(style);
}

/// Tombol utama beraksen emas, lebar penuh — CTA "megah".
fn primary_button(ui: &mut egui::Ui, text: &str, min_w: f32) -> egui::Response {
    ui.add(
        egui::Button::new(
            egui::RichText::new(text)
                .strong()
                .size(15.0)
                .color(on_accent()),
        )
        .fill(accent())
        .rounding(egui::Rounding::same(10.0))
        .min_size(egui::vec2(min_w, 42.0)),
    )
}

/// Chip kecil membulat (badge) — bahan dasar panel premium.
fn pill(ui: &mut egui::Ui, text: &str, bg: egui::Color32, fg: egui::Color32, mono: bool) {
    egui::Frame::default()
        .fill(bg)
        .rounding(egui::Rounding::same(999.0))
        .inner_margin(egui::Margin::symmetric(8.0, 3.0))
        .show(ui, |ui| {
            let mut t = egui::RichText::new(text).color(fg).size(11.0);
            if mono {
                t = t.monospace();
            }
            ui.label(t);
        });
}

/// Warna badge HTTP method ala API client (Postman/Insomnia).
fn method_color(m: &str) -> egui::Color32 {
    match m {
        "GET" => rgb(0x10b981),
        "POST" => rgb(0xf59e0b),
        "PUT" => rgb(0x38bdf8),
        "PATCH" => rgb(0xa78bfa),
        "DELETE" => rgb(0xef4444),
        _ => rgb(0x64748b),
    }
}

fn state_color(s: NodeState) -> egui::Color32 {
    match s {
        NodeState::Ok => rgb(0x22c55e),
        NodeState::Fail => rgb(0xef4444),
        NodeState::Skip => rgb(0x94a3b8),
        NodeState::Manual => rgb(0xa78bfa),
        NodeState::Current => rgb(0x3b82f6),
        NodeState::Idle => rgb(0x475569),
    }
}

/// Kartu panel: frame gelap membulat ber-stroke halus.
fn card<R>(ui: &mut egui::Ui, f: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::default()
        .fill(rgb(0x1a1f2d))
        .stroke(egui::Stroke::new(1.0, rgb(0x272e40)))
        .rounding(egui::Rounding::same(10.0))
        .inner_margin(egui::Margin::same(12.0))
        .outer_margin(egui::Margin::symmetric(0.0, 4.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            f(ui)
        })
        .inner
}

/// Kartu picker: padding lebih lega untuk layar buka (run screen tetap `card`).
fn card_lg<R>(ui: &mut egui::Ui, f: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::default()
        .fill(rgb(0x171c29))
        .stroke(egui::Stroke::new(1.0, rgb(0x272e40)))
        .rounding(egui::Rounding::same(12.0))
        .inner_margin(egui::Margin::same(18.0))
        .outer_margin(egui::Margin::symmetric(0.0, 2.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            f(ui)
        })
        .inner
}

/// Label section kecil huruf kapital — pemisah premium antar blok.
fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.add_space(12.0);
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .size(10.5)
            .color(rgb(0x6b7688))
            .strong(),
    );
    ui.add_space(7.0);
}

fn role_color(r: Role) -> egui::Color32 {
    match r {
        Role::Customer => rgb(0x3b82f6),
        Role::Owner => rgb(0xf59e0b),
        Role::Neutral => rgb(0x64748b),
    }
}

impl App {
    fn state_color(st: NodeState, role: Role) -> egui::Color32 {
        match st {
            NodeState::Current => rgb(0xeab308),
            NodeState::Ok => rgb(0x22c55e),
            NodeState::Fail => rgb(0xef4444),
            NodeState::Skip => rgb(0x94a3b8),
            NodeState::Manual => rgb(0xa78bfa),
            NodeState::Idle => match role {
                Role::Customer => rgb(0x2c4a7a),
                Role::Owner => rgb(0x7a5320),
                Role::Neutral => rgb(0x3a3f4b),
            },
        }
    }

    fn try_start(&mut self) {
        let (Some(f), Some(c)) = (self.picker.flow.clone(), self.picker.cfg.clone()) else {
            self.picker.error = Some("select flow.mmd and flow.yaml first".into());
            return;
        };
        match Session::start(
            &f,
            &c,
            self.picker.to_env_config(),
            self.timeout,
            self.reveal_tokens,
        ) {
            Ok(s) => {
                push_recent(RecentEntry {
                    flow: f,
                    cfg: c,
                    env: self.picker.env.clone(),
                });
                self.picker.recent = load_recent();
                self.session = Some(s);
            }
            Err(e) => self.picker.error = Some(format!("{e:#}")),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        match &mut self.session {
            Some(_) => self.ui_run(ctx),
            None => self.ui_picker(ctx),
        }
    }
}

// ------------------------- layar picker ------------------------

impl App {
    fn ui_picker(&mut self, ctx: &egui::Context) {
        let panel_frame = egui::Frame::none().fill(rgb(0x0d0f15));
        egui::CentralPanel::default()
            .frame(panel_frame)
            .show(ctx, |ui| {
                // Latar premium (gradien + grid halus + vignette) — menyatu
                // dengan kanvas layar run.
                let full = ui.max_rect();
                draw_premium_bg(ui.painter(), full, egui::Vec2::ZERO, 1.0);

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Kolom terpusat lebar-maks agar konten tidak tenggelam
                        // di window besar.
                        let colw = 660.0_f32.min(ui.available_width() - 48.0);
                        let pad = ((ui.available_width() - colw) * 0.5).max(0.0);
                        ui.horizontal(|ui| {
                            ui.add_space(pad);
                            ui.vertical(|ui| {
                                ui.set_width(colw);
                                ui.add_space(34.0);
                                self.picker_hero(ui, colw);
                                ui.add_space(18.0);
                                self.picker_sources(ui);
                                ui.add_space(6.0);
                                self.picker_connection(ui);
                                ui.add_space(16.0);

                                if let Some(err) = self.picker.error.clone() {
                                    egui::Frame::none()
                                        .fill(rgb(0x2a1417))
                                        .stroke(egui::Stroke::new(1.0, rgb(0x5b2026)))
                                        .rounding(egui::Rounding::same(8.0))
                                        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
                                        .show(ui, |ui| {
                                            ui.colored_label(rgb(0xf87171), format!("⚠  {err}"));
                                        });
                                    ui.add_space(10.0);
                                }

                                let ready = self.picker.flow.is_some() && self.picker.cfg.is_some();
                                ui.add_enabled_ui(ready, |ui| {
                                    if primary_button(ui, "▶   Mulai Flow", colw).clicked() {
                                        self.try_start();
                                    }
                                });
                                if !ready {
                                    ui.add_space(4.0);
                                    ui.label(
                                        egui::RichText::new(
                                            "Select flow.mmd + flow.yaml to begin.",
                                        )
                                        .size(11.0)
                                        .color(rgb(0x64748b)),
                                    );
                                }

                                self.picker_recent(ui, colw);
                                ui.add_space(40.0);
                            });
                        });
                    });
            });
    }

    /// Header hero: logo ⚡ besar + judul + tagline + garis aksen.
    fn picker_hero(&self, ui: &mut egui::Ui, colw: f32) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("⚡").size(34.0).color(accent()));
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("flowrun")
                    .size(34.0)
                    .strong()
                    .color(rgb(0xf3f6fb)),
            );
            ui.add_space(8.0);
            ui.add_space(1.0);
            pill(ui, "v0.2", rgb(0x26304a), accent(), true);
        });
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(
                "Visual flow-runner — run multi-step API flows, step-by-step or auto.",
            )
            .size(13.5)
            .color(rgb(0x8b95a7)),
        );
        ui.add_space(12.0);
        // Garis aksen membulat.
        let (bar, _) = ui.allocate_exact_size(egui::vec2(colw, 3.0), egui::Sense::hover());
        let seg = egui::Rect::from_min_size(bar.left_top(), egui::vec2(132.0, 3.0));
        ui.painter()
            .rect_filled(seg, egui::Rounding::same(2.0), accent());
        let rest = egui::Rect::from_min_max(
            egui::pos2(seg.right() + 6.0, bar.top() + 1.0),
            egui::pos2(bar.right(), bar.top() + 2.0),
        );
        ui.painter()
            .rect_filled(rest, egui::Rounding::same(1.0), rgb(0x1e2536));
    }

    /// Kartu sumber flow: 3 baris pemilih file.
    fn picker_sources(&mut self, ui: &mut egui::Ui) {
        section_label(ui, "Flow Source");
        card_lg(ui, |ui| {
            #[derive(Clone, Copy)]
            enum Slot {
                Flow,
                Cfg,
                Env,
            }
            // (slot, ikon, label ringkas, filter dialog, ekstensi, nilai)
            let rows: [(Slot, &str, &str, &str, &[&str], &Option<PathBuf>); 3] = [
                (
                    Slot::Flow,
                    "📄",
                    "Flow",
                    "flow.mmd",
                    &["mmd"],
                    &self.picker.flow,
                ),
                (
                    Slot::Cfg,
                    "⚙",
                    "Config",
                    "flow.yaml",
                    &["yaml", "yml"],
                    &self.picker.cfg,
                ),
                (
                    Slot::Env,
                    "🔑",
                    "Env",
                    "env",
                    &["yaml", "yml"],
                    &self.picker.env,
                ),
            ];
            let mut picked: Option<(Slot, PathBuf)> = None;
            for (i, (slot, icon, label, filter, exts, val)) in rows.iter().enumerate() {
                if i > 0 {
                    ui.add_space(9.0);
                }
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(*icon).size(15.0));
                    ui.add_space(4.0);
                    ui.add_sized(
                        [58.0, 22.0],
                        egui::Label::new(egui::RichText::new(*label).strong().color(rgb(0xc9d2df)))
                            .selectable(false),
                    );
                    ui.add_space(4.0);
                    match val {
                        Some(p) => {
                            let name = p
                                .file_name()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_default();
                            let dir = p
                                .parent()
                                .map(|d| d.display().to_string())
                                .unwrap_or_default();
                            ui.label(egui::RichText::new(name).monospace().color(rgb(0xe5e9f0)))
                                .on_hover_text(p.display().to_string());
                            ui.label(egui::RichText::new(dir).size(10.5).color(rgb(0x5c6678)));
                        }
                        None => {
                            let hint = if matches!(slot, Slot::Env) {
                                "(opsional — token & base_url)"
                            } else {
                                "(none selected)"
                            };
                            ui.label(egui::RichText::new(hint).italics().color(rgb(0x6b7688)));
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Select…").clicked() {
                            let mut dlg = rfd::FileDialog::new();
                            if !exts.is_empty() {
                                dlg = dlg.add_filter(*filter, exts);
                            }
                            if let Some(p) = dlg.pick_file() {
                                picked = Some((*slot, p));
                            }
                        }
                    });
                });
            }
            if let Some((slot, p)) = picked {
                match slot {
                    Slot::Flow => self.picker.on_flow_picked(p),
                    Slot::Cfg => {
                        self.picker.cfg = Some(p);
                        self.picker.reload_draft();
                    }
                    Slot::Env => {
                        self.picker.env = Some(p);
                        self.picker.reload_draft();
                    }
                }
            }
        });
    }

    /// Kartu koneksi: base_url + token + vars.
    fn picker_connection(&mut self, ui: &mut egui::Ui) {
        section_label(ui, "Connection");
        card_lg(ui, |ui| {
            ui.label(
                egui::RichText::new("This session only — never written to file.")
                    .size(11.0)
                    .color(rgb(0x64748b)),
            );
            ui.add_space(12.0);
            // Lebar kolom label SERAGAM untuk base_url / token / vars agar semua
            // field mulai di x yang sama. Label kiri-rata (bukan centered).
            const LBL_W: f32 = 150.0;
            let lbl = |ui: &mut egui::Ui, text: egui::RichText| {
                ui.allocate_ui_with_layout(
                    egui::vec2(LBL_W, 24.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.add(egui::Label::new(text).selectable(false));
                    },
                );
            };

            ui.horizontal(|ui| {
                lbl(ui, egui::RichText::new("base_url").color(rgb(0xa7b0be)));
                ui.add(
                    egui::TextEdit::singleline(&mut self.picker.base_url)
                        .desired_width(ui.available_width())
                        .hint_text("https://host-dev"),
                );
            });
            ui.add_space(10.0);
            ui.checkbox(&mut self.picker.show_tokens, "show tokens");
            let show = self.picker.show_tokens;
            for (name, val) in &mut self.picker.tokens {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    lbl(
                        ui,
                        egui::RichText::new(format!("token {name}")).color(rgb(0xa7b0be)),
                    );
                    ui.add(
                        egui::TextEdit::singleline(val)
                            .password(!show)
                            .desired_width(ui.available_width())
                            .hint_text("eyJ…"),
                    );
                });
            }

            // ---- vars seed: toggle manual (tanpa indent collapsing) ----
            ui.add_space(12.0);
            let arrow = if self.picker.vars_open { "▾" } else { "▸" };
            if ui
                .add(
                    egui::Label::new(
                        egui::RichText::new(format!("{arrow}  vars seed")).color(rgb(0x9aa4b4)),
                    )
                    .sense(egui::Sense::click()),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                self.picker.vars_open = !self.picker.vars_open;
            }

            if self.picker.vars_open {
                ui.add_space(8.0);
                let mut del: Option<usize> = None;
                for (i, (k, v)) in self.picker.vars.iter_mut().enumerate() {
                    if i > 0 {
                        ui.add_space(6.0);
                    }
                    ui.horizontal(|ui| {
                        lbl(ui, egui::RichText::new(k.as_str()).monospace());
                        // 🗑 dikunci ke kanan; field mengisi sisa → lebar &
                        // posisi ikon identik antar-baris.
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("🗑").clicked() {
                                del = Some(i);
                            }
                            ui.add_space(4.0);
                            ui.add(
                                egui::TextEdit::singleline(v).desired_width(ui.available_width()),
                            );
                        });
                    });
                }
                if let Some(i) = del {
                    self.picker.vars.remove(i);
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [LBL_W, 24.0],
                        egui::TextEdit::singleline(&mut self.picker.new_var_k).hint_text("key"),
                    );
                    let mut do_add = false;
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        do_add = ui.button("+ add").clicked();
                        ui.add_space(4.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.picker.new_var_v)
                                .desired_width(ui.available_width())
                                .hint_text("value"),
                        );
                    });
                    if do_add && !self.picker.new_var_k.trim().is_empty() {
                        self.picker.vars.push((
                            self.picker.new_var_k.trim().to_string(),
                            self.picker.new_var_v.clone(),
                        ));
                        self.picker.new_var_k.clear();
                        self.picker.new_var_v.clear();
                    }
                });
            }
        });
    }

    /// Daftar flow terakhir dibuka — kartu klik.
    fn picker_recent(&mut self, ui: &mut egui::Ui, colw: f32) {
        if self.picker.recent.is_empty() {
            return;
        }
        ui.add_space(10.0);
        section_label(ui, "Recently Opened");
        let recents = self.picker.recent.clone();
        for r in recents {
            let name = r
                .flow
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let dir = r
                .flow
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let resp = egui::Frame::none()
                .fill(rgb(0x161b27))
                .stroke(egui::Stroke::new(1.0, rgb(0x242c3d)))
                .rounding(egui::Rounding::same(10.0))
                .inner_margin(egui::Margin::symmetric(14.0, 11.0))
                .outer_margin(egui::Margin::symmetric(0.0, 4.0))
                .show(ui, |ui| {
                    ui.set_width(colw - 30.0);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("📂").size(15.0));
                        ui.add_space(2.0);
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new(&name).strong().color(rgb(0xe5e9f0)));
                            ui.label(egui::RichText::new(&dir).size(10.5).color(rgb(0x5c6678)));
                        });
                    });
                });
            let r_int = resp.response.interact(egui::Sense::click());
            if r_int.hovered() {
                ui.painter().rect_stroke(
                    r_int.rect,
                    egui::Rounding::same(9.0),
                    egui::Stroke::new(1.2, accent()),
                );
                ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
            }
            if r_int.clicked() {
                self.picker.flow = Some(r.flow.clone());
                self.picker.cfg = Some(r.cfg.clone());
                self.picker.env = r.env.clone();
                self.picker.reload_draft();
            }
        }
    }
}

// -------------------------- layar run --------------------------

impl App {
    fn ui_run(&mut self, ctx: &egui::Context) {
        let mut back_to_picker = false;
        let sess = self.session.as_mut().expect("dicek pemanggil");
        sess.drain_events();

        // ---- toolbar ----
        egui::TopBottomPanel::top("bar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("📂 Open").clicked() {
                    back_to_picker = true;
                }
                ui.separator();
                let can_step = !sess.finished && !sess.auto_running && sess.pending.is_none();
                if ui
                    .add_enabled(can_step, egui::Button::new("▶ Next"))
                    .clicked()
                {
                    let _ = sess.tx.send(Cmd::Next);
                }
                if ui
                    .add_enabled(can_step, egui::Button::new("⏭ Skip"))
                    .clicked()
                {
                    let _ = sess.tx.send(Cmd::Skip);
                }
                if ui
                    .add_enabled(can_step, egui::Button::new("⏩ Auto"))
                    .clicked()
                {
                    sess.auto_running = true;
                    let _ = sess.tx.send(Cmd::Auto);
                }
                if ui.button("↺ Reset").clicked() {
                    let _ = sess.tx.send(Cmd::Reset);
                }
                ui.separator();
                let mut dir = sess.dir;
                if sess.linear {
                    ui.selectable_value(&mut dir, LayoutDir::Snake, "🐍 Snake")
                        .on_hover_text("wrap into rows — fit on screen");
                }
                ui.selectable_value(&mut dir, LayoutDir::LR, "⇉ LR");
                ui.selectable_value(&mut dir, LayoutDir::TD, "⇊ TD");
                sess.set_dir(dir);
                if ui.button("⤢ Fit").clicked() {
                    sess.fitted = false;
                }
                ui.toggle_value(&mut sess.follow, "👁 Follow");
                ui.separator();
                let visited = sess
                    .states
                    .iter()
                    .filter(|s| !matches!(s, NodeState::Idle))
                    .count();
                let done = sess
                    .states
                    .iter()
                    .filter(|s| matches!(s, NodeState::Ok))
                    .count();
                let fail = sess
                    .states
                    .iter()
                    .filter(|s| matches!(s, NodeState::Fail))
                    .count();
                let badge = if sess.finished { "  🎉 done" } else { "" };
                ui.label(format!(
                    "visited {visited}/{}   ✅ {done}  ❌ {fail}{badge}",
                    sess.total
                ));
                legend(ui, rgb(0x3b82f6), "Customer");
                legend(ui, rgb(0xf59e0b), "Owner");
                ui.separator();
                ui.colored_label(
                    rgb(0x94a3b8),
                    egui::RichText::new(format!("\u{1F3AF} {}", sess.base_url))
                        .monospace()
                        .size(11.0),
                )
                .on_hover_text("base_url env — host hit by every step");
                ui.small("pinch / \u{2318}+scroll = zoom \u{00b7} scroll = pan \u{00b7} drag = pan \u{00b7} F = fit \u{00b7} \u{2318}0 = 100%");
            });
            ui.add_space(4.0);
        });

        // ---- log bawah ----
        // Aksi lipatan dikumpulkan di sini lalu diterapkan SETELAH panel, agar
        // tak perlu &mut sess di dalam closure yang meminjam sess.log immutable.
        let mut toggle_log: Option<usize> = None;
        let mut bulk_log: Option<bool> = None;
        egui::TopBottomPanel::bottom("log")
            .resizable(true)
            .default_height(130.0)
            .show(ctx, |ui| {
                // Header: judul + tombol buka/tutup SEMUA detail sekaligus.
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("LOG")
                            .color(rgb(0x6b7280))
                            .size(10.5)
                            .strong(),
                    );
                    if sess.log.iter().any(|l| l.detail.is_some()) {
                        let any_collapsed =
                            sess.log.iter().any(|l| l.detail.is_some() && !l.expanded);
                        let label = if any_collapsed {
                            "\u{25be} expand all"
                        } else {
                            "\u{25b8} collapse all"
                        };
                        if ui.small_button(label).clicked() {
                            bulk_log = Some(any_collapsed);
                        }
                    }
                });
                ui.separator();
                // Scroll DUA arah: baris panjang (curl/auth/JSON) bisa digeser
                // horizontal alih-alih terpotong di tepi kanan.
                egui::ScrollArea::both()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (i, line) in sess.log.iter().enumerate() {
                            // push_id: tiap baris punya namespace ID sendiri, agar
                            // klik satu entri tak bertabrakan ID dengan entri lain
                            // atau tombol expand-all (klik satu = expand satu).
                            ui.push_id(i, |ui| {
                                ui.horizontal_top(|ui| {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(format!("T+{:6.1}s", line.t))
                                                .color(rgb(0x6b7280))
                                                .monospace()
                                                .size(10.5),
                                        )
                                        .wrap_mode(egui::TextWrapMode::Extend),
                                    );
                                    // Penanda lipatan ▸/▾ hanya bila entri punya detail.
                                    let marker = match &line.detail {
                                        Some(_) if line.expanded => "\u{25be} ", // ▾
                                        Some(_) => "\u{25b8} ",                  // ▸
                                        None => "  ",
                                    };
                                    let label = egui::Label::new(
                                        egui::RichText::new(format!("{marker}{}", line.text))
                                            .color(line.color)
                                            .monospace()
                                            .size(11.0),
                                    )
                                    .wrap_mode(egui::TextWrapMode::Extend);
                                    if line.detail.is_some() {
                                        let r = ui
                                            .add(label.sense(egui::Sense::click()))
                                            .on_hover_text("click to expand/collapse");
                                        if r.clicked() {
                                            toggle_log = Some(i);
                                        }
                                    } else {
                                        ui.add(label);
                                    }
                                });
                                if line.expanded {
                                    if let Some(d) = &line.detail {
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(d)
                                                    .color(line.color)
                                                    .monospace()
                                                    .size(11.0),
                                            )
                                            .wrap_mode(egui::TextWrapMode::Extend),
                                        );
                                    }
                                }
                            });
                        }
                    });
            });
        if let Some(i) = toggle_log {
            sess.log[i].expanded = !sess.log[i].expanded;
        }
        if let Some(v) = bulk_log {
            for l in sess.log.iter_mut() {
                if l.detail.is_some() {
                    l.expanded = v;
                }
            }
        }

        // ---- inspector kanan ----
        egui::SidePanel::right("inspector")
            .resizable(true)
            .default_width(380.0)
            .frame(
                egui::Frame::default()
                    .fill(rgb(0x12151f))
                    .inner_margin(egui::Margin::same(12.0)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if let Some(i) = sess.selected {
                        let m = sess.meta[i].clone();
                        let st = sess.states[i];

                        // ── kartu header: status + judul + chips ──
                        card(ui, |ui| {
                            ui.horizontal(|ui| {
                                let (r, _) = ui.allocate_exact_size(
                                    egui::vec2(12.0, 12.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().circle_filled(r.center(), 5.0, state_color(st));
                                ui.painter().circle_stroke(
                                    r.center(),
                                    5.5,
                                    egui::Stroke::new(1.0, state_color(st).gamma_multiply(0.4)),
                                );
                                ui.label(egui::RichText::new(&m.title).size(15.0).strong());
                            });
                            ui.add_space(6.0);
                            ui.horizontal_wrapped(|ui| {
                                pill(ui, &m.node_id, rgb(0x232a3b), rgb(0x94a3b8), true);
                                match m.role {
                                    Role::Customer => pill(
                                        ui,
                                        "Customer",
                                        egui::Color32::from_rgba_unmultiplied(0x3b, 0x82, 0xf6, 38),
                                        rgb(0x7fb3ff),
                                        false,
                                    ),
                                    Role::Owner => pill(
                                        ui,
                                        "Owner",
                                        egui::Color32::from_rgba_unmultiplied(0xf5, 0x9e, 0x0b, 38),
                                        rgb(0xfacc6b),
                                        false,
                                    ),
                                    Role::Neutral => {
                                        pill(ui, "eksternal", rgb(0x232a3b), rgb(0x94a3b8), false)
                                    }
                                }
                                let (stxt, scol) = match st {
                                    NodeState::Ok => ("done", rgb(0x22c55e)),
                                    NodeState::Fail => ("failed", rgb(0xef4444)),
                                    NodeState::Skip => ("skipped", rgb(0x94a3b8)),
                                    NodeState::Manual => ("manual", rgb(0xa78bfa)),
                                    NodeState::Current => ("running", rgb(0x3b82f6)),
                                    NodeState::Idle => ("antre", rgb(0x64748b)),
                                };
                                pill(ui, stxt, scol.gamma_multiply(0.16), scol, false);
                            });
                        });

                        // ── kartu request: badge method + path + aksi ──
                        card(ui, |ui| {
                            let (method, path) = m
                                .endpoint
                                .split_once(' ')
                                .unwrap_or(("", m.endpoint.as_str()));
                            ui.horizontal_wrapped(|ui| {
                                if !method.is_empty() {
                                    pill(
                                        ui,
                                        method,
                                        method_color(method).gamma_multiply(0.2),
                                        method_color(method),
                                        true,
                                    );
                                }
                                ui.label(
                                    egui::RichText::new(path)
                                        .monospace()
                                        .size(12.0)
                                        .color(rgb(0xcbd5e1)),
                                );
                            });
                            if let Some(n) = &m.note {
                                ui.add_space(6.0);
                                ui.label(
                                    egui::RichText::new(format!("\u{1f4dd} {n}"))
                                        .size(11.0)
                                        .color(rgb(0x8b93a7))
                                        .italics(),
                                );
                            }
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                let hit = egui::Button::new(
                                    egui::RichText::new("\u{27f3}  Run this node")
                                        .color(egui::Color32::WHITE)
                                        .size(12.0),
                                )
                                .fill(rgb(0x4f46e5))
                                .rounding(egui::Rounding::same(8.0));
                                if ui
                                    .add(hit)
                                    .on_hover_text(
                                        "re-run this node now (without advancing the sequence)",
                                    )
                                    .clicked()
                                {
                                    let _ = sess.tx.send(Cmd::RunAt(i));
                                }
                                if let Some(r) = &sess.results[i] {
                                    if let Some(c) = &r.curl
                                        && ui
                                            .small_button("\u{1F4CB} curl")
                                            .on_hover_text("copy curl command")
                                            .clicked()
                                    {
                                        ui.output_mut(|o| o.copied_text = c.clone());
                                    }
                                    if let Some(b) = &r.request_body
                                        && ui.small_button("\u{1F4CB} payload").clicked()
                                    {
                                        ui.output_mut(|o| {
                                            o.copied_text =
                                                serde_json::to_string_pretty(b).unwrap_or_default()
                                        });
                                    }
                                }
                            });
                        });

                        // ── hasil eksekusi ──
                        if let Some(r) = &sess.results[i] {
                            section_label(ui, "result");
                            card(ui, |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    let (scol, stext) = match r.status {
                                        Some(c) if c >= 500 => (rgb(0xef4444), format!("HTTP {c}")),
                                        Some(c) if c >= 400 => (rgb(0xf59e0b), format!("HTTP {c}")),
                                        Some(c) => (rgb(0x22c55e), format!("HTTP {c}")),
                                        None => (rgb(0x94a3b8), "\u{2014}".into()),
                                    };
                                    pill(ui, &stext, scol.gamma_multiply(0.18), scol, true);
                                    pill(
                                        ui,
                                        &format!("{} ms", r.ms),
                                        rgb(0x232a3b),
                                        rgb(0x94a3b8),
                                        true,
                                    );
                                });
                                if let Some(rl) = &r.request_line {
                                    ui.add_space(6.0);
                                    ui.label(
                                        egui::RichText::new(rl.as_str())
                                            .monospace()
                                            .size(10.5)
                                            .color(rgb(0x7c8598)),
                                    );
                                }
                                if let Some(a) = &r.auth_info {
                                    ui.label(
                                        egui::RichText::new(format!("\u{1f511} {a}"))
                                            .monospace()
                                            .size(10.5)
                                            .color(rgb(0x7c8598)),
                                    );
                                }
                                if !r.msg.is_empty() {
                                    ui.add_space(6.0);
                                    ui.colored_label(
                                        if r.state == NodeState::Fail {
                                            rgb(0xf87171)
                                        } else {
                                            rgb(0x9ca3af)
                                        },
                                        &r.msg,
                                    );
                                }
                                for n in &r.notes {
                                    ui.label(
                                        egui::RichText::new(n).size(11.0).color(rgb(0x8b93a7)),
                                    );
                                }
                            });

                            if let Some(body) = &r.body {
                                section_label(ui, "response");
                                card(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        if ui.small_button("\u{1F4CB} copy JSON").clicked() {
                                            let pretty = serde_json::to_string_pretty(body)
                                                .unwrap_or_default();
                                            ui.output_mut(|o| o.copied_text = pretty);
                                        }
                                    });
                                    egui::ScrollArea::vertical()
                                        .max_height(300.0)
                                        .show(ui, |ui| {
                                            json_tree(ui, &format!("resp{i}"), "response", body);
                                        });
                                });
                            }
                        } else {
                            card(ui, |ui| {
                                ui.vertical_centered(|ui| {
                                    ui.add_space(10.0);
                                    ui.label(
                                        egui::RichText::new("\u{25c7}")
                                            .size(22.0)
                                            .color(rgb(0x3a4258)),
                                    );
                                    ui.label(
                                        egui::RichText::new("not run yet").color(rgb(0x8b93a7)),
                                    );
                                    ui.label(
                                        egui::RichText::new(
                                            "press  \u{25b6} Next  or  \u{27f3} Run this node",
                                        )
                                        .size(11.0)
                                        .color(rgb(0x5b6377)),
                                    );
                                    ui.add_space(10.0);
                                });
                            });
                        }
                    } else {
                        card(ui, |ui| {
                            ui.vertical_centered(|ui| {
                                ui.add_space(14.0);
                                ui.label(egui::RichText::new("\u{1f446}").size(20.0));
                                ui.label(
                                    egui::RichText::new("click a node to inspect")
                                        .color(rgb(0x8b93a7)),
                                );
                                ui.add_space(14.0);
                            });
                        });
                    }

                    section_label(ui, &format!("context vars ({})", sess.vars.len()));
                    card(ui, |ui| {
                        if sess.vars.is_empty() {
                            ui.label(
                                egui::RichText::new("empty — filled via capture")
                                    .size(11.0)
                                    .color(rgb(0x5b6377)),
                            );
                        }
                        for (k, v) in &sess.vars {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    egui::RichText::new(k)
                                        .monospace()
                                        .size(11.0)
                                        .color(rgb(0x7fb3ff)),
                                );
                                ui.label(egui::RichText::new("=").size(11.0).color(rgb(0x5b6377)));
                                ui.label(
                                    egui::RichText::new(v)
                                        .monospace()
                                        .size(11.0)
                                        .color(rgb(0xcbd5e1)),
                                );
                            });
                        }
                    });
                });
            });

        // ---- modal pilih cabang (cabang ambigu / tanpa kondisi) ----
        let mut chosen: Option<usize> = None;
        if let Some(opts) = sess.pending.clone() {
            egui::Window::new("🔀 Choose branch")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, -40.0])
                .show(ctx, |ui| {
                    ui.label("Runner reached a branch — which path to take?");
                    ui.add_space(6.0);
                    for (t, label) in &opts {
                        let m = &sess.meta[*t];
                        let txt = if label.is_empty() {
                            format!("→ {}", m.title)
                        } else {
                            format!("→ {}   ({label})", m.title)
                        };
                        if ui
                            .add(egui::Button::new(txt).min_size(egui::vec2(280.0, 28.0)))
                            .clicked()
                        {
                            chosen = Some(*t);
                        }
                    }
                });
        }
        if let Some(t) = chosen {
            let _ = sess.tx.send(Cmd::Choose(t));
            sess.pending = None;
            sess.selected = Some(t);
            if sess.follow {
                sess.center_on = Some(t);
            }
            let title = sess.meta[t].title.clone();
            sess.logln(format!("↪ branch chosen: {title}"), rgb(0xeab308));
        }

        // ---- canvas ----
        egui::CentralPanel::default().show(ctx, |ui| {
            let (resp, painter) =
                ui.allocate_painter(ui.available_size(), egui::Sense::click_and_drag());
            let rect = resp.rect;
            painter.rect_filled(rect, egui::Rounding::ZERO, rgb(0x14161b));

            let margin = 40.0;
            if !sess.fitted && sess.geo.w > 1.0 {
                let sx = (rect.width() - 2.0 * margin) / sess.geo.w;
                let sy = (rect.height() - 2.0 * margin) / sess.geo.h;
                // Fit HARUS bisa zoom-out sekecil apa pun agar seluruh graf muat;
                // floor lama 0.8 memotong graf lebar (mis. Snake banyak kolom).
                sess.zoom = sx.min(sy).clamp(0.1, 2.0);
                sess.pan = egui::vec2(
                    (rect.width() - sess.geo.w * sess.zoom) / 2.0,
                    (rect.height() - sess.geo.h * sess.zoom) / 2.0,
                );
                sess.center_on = Some(sess.selected.unwrap_or(0));
                sess.fitted = true;
            }
            if let Some(i) = sess.center_on.take() {
                // Follow hanya perlu bila flow TIDAK muat penuh di viewport
                // (di mode Ular biasanya muat → view diam, tak loncat-loncat).
                let fits = sess.geo.w * sess.zoom <= rect.width()
                    && sess.geo.h * sess.zoom <= rect.height();
                if !fits && let Some(sn) = sess.geo.nodes.iter().find(|n| n.step == i) {
                    let want = rect.center() - rect.min;
                    sess.pan = want - egui::vec2(sn.center.x * sess.zoom, sn.center.y * sess.zoom);
                }
            }
            // ---- navigasi ala Figma ----
            // pinch trackpad ATAU Cmd/Ctrl+scroll → zoom berlabuh di kursor
            // (egui melipat keduanya ke zoom_delta dan mengeluarkannya dari
            // scroll_delta, jadi tidak dobel); scroll dua jari polos → geser
            // dua sumbu; drag → geser; F = fit; Cmd/Ctrl+0 = 100%.
            let (zoom_delta, scroll_vec) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta));
            if resp.hovered() {
                if zoom_delta != 1.0
                    && let Some(p) = resp.hover_pos()
                {
                    let old = sess.zoom;
                    sess.zoom = (sess.zoom * zoom_delta).clamp(0.15, 6.0);
                    let s = (p - rect.min - sess.pan) / old;
                    sess.pan = (p - rect.min) - s * sess.zoom;
                    sess.follow = false;
                } else if scroll_vec != egui::Vec2::ZERO {
                    sess.pan += scroll_vec;
                    sess.follow = false;
                }
            }
            if resp.dragged() {
                sess.pan += resp.drag_delta();
                sess.follow = false;
            }
            if !ui.ctx().wants_keyboard_input() {
                let (fit_key, hundred) = ui.input(|i| {
                    (
                        i.key_pressed(egui::Key::F),
                        i.modifiers.command && i.key_pressed(egui::Key::Num0),
                    )
                });
                if fit_key {
                    sess.fitted = false; // dipicu ulang blok fit di frame ini/berikut
                }
                if hundred {
                    // 100% berlabuh di tengah viewport.
                    let c = rect.center() - rect.min;
                    let s = (c - sess.pan) / sess.zoom;
                    sess.zoom = 1.0;
                    sess.pan = c - s;
                    sess.follow = false;
                }
            }
            draw_premium_bg(&painter, rect, sess.pan, sess.zoom);
            let zoom = sess.zoom;
            let base = rect.min + sess.pan;
            let tf = |p: egui::Pos2| base + egui::vec2(p.x * zoom, p.y * zoom);
            let time = ui.input(|i| i.time);

            // edges + panah
            for (pts, src) in &sess.geo.edges {
                let done =
                    *src != usize::MAX && matches!(sess.states.get(*src), Some(NodeState::Ok));
                let col = if done { rgb(0x22c55e) } else { rgb(0x4b5563) };
                let poly: Vec<egui::Pos2> = pts.iter().map(|&p| tf(p)).collect();
                if poly.len() >= 2 {
                    painter.add(egui::Shape::line(poly.clone(), egui::Stroke::new(2.0, col)));
                    // Panah di ujung: arah dari 2 titik terakhir.
                    let end = poly[poly.len() - 1];
                    let prev = poly[poly.len() - 2];
                    let d = (end - prev).normalized();
                    if d.length() > 0.1 {
                        let perp = egui::vec2(-d.y, d.x);
                        let sz = (7.0 * zoom).clamp(5.0, 14.0);
                        painter.add(egui::Shape::convex_polygon(
                            vec![
                                end,
                                end - d * sz + perp * sz * 0.55,
                                end - d * sz - perp * sz * 0.55,
                            ],
                            col,
                            egui::Stroke::NONE,
                        ));
                    }
                }
            }

            // nodes
            sess.hitboxes.clear();
            for sn in &sess.geo.nodes {
                let c = tf(sn.center);
                let size = sn.size * zoom;
                let r = egui::Rect::from_center_size(c, size);
                sess.hitboxes.push((sn.step, r));
                let st = sess.states[sn.step];
                let role = sess.meta[sn.step].role;
                let mut fill = Self::state_color(st, role);
                // Jalur selesai → node yang TAK dilalui diredupkan.
                let dimmed = sess.finished && st == NodeState::Idle;
                if dimmed {
                    fill = fill.gamma_multiply(0.35);
                }
                let sel = sess.selected == Some(sn.step);
                painter.rect_filled(r, egui::Rounding::same(6.0), fill);

                // Ring: pulse kuning utk node aktif; putih utk seleksi.
                if st == NodeState::Current {
                    let a = (0.45 + 0.4 * ((time * 5.0).sin() * 0.5 + 0.5)) as f32;
                    let ring =
                        egui::Color32::from_rgba_unmultiplied(0xea, 0xb3, 0x08, (a * 255.0) as u8);
                    painter.rect_stroke(
                        r.expand(3.0),
                        egui::Rounding::same(8.0),
                        egui::Stroke::new(3.0, ring),
                    );
                } else if sel {
                    painter.rect_stroke(
                        r.expand(2.0),
                        egui::Rounding::same(7.0),
                        egui::Stroke::new(2.0, rgb(0xffffff)),
                    );
                }
                painter.rect_stroke(
                    r,
                    egui::Rounding::same(6.0),
                    egui::Stroke::new(1.0, rgb(0x14161b)),
                );

                // Badge role di pojok kiri-atas (role tetap terlihat saat status menimpa warna).
                if size.x > 30.0 {
                    painter.circle_filled(
                        r.min + egui::vec2(7.0, 7.0),
                        (3.5 * zoom).clamp(2.5, 5.0),
                        role_color(role),
                    );
                }

                // Label: font mengikuti zoom PENUH (tanpa floor) supaya selalu
                // muat di kotak; zoom jauh → label disembunyikan (baca via
                // inspector / zoom-in), bukan luber keluar kotak.
                let font_px = 11.0 * zoom;
                if font_px >= 6.5 {
                    let font = egui::FontId::proportional(font_px.min(20.0));
                    let mut txt_col = if matches!(st, NodeState::Idle | NodeState::Current) {
                        rgb(0xffffff)
                    } else {
                        rgb(0x0b1220)
                    };
                    if dimmed {
                        txt_col = txt_col.gamma_multiply(0.5);
                    }
                    let make = |rows: usize| {
                        let mut job = egui::text::LayoutJob::simple(
                            sn.label.clone(),
                            font.clone(),
                            txt_col,
                            size.x - 10.0,
                        );
                        job.wrap.max_rows = rows;
                        job.wrap.overflow_character = Some('…');
                        job.halign = egui::Align::Center;
                        painter.layout_job(job)
                    };
                    let mut galley = make(2);
                    if galley.size().y > size.y - 3.0 {
                        galley = make(1); // fit-check: turunkan ke 1 baris
                    }
                    if galley.size().y <= size.y - 1.0 {
                        painter.galley(
                            egui::pos2(c.x, c.y - galley.size().y / 2.0),
                            galley,
                            txt_col,
                        );
                    }
                }
            }

            if resp.clicked()
                && let Some(pos) = resp.interact_pointer_pos()
            {
                for (step, hb) in sess.hitboxes.iter().rev() {
                    if hb.contains(pos) {
                        sess.selected = Some(*step);
                        break;
                    }
                }
            }
        });

        ctx.request_repaint_after(Duration::from_millis(60));

        if back_to_picker {
            self.session = None; // Drop → worker Quit
        }
    }
}

fn legend(ui: &mut egui::Ui, c: egui::Color32, label: &str) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().rect_filled(r, egui::Rounding::same(2.0), c);
    ui.small(label);
}

/// JSON viewer collapsible sederhana (rekursif).
fn json_tree(ui: &mut egui::Ui, id: &str, key: &str, v: &serde_json::Value) {
    match v {
        serde_json::Value::Object(o) if !o.is_empty() => {
            egui::CollapsingHeader::new(
                egui::RichText::new(format!("{key} {{{}}}", o.len())).monospace(),
            )
            .id_salt(format!("{id}/{key}"))
            .default_open(key == "response" || key == "data")
            .show(ui, |ui| {
                for (k, val) in o {
                    json_tree(ui, &format!("{id}/{key}"), k, val);
                }
            });
        }
        serde_json::Value::Array(a) if !a.is_empty() => {
            egui::CollapsingHeader::new(
                egui::RichText::new(format!("{key} [{}]", a.len())).monospace(),
            )
            .id_salt(format!("{id}/{key}"))
            .show(ui, |ui| {
                for (i, val) in a.iter().enumerate() {
                    json_tree(ui, &format!("{id}/{key}"), &i.to_string(), val);
                }
            });
        }
        _ => {
            ui.monospace(format!("{key}: {v}"));
        }
    }
}

// ============================ main =============================

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut app = App {
        picker: Picker::new(),
        session: None,
        timeout: args.timeout,
        reveal_tokens: args.reveal_tokens,
    };

    // CLI args tetap didukung: langsung mulai bila lengkap.
    if let (Some(f), Some(c)) = (args.flow.clone(), args.config.clone()) {
        app.picker.flow = Some(f);
        app.picker.cfg = Some(c);
        app.picker.env = args.env.clone();
        app.picker.reload_draft();
        app.try_start();
    }

    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 900.0]),
        ..Default::default()
    };
    eframe::run_native(
        "flowrun",
        native,
        Box::new(|cc| {
            apply_fonts(&cc.egui_ctx);
            apply_theme(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}
