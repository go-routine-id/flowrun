//! flowrun-gui — desktop native (egui). Menggambar graf mermaid via flowmaid
//! `scene` API, menjalankan engine di worker thread, dan menyorot progres
//! (idle → current → ok/fail) langsung di canvas. Tombol Next/Auto/Skip/Reset.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use clap::Parser;
use eframe::egui;

use flowrun::config;
use flowrun::engine::{run_step, Ctx, Outcome};
use flowrun::flow::{self, FlowStep};

#[derive(Parser)]
#[command(name = "flowrun-gui", about = "flowrun desktop (egui) — visual flow runner")]
struct Args {
    #[arg(short = 'f', long)]
    flow: PathBuf,
    #[arg(short = 'c', long)]
    config: PathBuf,
    #[arg(short = 'e', long)]
    env: PathBuf,
    #[arg(long = "var", value_parser = parse_kv)]
    vars: Vec<(String, String)>,
    #[arg(long, default_value_t = 20)]
    timeout: u64,
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .ok_or_else(|| format!("--var harus kunci=nilai: {s}"))
}

// ---- protokol GUI <-> worker ----
enum Cmd {
    Next,
    Skip,
    Auto,
    Reset,
    Quit,
}

#[derive(Clone)]
struct StepResult {
    idx: usize,
    state: NodeState,
    status: Option<u16>,
    ms: u128,
    msg: String,
    notes: Vec<String>,
    body: String,
    vars: Vec<(String, String)>,
}

enum Evt {
    Started(usize),
    Done(StepResult),
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

// ---- worker thread: pemilik Ctx + client, otoritatif atas cursor ----
fn worker(mut ctx: Ctx, steps: Vec<FlowStep>, timeout: u64, rx: Receiver<Cmd>, tx: Sender<Evt>) {
    let initial = ctx.vars.clone();
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout))
        .build()
        .expect("build http client");
    let n = steps.len();
    let mut cursor = 0usize;

    let snapshot = |ctx: &Ctx| ctx.vars.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>();

    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Quit => break,
            Cmd::Reset => {
                ctx.vars = initial.clone();
                cursor = 0;
                let _ = tx.send(Evt::Reset);
            }
            Cmd::Skip => {
                if cursor < n {
                    let _ = tx.send(Evt::Done(StepResult {
                        idx: cursor,
                        state: NodeState::Skip,
                        status: None,
                        ms: 0,
                        msg: "dilewati manual".into(),
                        notes: vec![],
                        body: String::new(),
                        vars: snapshot(&ctx),
                    }));
                    cursor += 1;
                }
            }
            Cmd::Next => {
                if cursor < n && exec(&steps, &mut ctx, &client, &tx, cursor) {
                    cursor += 1;
                }
            }
            Cmd::Auto => {
                while cursor < n {
                    if exec(&steps, &mut ctx, &client, &tx, cursor) {
                        cursor += 1;
                        std::thread::sleep(Duration::from_millis(250)); // animasi kelihatan
                    } else {
                        break; // stop-on-fail
                    }
                }
                let _ = tx.send(Evt::AutoDone);
            }
        }
    }
}

/// Jalankan langkah `i`. Return true bila boleh maju (pass/skip/manual),
/// false bila gagal (biar bisa retry).
fn exec(
    steps: &[FlowStep],
    ctx: &mut Ctx,
    client: &reqwest::blocking::Client,
    tx: &Sender<Evt>,
    i: usize,
) -> bool {
    let _ = tx.send(Evt::Started(i));
    let step = &steps[i];
    let vars = |ctx: &Ctx| ctx.vars.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>();

    if step.cfg.manual || step.cfg.request.is_none() {
        let _ = tx.send(Evt::Done(StepResult {
            idx: i,
            state: NodeState::Manual,
            status: None,
            ms: 0,
            msg: "langkah manual/eksternal".into(),
            notes: vec![],
            body: String::new(),
            vars: vars(ctx),
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
    let body = rep
        .body
        .as_ref()
        .map(|b| serde_json::to_string_pretty(b).unwrap_or_default())
        .unwrap_or_default();
    let _ = tx.send(Evt::Done(StepResult {
        idx: i,
        state,
        status: rep.http_status,
        ms: rep.ms,
        msg,
        notes: rep.notes,
        body,
        vars: vars(ctx),
    }));
    cont
}

// ---- geometri node dari flowmaid scene ----
struct SceneNodeG {
    step: usize,          // indeks langkah (mapping via graph node id)
    center: egui::Pos2,   // koordinat scene (belum diskala)
    size: egui::Vec2,
    label: String,
}
struct Geometry {
    nodes: Vec<SceneNodeG>,
    edges: Vec<(Vec<egui::Pos2>, usize)>, // polyline scene-coords + step sumber (untuk warna)
    w: f32,
    h: f32,
}

fn build_geometry(mermaid_src: &str, node_to_step: &HashMap<String, usize>) -> anyhow::Result<Geometry> {
    let graph = match flowmaid::parser::parse_document(mermaid_src)
        .map_err(|e| anyhow::anyhow!("parse: {e:?}"))?
    {
        flowmaid::model::Document::Flowchart(g) | flowmaid::model::Document::State(g) => g,
        _ => anyhow::bail!("bukan flowchart"),
    };
    let sc = flowmaid::scene::scene(&graph);
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
    // Warnai edge dari step sumber (graph.edges sejajar scene.edges).
    let edges = sc
        .edges
        .iter()
        .enumerate()
        .map(|(k, e)| {
            let pts: Vec<egui::Pos2> = if e.waypoints.len() >= 2 {
                e.waypoints.iter().map(|&(x, y)| egui::pos2(x as f32, y as f32)).collect()
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
    Ok(Geometry { nodes, edges, w: sc.width as f32, h: sc.height as f32 })
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

// ---- meta langkah untuk panel ----
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

fn rgb(hex: u32) -> egui::Color32 {
    egui::Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

struct App {
    meta: Vec<StepMeta>,
    geo: Geometry,
    states: Vec<NodeState>,
    results: Vec<Option<StepResult>>,
    vars: Vec<(String, String)>,
    selected: Option<usize>,
    cursor: usize,
    total: usize,
    auto_running: bool,
    tx: Sender<Cmd>,
    rx: Receiver<Evt>,
    // rect layar terakhir tiap node (untuk hit-test klik).
    hitboxes: Vec<(usize, egui::Rect)>,
    // pan/zoom kanvas (ala n8n).
    zoom: f32,
    pan: egui::Vec2,
    fitted: bool,
    follow: bool,               // view mengikuti node aktif
    center_on: Option<usize>,   // permintaan center (diproses saat canvas tahu rect)
}

impl App {
    fn drain_events(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Evt::Started(i) => {
                    self.states[i] = NodeState::Current;
                    self.selected = Some(i);
                    if self.follow {
                        self.center_on = Some(i);
                    }
                }
                Evt::Done(r) => {
                    let idx = r.idx;
                    self.states[idx] = r.state;
                    self.vars = r.vars.clone();
                    self.selected = Some(idx);
                    if r.state != NodeState::Fail && idx + 1 > self.cursor {
                        self.cursor = idx + 1;
                    }
                    self.results[idx] = Some(r);
                }
                Evt::AutoDone => self.auto_running = false,
                Evt::Reset => {
                    self.states.iter_mut().for_each(|s| *s = NodeState::Idle);
                    self.results.iter_mut().for_each(|r| *r = None);
                    self.cursor = 0;
                    self.selected = None;
                    self.vars.clear();
                    self.auto_running = false;
                }
            }
        }
    }

    fn state_color(&self, st: NodeState, role: Role) -> egui::Color32 {
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
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        self.drain_events();

        // ---- toolbar ----
        egui::TopBottomPanel::top("bar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let can_step = self.cursor < self.total && !self.auto_running;
                if ui.add_enabled(can_step, egui::Button::new("▶ Next")).clicked() {
                    let _ = self.tx.send(Cmd::Next);
                }
                if ui.add_enabled(can_step, egui::Button::new("⏭ Skip")).clicked() {
                    let _ = self.tx.send(Cmd::Skip);
                }
                if ui
                    .add_enabled(self.cursor < self.total && !self.auto_running, egui::Button::new("⏩ Auto"))
                    .clicked()
                {
                    self.auto_running = true;
                    let _ = self.tx.send(Cmd::Auto);
                }
                if ui.button("↺ Reset").clicked() {
                    let _ = self.tx.send(Cmd::Reset);
                }
                if ui.button("⤢ Fit").clicked() {
                    self.fitted = false; // re-fit frame berikutnya
                }
                ui.toggle_value(&mut self.follow, "👁 Follow");
                ui.separator();
                let done = self.states.iter().filter(|s| matches!(s, NodeState::Ok)).count();
                let fail = self.states.iter().filter(|s| matches!(s, NodeState::Fail)).count();
                ui.label(format!("progress {}/{}   ✅ {done}  ❌ {fail}", self.cursor, self.total));
                legend(ui, rgb(0x2c4a7a), "Customer");
                legend(ui, rgb(0x7a5320), "Owner");
                legend(ui, rgb(0x22c55e), "ok");
                legend(ui, rgb(0xef4444), "fail");
                ui.separator();
                ui.small("scroll = zoom · drag = geser");
            });
            ui.add_space(4.0);
        });

        // ---- inspector (kanan) ----
        egui::SidePanel::right("inspector").default_width(360.0).show(ctx, |ui| {
            if let Some(i) = self.selected {
                let m = &self.meta[i];
                let role = match m.role {
                    Role::Customer => "Customer",
                    Role::Owner => "Owner",
                    Role::Neutral => "-",
                };
                ui.heading(&m.title);
                ui.label(format!("{}  ·  {}", m.node_id, role));
                ui.monospace(&m.endpoint);
                if let Some(n) = &m.note {
                    ui.colored_label(rgb(0x9ca3af), format!("📝 {n}"));
                }
                ui.separator();
                if let Some(r) = &self.results[i] {
                    let col = match r.status {
                        Some(c) if c >= 500 => rgb(0xef4444),
                        Some(c) if c >= 400 => rgb(0xf59e0b),
                        Some(_) => rgb(0x22c55e),
                        None => rgb(0x9ca3af),
                    };
                    ui.horizontal(|ui| {
                        ui.label("HTTP");
                        ui.colored_label(col, r.status.map(|c| c.to_string()).unwrap_or("-".into()));
                        ui.label(format!("· {} ms", r.ms));
                    });
                    if !r.msg.is_empty() {
                        ui.colored_label(if r.state == NodeState::Fail { rgb(0xef4444) } else { rgb(0x9ca3af) }, &r.msg);
                    }
                    for n in &r.notes {
                        ui.small(n);
                    }
                    if !r.body.is_empty() {
                        ui.separator();
                        ui.label("response:");
                        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                            ui.monospace(&r.body);
                        });
                    }
                } else {
                    ui.colored_label(rgb(0x9ca3af), "belum dijalankan");
                }
            } else {
                ui.colored_label(rgb(0x9ca3af), "klik node untuk inspeksi");
            }
            ui.separator();
            ui.collapsing("context vars", |ui| {
                for (k, v) in &self.vars {
                    ui.small(format!("{k} = {v}"));
                }
            });
        });

        // ---- canvas (pan + zoom ala n8n) ----
        egui::CentralPanel::default().show(ctx, |ui| {
            let (resp, painter) = ui.allocate_painter(ui.available_size(), egui::Sense::click_and_drag());
            let rect = resp.rect;
            painter.rect_filled(rect, egui::Rounding::ZERO, rgb(0x14161b));

            // Fit-to-view sekali di awal (lihat seluruh flow), lalu user zoom/pan.
            let margin = 40.0;
            if !self.fitted && self.geo.w > 1.0 {
                let sx = (rect.width() - 2.0 * margin) / self.geo.w;
                let sy = (rect.height() - 2.0 * margin) / self.geo.h;
                // Floor 0.8 supaya label terbaca dari awal (ribbon lebar → pan/follow).
                self.zoom = sx.min(sy).clamp(0.8, 2.0);
                self.pan = egui::vec2(
                    (rect.width() - self.geo.w * self.zoom) / 2.0,
                    (rect.height() - self.geo.h * self.zoom) / 2.0,
                );
                self.center_on = Some(0);
                self.fitted = true;
            }
            // Auto-follow: center-kan node aktif (setelah zoom diketahui).
            if let Some(i) = self.center_on.take() {
                if let Some(sn) = self.geo.nodes.iter().find(|n| n.step == i) {
                    let want = rect.center() - rect.min;
                    self.pan = want - egui::vec2(sn.center.x * self.zoom, sn.center.y * self.zoom);
                }
            }

            // Zoom scroll (anchored di kursor).
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 && resp.hovered() {
                if let Some(p) = resp.hover_pos() {
                    let old = self.zoom;
                    self.zoom = (self.zoom * (1.0 + scroll * 0.0015)).clamp(0.15, 6.0);
                    let s = (p - rect.min - self.pan) / old; // titik scene di bawah kursor
                    self.pan = (p - rect.min) - s * self.zoom;
                }
            }
            // Pan drag → user ambil alih (matikan follow).
            if resp.dragged() {
                self.pan += resp.drag_delta();
                self.follow = false;
            }
            let zoom = self.zoom;
            let base = rect.min + self.pan;
            let tf = |p: egui::Pos2| base + egui::vec2(p.x * zoom, p.y * zoom);

            // edges
            for (pts, src) in &self.geo.edges {
                let done = *src != usize::MAX && matches!(self.states.get(*src), Some(NodeState::Ok));
                let col = if done { rgb(0x22c55e) } else { rgb(0x4b5563) };
                let poly: Vec<egui::Pos2> = pts.iter().map(|&p| tf(p)).collect();
                if poly.len() >= 2 {
                    painter.add(egui::Shape::line(poly, egui::Stroke::new(2.0, col)));
                }
            }

            // nodes
            self.hitboxes.clear();
            for sn in &self.geo.nodes {
                let c = tf(sn.center);
                let size = sn.size * zoom;
                let r = egui::Rect::from_center_size(c, size);
                self.hitboxes.push((sn.step, r));
                let st = self.states[sn.step];
                let role = self.meta[sn.step].role;
                let fill = self.state_color(st, role);
                let sel = self.selected == Some(sn.step);
                painter.rect_filled(r, egui::Rounding::same(6.0), fill);
                let border = if sel { rgb(0xffffff) } else { rgb(0x14161b) };
                painter.rect_stroke(r, egui::Rounding::same(6.0), egui::Stroke::new(if sel { 2.5 } else { 1.0 }, border));
                let txt_col = if matches!(st, NodeState::Idle | NodeState::Current) { rgb(0xffffff) } else { rgb(0x0b1220) };
                // Sembunyikan label saat terlalu kecil (zoom jauh) agar tak jadi bercak.
                if size.x > 34.0 {
                    let chars = (size.x / (6.0 * zoom).max(3.0)) as usize;
                    painter.text(
                        c,
                        egui::Align2::CENTER_CENTER,
                        ellipsize(&sn.label, chars.clamp(4, 40)),
                        egui::FontId::proportional((11.0 * zoom).clamp(9.0, 20.0)),
                        txt_col,
                    );
                }
            }

            // klik → pilih node
            if resp.clicked() {
                if let Some(pos) = resp.interact_pointer_pos() {
                    for (step, hb) in self.hitboxes.iter().rev() {
                        if hb.contains(pos) {
                            self.selected = Some(*step);
                            break;
                        }
                    }
                }
            }
        });

        // poll worker (animasi auto + hasil masuk mulus)
        ctx.request_repaint_after(Duration::from_millis(60));
    }
}

fn legend(ui: &mut egui::Ui, c: egui::Color32, label: &str) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().rect_filled(r, egui::Rounding::same(2.0), c);
    ui.small(label);
}

fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}

fn role_of(auth: Option<&str>) -> Role {
    match auth {
        Some("customer") => Role::Customer,
        Some("owner") => Role::Owner,
        _ => Role::Neutral,
    }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let flow_cfg = config::load_flow_config(&args.config)?;
    let env_cfg = config::load_env_config(&args.env)?;
    for p in &flow_cfg.auth_profiles {
        match env_cfg.tokens.get(p) {
            Some(t) if !t.trim().is_empty() => {}
            _ => anyhow::bail!("token profil '{p}' kosong/tak ada di env {}", args.env.display()),
        }
    }
    let ctx = Ctx::build(&flow_cfg, env_cfg, &args.vars);
    let parsed = flow::load(&args.flow, flow_cfg)?;

    let node_to_step: HashMap<String, usize> =
        parsed.steps.iter().enumerate().map(|(i, s)| (s.node_id.clone(), i)).collect();
    let geo = build_geometry(&parsed.mermaid_src, &node_to_step)?;
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
    let timeout = args.timeout;
    std::thread::spawn(move || worker(ctx, steps, timeout, cmd_rx, evt_tx));

    let quit_tx = cmd_tx.clone();
    let app = App {
        meta,
        geo,
        states: vec![NodeState::Idle; total],
        results: vec![None; total],
        vars: Vec::new(),
        selected: None,
        cursor: 0,
        total,
        auto_running: false,
        tx: cmd_tx,
        rx: evt_rx,
        hitboxes: Vec::new(),
        zoom: 1.0,
        pan: egui::Vec2::ZERO,
        fitted: false,
        follow: true,
        center_on: None,
    };

    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1180.0, 640.0]),
        ..Default::default()
    };
    let res = eframe::run_native("flowrun", native, Box::new(|_cc| Ok(Box::new(app))));
    let _ = quit_tx.send(Cmd::Quit);
    res.map_err(|e| anyhow::anyhow!("eframe: {e}"))
}
