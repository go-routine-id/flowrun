//! flowrun — framework flow-runner dinamis: visual mermaid (flowmaid) +
//! engine runner HTTP terpisah. Flow apa pun = `flow.mmd` (graf) + `flow.yaml`
//! (config runner per node-id) + env file per-deployment (token/base_url).

mod config;
mod diagram;
mod engine;
mod flow;
mod interactive;
mod runner_ui;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::engine::{run_step, Ctx, Outcome};
use crate::runner_ui::{print_report, Ui};

#[derive(Parser)]
#[command(name = "flowrun", version, about = "Flow runner dinamis: mermaid + engine HTTP next-next")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Jalankan flow (interaktif next-next; --auto untuk CI/test).
    Run {
        /// File graf mermaid (.mmd)
        #[arg(short = 'f', long)]
        flow: PathBuf,
        /// Sidecar config runner (.yaml)
        #[arg(short = 'c', long)]
        config: PathBuf,
        /// Env file (base_url + tokens + vars) — jangan di-commit
        #[arg(short = 'e', long)]
        env: PathBuf,
        /// Non-interaktif: jalankan semua, stop-on-fail, exit code ≠ 0 bila gagal
        #[arg(long)]
        auto: bool,
        /// Tulis SVG status live ke file ini
        #[arg(long)]
        svg: Option<PathBuf>,
        /// Preview server mini (mis. 127.0.0.1:8787)
        #[arg(long)]
        serve: Option<String>,
        /// Override var (bisa berulang): --var kunci=nilai
        #[arg(long = "var", value_parser = parse_kv)]
        vars: Vec<(String, String)>,
        /// Timeout HTTP per-request (detik)
        #[arg(long, default_value_t = 20)]
        timeout: u64,
    },
    /// Render .mmd → SVG sekali (tanpa eksekusi).
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
        .ok_or_else(|| format!("format --var harus kunci=nilai, dapat: {s}"))
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
            let src = std::fs::read_to_string(&flow).with_context(|| format!("baca {}", flow.display()))?;
            let svg = flowmaid::render_svg(&src).map_err(|e| anyhow::anyhow!("render: {e:?}"))?;
            std::fs::write(&out, svg).with_context(|| format!("tulis {}", out.display()))?;
            println!("SVG → {}", out.display());
            Ok(true)
        }
        Cmd::Run { flow, config, env, auto, svg, serve, vars, timeout } => {
            let flow_cfg = config::load_flow_config(&config)?;
            let env_cfg = config::load_env_config(&env)?;
            // Validasi token profil yang dideklarasikan flow tersedia di env.
            for p in &flow_cfg.auth_profiles {
                if !env_cfg.tokens.contains_key(p) {
                    anyhow::bail!("token profil '{p}' tidak ada di env file {}", env.display());
                }
            }
            let mut ctx = Ctx::build(&flow_cfg, env_cfg, &vars);
            let parsed = flow::load(&flow, flow_cfg)?;

            let shared = serve.as_ref().map(|_| Arc::new(Mutex::new(String::new())));
            let mut ui = Ui::new(&parsed, svg, shared.clone())?;
            if let (Some(addr), Some(shared)) = (&serve, shared) {
                diagram::serve_preview(addr, shared)?;
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
            println!("\n=== ringkasan: ✅ {pass}  ❌ {fail}  ⏭ {skip}  ⚪ {idle} ===");
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
    let total = parsed.steps.len();
    for (i, step) in parsed.steps.iter().enumerate() {
        ui.set_current(i)?;
        println!("\n[{}/{}] {} ({})", i + 1, total, step.title, step.node_id);
        if step.cfg.manual || step.cfg.request.is_none() {
            println!("   ✋ manual — dilewati di mode auto");
            ui.set_manual(i)?;
            continue;
        }
        let rep = run_step(step, ctx, client);
        print_report(&rep);
        match rep.outcome {
            Outcome::Passed => ui.set_ok(i)?,
            Outcome::Skipped(_) => ui.set_skip(i)?,
            Outcome::Manual => ui.set_manual(i)?,
            Outcome::Failed(_) => {
                ui.set_fail(i)?;
                return Ok(false); // stop-on-fail → exit code ≠ 0
            }
        }
    }
    Ok(true)
}
