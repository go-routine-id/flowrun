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
    about = "Flow runner dinamis: mermaid + engine HTTP next-next"
)]
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
        /// Tampilkan token ASLI di baris curl (default disamarkan ${TOKEN_*}).
        #[arg(long)]
        reveal_tokens: bool,
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
            let src = std::fs::read_to_string(&flow)
                .with_context(|| format!("baca {}", flow.display()))?;
            let svg = flowmaid::render_svg(&src).map_err(|e| anyhow::anyhow!("render: {e:?}"))?;
            std::fs::write(&out, svg).with_context(|| format!("tulis {}", out.display()))?;
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
                        anyhow::bail!("token profil '{p}' tidak ada di env file {}", env.display())
                    }
                    Some(t) if t.trim().is_empty() => {
                        anyhow::bail!("token profil '{p}' KOSONG di env file {}", env.display())
                    }
                    Some(_) => {}
                }
            }
            let target_host = env_cfg.base_url.trim_end_matches('/').to_string();
            let mut ctx = Ctx::build(&flow_cfg, env_cfg, &vars);
            ctx.reveal_tokens = reveal_tokens;
            if reveal_tokens {
                eprintln!(
                    "\u{26a0} --reveal-tokens AKTIF: curl memuat token asli \u{2014} jangan bagikan log ini."
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
            println!("\n=== ringkasan: ✅ {pass}  ❌ {fail}  ⏭ {skip}  ⚪ {idle} ===");

            // Preview tetap hidup setelah flow selesai (mode interaktif) agar
            // diagram terakhir bisa dilihat di browser. Mode --auto (CI) tetap
            // exit normal supaya exit-code kepakai.
            if let (Some(addr), false) = (&serve, auto) {
                println!("🔎 preview masih hidup di http://{addr} — Ctrl-C untuk berhenti");
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
            println!("   ✋ manual — dilewati di mode auto");
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
                    "   ⚠ cabang ambigu di '{}' — mode auto butuh kondisi edge yang deterministik:",
                    step.node_id
                );
                for (t, label) in &opts {
                    println!("     → {} ({label})", parsed.steps[*t].node_id);
                }
                anyhow::bail!(
                    "cabang ambigu — tambahkan kondisi |var == nilai| / |else|, atau jalankan interaktif"
                );
            }
        }
    }
    Ok(true)
}
