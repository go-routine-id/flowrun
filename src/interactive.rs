//! Mode interaktif next-next: Enter=run, s=skip, r=retry, b=body terakhir,
//! vars=lihat context, q=quit. Visual SVG diperbarui setiap perubahan state.

use std::io::{BufRead, Write};

use anyhow::Result;

use flowrun::engine::{run_step, Ctx, Outcome};
use flowrun::flow::Flow;
use flowrun::runner_ui::{print_report, Ui};

pub fn run(flow: &Flow, ctx: &mut Ctx, client: &reqwest::blocking::Client, ui: &mut Ui) -> Result<bool> {
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut ok_all = true;
    let total = flow.steps.len();

    let mut i = 0usize;
    while i < total {
        let step = &flow.steps[i];
        ui.set_current(i)?;
        let role = step.cfg.auth.as_deref().unwrap_or("-");
        println!("\n[{}/{}] {}  ({} · auth:{})", i + 1, total, step.title, step.node_id, role);
        if let Some(n) = &step.cfg.note {
            println!("   📝 {n}");
        }

        if step.cfg.manual || step.cfg.request.is_none() {
            println!("   langkah manual/eksternal — kerjakan di luar, lalu [Enter]=lanjut  q=quit");
            match prompt(&mut lines)?.as_str() {
                "q" => return Ok(false),
                _ => {
                    ui.set_manual(i)?;
                    i += 1;
                    continue;
                }
            }
        }

        println!("   {}  [Enter]=run  s=skip  vars=context  q=quit", step.cfg.request.as_deref().unwrap_or(""));
        match prompt(&mut lines)?.as_str() {
            "q" => return Ok(false),
            "s" => {
                ui.set_skip(i)?;
                i += 1;
                continue;
            }
            "vars" => {
                for (k, v) in &ctx.vars {
                    println!("   {k} = {v}");
                }
                continue; // tetap di langkah yang sama
            }
            _ => {}
        }

        let rep = run_step(step, ctx, client);
        print_report(&rep);
        match rep.outcome {
            Outcome::Passed => {
                ui.set_ok(i)?;
                i += 1;
            }
            Outcome::Skipped(_) => {
                ui.set_skip(i)?;
                i += 1;
            }
            Outcome::Manual => {
                ui.set_manual(i)?;
                i += 1;
            }
            Outcome::Failed(_) => {
                ok_all = false;
                ui.set_fail(i)?;
                println!("   r=retry  s=skip  q=quit");
                match prompt(&mut lines)?.as_str() {
                    "r" => continue, // ulangi langkah yang sama
                    "s" => {
                        ui.set_skip(i)?;
                        i += 1;
                    }
                    _ => return Ok(false),
                }
            }
        }
    }
    Ok(ok_all)
}

fn prompt(lines: &mut impl Iterator<Item = std::io::Result<String>>) -> Result<String> {
    print!("→ ");
    std::io::stdout().flush()?;
    Ok(lines
        .next()
        .transpose()?
        .unwrap_or_else(|| "q".to_string())
        .trim()
        .to_lowercase())
}
