//! Mode interaktif next-next: Enter=run, s=skip, r=retry, vars=context,
//! q=quit. Menyusuri graf; di cabang ambigu, user memilih jalur (bernomor).
//! Visual SVG diperbarui setiap perubahan state.

use std::io::{BufRead, Write};

use anyhow::Result;

use flowrun::engine::{choose_next, run_step, Ctx, Next, Outcome};
use flowrun::flow::Flow;
use flowrun::runner_ui::{print_report, Ui};

pub fn run(flow: &Flow, ctx: &mut Ctx, client: &reqwest::blocking::Client, ui: &mut Ui) -> Result<bool> {
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut ok_all = true;
    let total = flow.steps.len();
    let mut cur = flow.start;
    let mut ran = 0usize;

    'walk: loop {
        let step = &flow.steps[cur];
        ran += 1;
        ui.set_current(cur)?;
        let role = step.cfg.auth.as_deref().unwrap_or("-");
        println!("\n[{ran}/≤{total}] {}  ({} · auth:{})", step.title, step.node_id, role);
        if let Some(n) = &step.cfg.note {
            println!("   📝 {n}");
        }

        if step.cfg.manual || step.cfg.request.is_none() {
            println!("   langkah manual/eksternal — kerjakan di luar, lalu [Enter]=lanjut  q=quit");
            match prompt(&mut lines)?.as_str() {
                "q" => return Ok(false),
                _ => ui.set_manual(cur)?,
            }
        } else {
            println!(
                "   {}  [Enter]=run  s=skip  vars=context  q=quit",
                step.cfg.request.as_deref().unwrap_or("")
            );
            match prompt(&mut lines)?.as_str() {
                "q" => return Ok(false),
                "s" => ui.set_skip(cur)?,
                "vars" => {
                    for (k, v) in &ctx.vars {
                        println!("   {k} = {v}");
                    }
                    ran -= 1;
                    continue 'walk; // tetap di langkah yang sama
                }
                _ => loop {
                    let rep = run_step(step, ctx, client);
                    print_report(&rep);
                    match rep.outcome {
                        Outcome::Passed => {
                            ui.set_ok(cur)?;
                            break;
                        }
                        Outcome::Skipped(_) => {
                            ui.set_skip(cur)?;
                            break;
                        }
                        Outcome::Manual => {
                            ui.set_manual(cur)?;
                            break;
                        }
                        Outcome::Failed(_) => {
                            ok_all = false;
                            ui.set_fail(cur)?;
                            println!("   r=retry  s=skip  q=quit");
                            match prompt(&mut lines)?.as_str() {
                                "r" => continue,
                                "s" => {
                                    ui.set_skip(cur)?;
                                    break;
                                }
                                _ => return Ok(false),
                            }
                        }
                    }
                },
            }
        }

        // Tentukan langkah berikutnya (kondisi dievaluasi SETELAH step jalan).
        match choose_next(&flow.outgoing(cur), &ctx.vars)? {
            Next::Advance(nx) => cur = nx,
            Next::End => break,
            Next::Pick(opts) => {
                println!("   🔀 pilih cabang:");
                for (k, (t, label)) in opts.iter().enumerate() {
                    println!("     {}. {} — {label}", k + 1, flow.steps[*t].title);
                }
                loop {
                    let ans = prompt(&mut lines)?;
                    if ans == "q" {
                        return Ok(false);
                    }
                    if let Ok(k) = ans.parse::<usize>() {
                        if k >= 1 && k <= opts.len() {
                            cur = opts[k - 1].0;
                            break;
                        }
                    }
                    println!("   masukkan 1..{} atau q", opts.len());
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
