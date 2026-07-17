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
        msg: "dilewati manual".into(),
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
            msg: "langkah manual/eksternal".into(),
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
        return Ok(snake_layout(&graph, &sc, node_to_step));
    }

    let nodes = sc
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| SceneNodeG {
            step: *node_to_step.get(&graph.nodes[i].id).unwrap_or(&0),
            center: egui::pos2(n.x as f32, n.y as f32),
            size: egui::vec2(n.w as f32, n.h as f32),
            label: n.label.clone(),
        })
        .collect();
    let edges = sc
        .edges
        .iter()
        .enumerate()
        .map(|(k, e)| {
            let pts: Vec<egui::Pos2> = if e.waypoints.len() >= 2 {
                e.waypoints
                    .iter()
                    .map(|&(x, y)| egui::pos2(x as f32, y as f32))
                    .collect()
            } else {
                sample_bezier(&e.bezier, 18)
            };
            let src_step = graph
                .edges
                .get(k)
                .and_then(|ge| node_to_step.get(&graph.nodes[ge.from].id).copied())
                .unwrap_or(usize::MAX);
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
    graph: &flowmaid::model::Graph,
    sc: &flowmaid::scene::Scene,
    node_to_step: &HashMap<String, usize>,
) -> Geometry {
    let n = sc.nodes.len();
    // Urutan eksekusi: step index → indeks scene-node.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| {
        node_to_step
            .get(&graph.nodes[i].id)
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
            step: *node_to_step.get(&graph.nodes[si].id).unwrap_or(&0),
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
                _ => anyhow::bail!("token profil '{p}' kosong — isi di panel koneksi"),
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
        self.log.push(LogLine { t, text, color });
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
                        let s = b.to_string();
                        let t: String = s.chars().take(400).collect();
                        let ell = if s.chars().count() > 400 {
                            "\u{2026}"
                        } else {
                            ""
                        };
                        line.push_str(&format!("\n      \u{21e2} payload : {t}{ell}"));
                    }
                    if let Some(b) = &r.body {
                        let s = b.to_string();
                        let t: String = s.chars().take(400).collect();
                        let ell = if s.chars().count() > 400 {
                            "\u{2026}"
                        } else {
                            ""
                        };
                        line.push_str(&format!("\n      \u{21e0} response: {t}{ell}"));
                    }
                    if let Some(b) = &r.request_body {
                        let s = b.to_string();
                        let t: String = s.chars().take(400).collect();
                        line.push_str(&format!(
                            "\n      \u{21e2} payload : {t}{}",
                            if s.len() > 400 { "\u{2026}" } else { "" }
                        ));
                    }
                    if let Some(b) = &r.body {
                        let s = b.to_string();
                        let t: String = s.chars().take(400).collect();
                        line.push_str(&format!(
                            "\n      \u{21e0} response: {t}{}",
                            if s.len() > 400 { "\u{2026}" } else { "" }
                        ));
                    }
                    for n in &r.notes {
                        line.push_str(&format!("\n      {n}"));
                    }
                    self.logln(line, col);
                    self.results[idx] = Some(r);
                }
                Evt::NeedChoice(opts) => {
                    let daftar = opts
                        .iter()
                        .map(|(t, l)| format!("{} ({l})", self.meta[*t].title))
                        .collect::<Vec<_>>()
                        .join(" · ");
                    self.logln(format!("🔀 pilih cabang: {daftar}"), rgb(0xeab308));
                    self.pending = Some(opts);
                }
                Evt::FlowDone => {
                    self.finished = true;
                    self.logln(
                        "🎉 jalur selesai — node tak dilalui diredupkan".into(),
                        rgb(0x22c55e),
                    );
                }
                Evt::AutoDone => {
                    self.auto_running = false;
                    self.logln("auto berhenti".into(), rgb(0x9ca3af));
                }
                Evt::Reset => {
                    self.states.iter_mut().for_each(|s| *s = NodeState::Idle);
                    self.results.iter_mut().for_each(|r| *r = None);
                    self.selected = None;
                    self.vars.clear();
                    self.auto_running = false;
                    self.pending = None;
                    self.finished = false;
                    self.logln("reset — siap dari awal".into(), rgb(0x9ca3af));
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
}

impl Picker {
    fn new() -> Self {
        Picker {
            recent: load_recent(),
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

fn rgb(hex: u32) -> egui::Color32 {
    egui::Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
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
            self.picker.error = Some("pilih flow.mmd dan flow.yaml dulu".into());
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
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(12.0);
                ui.heading("⚡ flowrun — buka flow");
                ui.add_space(8.0);

                let pick_row = |ui: &mut egui::Ui,
                                label: &str,
                                val: &Option<PathBuf>,
                                exts: &[&str]|
                 -> Option<PathBuf> {
                    let mut out = None;
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(label).strong());
                        if ui.button("Pilih…").clicked() {
                            let mut dlg = rfd::FileDialog::new();
                            if !exts.is_empty() {
                                dlg = dlg.add_filter(label, exts);
                            }
                            if let Some(p) = dlg.pick_file() {
                                out = Some(p);
                            }
                        }
                        match val {
                            Some(p) => ui.monospace(p.display().to_string()),
                            None => ui.colored_label(rgb(0x9ca3af), "(belum dipilih)"),
                        };
                    });
                    out
                };

                if let Some(p) = pick_row(ui, "flow.mmd", &self.picker.flow, &["mmd"]) {
                    self.picker.on_flow_picked(p);
                }
                if let Some(p) = pick_row(ui, "flow.yaml", &self.picker.cfg, &["yaml", "yml"]) {
                    self.picker.cfg = Some(p);
                    self.picker.reload_draft();
                }
                if let Some(p) = pick_row(ui, "env (opsional)", &self.picker.env, &["yaml", "yml"])
                {
                    self.picker.env = Some(p);
                    self.picker.reload_draft();
                }

                ui.add_space(10.0);
                ui.separator();
                ui.label(
                    egui::RichText::new(
                        "Koneksi (bisa diedit — hanya untuk sesi ini, tidak ditulis ke file)",
                    )
                    .strong(),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("base_url");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.picker.base_url)
                            .desired_width(360.0)
                            .hint_text("https://host-dev"),
                    );
                });
                ui.checkbox(&mut self.picker.show_tokens, "tampilkan token");
                let show = self.picker.show_tokens;
                for (name, val) in &mut self.picker.tokens {
                    ui.horizontal(|ui| {
                        ui.label(format!("token {name}"));
                        ui.add(
                            egui::TextEdit::singleline(val)
                                .password(!show)
                                .desired_width(360.0)
                                .hint_text("eyJ…"),
                        );
                    });
                }
                ui.add_space(4.0);
                ui.collapsing("vars", |ui| {
                    let mut del: Option<usize> = None;
                    for (i, (k, v)) in self.picker.vars.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            ui.monospace(k.as_str());
                            ui.add(egui::TextEdit::singleline(v).desired_width(280.0));
                            if ui.small_button("🗑").clicked() {
                                del = Some(i);
                            }
                        });
                    }
                    if let Some(i) = del {
                        self.picker.vars.remove(i);
                    }
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.picker.new_var_k)
                                .desired_width(120.0)
                                .hint_text("kunci"),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut self.picker.new_var_v)
                                .desired_width(200.0)
                                .hint_text("nilai"),
                        );
                        if ui.button("+ tambah").clicked()
                            && !self.picker.new_var_k.trim().is_empty()
                        {
                            self.picker.vars.push((
                                self.picker.new_var_k.trim().to_string(),
                                self.picker.new_var_v.clone(),
                            ));
                            self.picker.new_var_k.clear();
                            self.picker.new_var_v.clear();
                        }
                    });
                });

                ui.add_space(10.0);
                if let Some(err) = &self.picker.error {
                    ui.colored_label(rgb(0xef4444), err);
                }
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new("▶ Mulai").strong())
                            .min_size(egui::vec2(120.0, 32.0)),
                    )
                    .clicked()
                {
                    self.try_start();
                }

                if !self.picker.recent.is_empty() {
                    ui.add_space(14.0);
                    ui.separator();
                    ui.label(egui::RichText::new("Terakhir dibuka").strong());
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
                        if ui.button(format!("📂 {name} — {dir}")).clicked() {
                            self.picker.flow = Some(r.flow.clone());
                            self.picker.cfg = Some(r.cfg.clone());
                            self.picker.env = r.env.clone();
                            self.picker.reload_draft();
                        }
                    }
                }
            });
        });
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
                if ui.button("📂 Buka").clicked() {
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
                    ui.selectable_value(&mut dir, LayoutDir::Snake, "🐍 Ular")
                        .on_hover_text("lipat jadi beberapa baris — muat layar");
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
                let badge = if sess.finished { "  🎉 selesai" } else { "" };
                ui.label(format!(
                    "dilalui {visited}/{}   ✅ {done}  ❌ {fail}{badge}",
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
                .on_hover_text("base_url env — host yang di-hit semua langkah");
                ui.small("scroll = zoom · drag = geser");
            });
            ui.add_space(4.0);
        });

        // ---- log bawah ----
        egui::TopBottomPanel::bottom("log")
            .resizable(true)
            .default_height(130.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &sess.log {
                            ui.horizontal_top(|ui| {
                                ui.monospace(
                                    egui::RichText::new(format!("T+{:6.1}s", line.t))
                                        .color(rgb(0x6b7280))
                                        .size(10.5),
                                );
                                ui.label(
                                    egui::RichText::new(&line.text)
                                        .color(line.color)
                                        .monospace()
                                        .size(11.0),
                                );
                            });
                        }
                    });
            });

        // ---- inspector kanan ----
        egui::SidePanel::right("inspector")
            .resizable(true)
            .default_width(380.0)
            .show(ctx, |ui| {
                if let Some(i) = sess.selected {
                    let m = sess.meta[i].clone();
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let rc = role_color(m.role);
                        let (r, _) =
                            ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                        ui.painter().circle_filled(r.center(), 5.0, rc);
                        ui.heading(&m.title);
                    });
                    ui.label(format!(
                        "{} · {}",
                        m.node_id,
                        match m.role {
                            Role::Customer => "Customer",
                            Role::Owner => "Owner",
                            Role::Neutral => "-",
                        }
                    ));
                    ui.monospace(&m.endpoint);
                    if let Some(n) = &m.note {
                        ui.colored_label(rgb(0x9ca3af), format!("📝 {n}"));
                    }
                    if ui
                        .button("⟳ Hit node ini")
                        .on_hover_text("jalankan ulang node ini sekarang (tanpa memajukan urutan)")
                        .clicked()
                    {
                        let _ = sess.tx.send(Cmd::RunAt(i));
                    }
                    ui.separator();
                    if let Some(r) = &sess.results[i] {
                        if let Some(rl) = &r.request_line {
                            ui.colored_label(
                                rgb(0x94a3b8),
                                egui::RichText::new(rl.as_str()).monospace().size(11.0),
                            );
                        }
                        if let Some(a) = &r.auth_info {
                            ui.colored_label(
                                rgb(0x94a3b8),
                                egui::RichText::new(format!("\u{1f511} {a}"))
                                    .monospace()
                                    .size(11.0),
                            );
                        }
                        ui.horizontal(|ui| {
                            if let Some(c) = &r.curl
                                && ui
                                    .small_button("\u{1F4CB} curl")
                                    .on_hover_text("copy curl \u{2014} token disamarkan ${TOKEN_*}")
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
                        });
                        let col = match r.status {
                            Some(c) if c >= 500 => rgb(0xef4444),
                            Some(c) if c >= 400 => rgb(0xf59e0b),
                            Some(_) => rgb(0x22c55e),
                            None => rgb(0x9ca3af),
                        };
                        ui.horizontal(|ui| {
                            ui.label("HTTP");
                            ui.colored_label(
                                col,
                                r.status.map(|c| c.to_string()).unwrap_or("-".into()),
                            );
                            ui.label(format!("· {} ms", r.ms));
                            if r.body.is_some() && ui.small_button("📋 copy").clicked() {
                                let pretty = r
                                    .body
                                    .as_ref()
                                    .map(|b| serde_json::to_string_pretty(b).unwrap_or_default())
                                    .unwrap_or_default();
                                ui.output_mut(|o| o.copied_text = pretty);
                            }
                        });
                        if !r.msg.is_empty() {
                            ui.colored_label(
                                if r.state == NodeState::Fail {
                                    rgb(0xef4444)
                                } else {
                                    rgb(0x9ca3af)
                                },
                                &r.msg,
                            );
                        }
                        for n in &r.notes {
                            ui.small(n);
                        }
                        if let Some(body) = &r.body {
                            ui.separator();
                            egui::ScrollArea::vertical()
                                .max_height(320.0)
                                .show(ui, |ui| {
                                    json_tree(ui, &format!("resp{i}"), "response", body);
                                });
                        }
                    } else {
                        ui.colored_label(rgb(0x9ca3af), "belum dijalankan");
                    }
                } else {
                    ui.add_space(6.0);
                    ui.colored_label(rgb(0x9ca3af), "klik node untuk inspeksi");
                }
                ui.separator();
                ui.collapsing("context vars", |ui| {
                    for (k, v) in &sess.vars {
                        ui.small(format!("{k} = {v}"));
                    }
                });
            });

        // ---- modal pilih cabang (cabang ambigu / tanpa kondisi) ----
        let mut chosen: Option<usize> = None;
        if let Some(opts) = sess.pending.clone() {
            egui::Window::new("🔀 Pilih cabang")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, -40.0])
                .show(ctx, |ui| {
                    ui.label("Runner tiba di percabangan — jalur mana yang diambil?");
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
            sess.logln(format!("↪ cabang dipilih: {title}"), rgb(0xeab308));
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
                sess.zoom = sx.min(sy).clamp(0.8, 2.0);
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
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0
                && resp.hovered()
                && let Some(p) = resp.hover_pos()
            {
                let old = sess.zoom;
                sess.zoom = (sess.zoom * (1.0 + scroll * 0.0015)).clamp(0.15, 6.0);
                let s = (p - rect.min - sess.pan) / old;
                sess.pan = (p - rect.min) - s * sess.zoom;
            }
            if resp.dragged() {
                sess.pan += resp.drag_delta();
                sess.follow = false;
            }
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
        viewport: egui::ViewportBuilder::default().with_inner_size([1220.0, 700.0]),
        ..Default::default()
    };
    eframe::run_native("flowrun", native, Box::new(|_cc| Ok(Box::new(app))))
        .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}
