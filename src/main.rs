//! flowrun — framework flow-runner dinamis: visual mermaid (flowmaid) +
//! engine runner HTTP terpisah. Flow apa pun = `flow.mmd` (graf) + `flow.yaml`
//! (config runner per node-id) + env file per-deployment (token/base_url).

mod interactive;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use flowrun::config;
use flowrun::diagram;
use flowrun::engine::{Ctx, Outcome, run_step};
use flowrun::flow;
use flowrun::runner_ui::{Ui, print_report};

#[derive(Parser)]
#[command(
    name = "flowrun",
    version,
    about = "Dynamic flow runner: mermaid + HTTP engine, step-by-step"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a flow (interactive step-by-step; --auto for CI/test).
    Run {
        /// Mermaid graph file (.mmd)
        #[arg(short = 'f', long)]
        flow: PathBuf,
        /// Runner sidecar config (.yaml)
        #[arg(short = 'c', long)]
        config: PathBuf,
        /// Env file (base_url + tokens + vars) — do not commit
        #[arg(short = 'e', long)]
        env: PathBuf,
        /// Non-interactive: run all, stop-on-fail, exit code ≠ 0 on failure
        #[arg(long)]
        auto: bool,
        /// Write live status SVG to this file
        #[arg(long)]
        svg: Option<PathBuf>,
        /// Mini preview server (e.g. 127.0.0.1:8787)
        #[arg(long)]
        serve: Option<String>,
        /// Show REAL tokens in curl lines (default masked as ${TOKEN_*}).
        #[arg(long)]
        reveal_tokens: bool,
        /// Override a var (repeatable): --var key=value
        #[arg(long = "var", value_parser = parse_kv)]
        vars: Vec<(String, String)>,
        /// Per-request HTTP timeout (seconds)
        #[arg(long, default_value_t = 20)]
        timeout: u64,
    },
    /// Render .mmd → SVG once (no execution).
    Render {
        #[arg(short = 'f', long)]
        flow: PathBuf,
        #[arg(short = 'o', long, default_value = "out.svg")]
        out: PathBuf,
    },
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .ok_or_else(|| format!("--var format must be key=value, got: {s}"))
}

fn main() {
    let code = match real_main() {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(e) => {
            eprintln!("error: {e:#}");
            2
        }
    };
    std::process::exit(code);
}

fn real_main() -> Result<bool> {
    match Cli::parse().cmd {
        Cmd::Render { flow, out } => {
            let src = std::fs::read_to_string(&flow)
                .with_context(|| format!("read {}", flow.display()))?;
            let svg = flowmaid::render_svg(&src).map_err(|e| anyhow::anyhow!("render: {e:?}"))?;
            std::fs::write(&out, svg).with_context(|| format!("write {}", out.display()))?;
            println!("SVG → {}", out.display());
            Ok(true)
        }
        Cmd::Run {
            flow,
            config,
            env,
            auto,
            svg,
            serve,
            reveal_tokens,
            vars,
            timeout,
        } => {
            let flow_cfg = config::load_flow_config(&config)?;
            let env_cfg = config::load_env_config(&env)?;
            // Validasi token profil yang dideklarasikan flow tersedia & TIDAK
            // kosong di env — token kosong berarti `Bearer ` dikirim diam-diam
            // dan kegagalan baru muncul membingungkan di tengah flow.
            for p in &flow_cfg.auth_profiles {
                match env_cfg.tokens.get(p) {
                    None => {
                        anyhow::bail!(
                            "token for profile '{p}' not found in env file {}",
                            env.display()
                        )
                    }
                    Some(t) if t.trim().is_empty() => {
                        anyhow::bail!(
                            "token for profile '{p}' is EMPTY in env file {}",
                            env.display()
                        )
                    }
                    Some(_) => {}
                }
            }
            let target_host = env_cfg.base_url.trim_end_matches('/').to_string();
            let mut ctx = Ctx::build(&flow_cfg, env_cfg, &vars);
            ctx.reveal_tokens = reveal_tokens;
            if reveal_tokens {
                eprintln!(
                    "\u{26a0} --reveal-tokens ON: curl contains real tokens \u{2014} do not share this log."
                );
            }
            let parsed = flow::load(&flow, flow_cfg)?;

            let shared = serve.as_ref().map(|_| Arc::new(Mutex::new(String::new())));
            let mut ui = Ui::new(&parsed, svg, shared.clone())?;
            if let (Some(addr), Some(shared)) = (&serve, shared) {
                diagram::serve_preview(addr, shared, target_host.clone())?;
            }

            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(timeout))
                .build()?;

            let ok = if auto {
                run_auto(&parsed, &mut ctx, &client, &mut ui)?
            } else {
                interactive::run(&parsed, &mut ctx, &client, &mut ui)?
            };

            let (pass, fail, skip, idle) = ui.tally();
            println!("\n=== summary: ✅ {pass}  ❌ {fail}  ⏭ {skip}  ⚪ {idle} ===");

            // Preview tetap hidup setelah flow selesai (mode interaktif) agar
            // diagram terakhir bisa dilihat di browser. Mode --auto (CI) tetap
            // exit normal supaya exit-code kepakai.
            if let (Some(addr), false) = (&serve, auto) {
                println!("🔎 preview still live at http://{addr} — Ctrl-C to stop");
                loop {
                    std::thread::sleep(Duration::from_secs(3600));
                }
            }
            Ok(ok)
        }
    }
}

fn run_auto(
    parsed: &flow::Flow,
    ctx: &mut Ctx,
    client: &reqwest::blocking::Client,
    ui: &mut Ui,
) -> Result<bool> {
    // Susuri graf dari start mengikuti edge (kondisi dievaluasi atas vars —
    // SETELAH step jalan, agar capture step itu bisa dipakai memilih cabang).
    let total = parsed.steps.len();
    let mut cur = parsed.start;
    let mut ran = 0usize;
    loop {
        let step = &parsed.steps[cur];
        ran += 1;
        ui.set_current(cur)?;
        println!("\n[{ran}/≤{total}] {} ({})", step.title, step.node_id);
        if step.cfg.manual || step.cfg.request.is_none() {
            println!("   ✋ manual — skipped in auto mode");
            ui.set_manual(cur)?;
        } else {
            let rep = run_step(step, ctx, client);
            print_report(&rep);
            match rep.outcome {
                Outcome::Passed => ui.set_ok(cur)?,
                Outcome::Skipped(_) => ui.set_skip(cur)?,
                Outcome::Manual => ui.set_manual(cur)?,
                Outcome::Failed(_) => {
                    ui.set_fail(cur)?;
                    return Ok(false); // stop-on-fail → exit code ≠ 0
                }
            }
        }
        match flowrun::engine::choose_next(&parsed.outgoing(cur), &ctx.vars)? {
            flowrun::engine::Next::Advance(nx) => cur = nx,
            flowrun::engine::Next::End => break,
            flowrun::engine::Next::Pick(opts) => {
                println!(
                    "   ⚠ ambiguous branch at '{}' — auto mode needs deterministic edge conditions:",
                    step.node_id
                );
                for (t, label) in &opts {
                    println!("     → {} ({label})", parsed.steps[*t].node_id);
                }
                anyhow::bail!(
                    "ambiguous branch — add a |var == value| / |else| condition, or run interactively"
                );
            }
        }
    }
    Ok(true)
}
