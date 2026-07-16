//! Engine runner — terpisah penuh dari visual. Memegang context vars,
//! templating `{{var}}`, eksekusi HTTP (reqwest blocking), capture dot-path,
//! dan assertion. UI (interactive/auto) hanya mengonsumsi `StepReport`.

use std::collections::BTreeMap;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::config::{yaml_to_var_string, EnvConfig, FlowConfig};
use crate::flow::FlowStep;

pub struct Ctx {
    pub base_url: String,
    /// profil auth → bearer token.
    pub tokens: BTreeMap<String, String>,
    pub vars: BTreeMap<String, String>,
}

impl Ctx {
    /// Susun context: vars flow (default) di-overlay vars env, lalu --var CLI.
    pub fn build(flow_cfg: &FlowConfig, env: EnvConfig, cli_vars: &[(String, String)]) -> Self {
        let mut vars = BTreeMap::new();
        for (k, v) in &flow_cfg.vars {
            vars.insert(k.clone(), yaml_to_var_string(v));
        }
        for (k, v) in &env.vars {
            vars.insert(k.clone(), yaml_to_var_string(v));
        }
        for (k, v) in cli_vars {
            vars.insert(k.clone(), v.clone());
        }
        Ctx { base_url: env.base_url.trim_end_matches('/').to_string(), tokens: env.tokens, vars }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Passed,
    Failed(String),
    Skipped(String),
    /// Langkah manual/eksternal (atau node tanpa config) — tidak dieksekusi.
    Manual,
}

#[derive(Debug)]
pub struct StepReport {
    pub outcome: Outcome,
    pub http_status: Option<u16>,
    pub ms: u128,
    pub body: Option<Value>,
    /// Catatan capture/assert untuk ditampilkan UI.
    pub notes: Vec<String>,
}

impl StepReport {
    fn skipped(reason: String) -> Self {
        StepReport { outcome: Outcome::Skipped(reason), http_status: None, ms: 0, body: None, notes: vec![] }
    }
    fn manual() -> Self {
        StepReport { outcome: Outcome::Manual, http_status: None, ms: 0, body: None, notes: vec![] }
    }
    fn failed(msg: String) -> Self {
        StepReport { outcome: Outcome::Failed(msg), http_status: None, ms: 0, body: None, notes: vec![] }
    }
}

/// Ganti semua `{{var}}`; error menyebut var yang belum terisi.
pub fn template(input: &str, vars: &BTreeMap<String, String>) -> Result<String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    let mut missing: Vec<String> = Vec::new();
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let key = after[..end].trim();
        match vars.get(key) {
            Some(v) => out.push_str(v),
            None => missing.push(key.to_string()),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    if !missing.is_empty() {
        bail!("var belum terisi: {}", missing.join(", "));
    }
    Ok(out)
}

/// Template semua string di dalam JSON value (body request).
fn template_json(v: &Value, vars: &BTreeMap<String, String>) -> Result<Value> {
    Ok(match v {
        Value::String(s) => Value::String(template(s, vars)?),
        Value::Array(a) => Value::Array(a.iter().map(|x| template_json(x, vars)).collect::<Result<_>>()?),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| Ok((k.clone(), template_json(x, vars)?)))
                .collect::<Result<_>>()?,
        ),
        other => other.clone(),
    })
}

/// Ambil nilai via dot-path (`data.order_lists.0.id`). Index numerik = array.
pub fn dot_get<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    for part in path.split('.') {
        cur = match cur {
            Value::Object(o) => o.get(part)?,
            Value::Array(a) => a.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// Nilai JSON → string var (string tanpa kutip; lainnya serialisasi kompak).
fn value_to_var(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Evaluasi `skip_if`: `var == nilai` / `var != nilai`. Var yang tak ada = "".
pub fn eval_skip_if(expr: &str, vars: &BTreeMap<String, String>) -> Result<bool> {
    let (op, idx) = if let Some(i) = expr.find("!=") {
        ("!=", i)
    } else if let Some(i) = expr.find("==") {
        ("==", i)
    } else {
        bail!("skip_if hanya mendukung `==` / `!=`: {expr}");
    };
    let left = expr[..idx].trim();
    let right = expr[idx + 2..].trim().trim_matches('"').trim_matches('\'');
    let val = vars.get(left).map(String::as_str).unwrap_or("");
    Ok(match op {
        "!=" => val != right,
        _ => val == right,
    })
}

/// Hasil pemilihan edge berikutnya pada graf bercabang.
#[derive(Debug, PartialEq)]
pub enum Next {
    /// Lanjut ke langkah indeks ini.
    Advance(usize),
    /// Tidak ada outgoing edge — jalur selesai.
    End,
    /// Ambigu (0 atau >1 kandidat) — mode interaktif bertanya ke user;
    /// mode auto harus gagal deterministik. (target, label-tampilan).
    Pick(Vec<(usize, String)>),
}

/// Pilih edge berikutnya dari daftar outgoing `edges` (lihat `Flow::outgoing`)
/// berdasar context vars. Aturan: kondisi (`var == nilai`/`!=`) dievaluasi;
/// tepat SATU true → ikuti. Nol true → fallback (`else`/tanpa label) bila
/// tepat satu. Selain itu → `Pick` (ambigu).
pub fn choose_next(edges: &[&crate::flow::FlowEdge], vars: &BTreeMap<String, String>) -> Result<Next> {
    if edges.is_empty() {
        return Ok(Next::End);
    }
    if edges.len() == 1 && edges[0].cond.is_none() {
        return Ok(Next::Advance(edges[0].to));
    }
    let mut matched: Vec<usize> = Vec::new();
    let mut fallbacks: Vec<usize> = Vec::new();
    for e in edges {
        match &e.cond {
            Some(expr) => {
                if eval_skip_if(expr, vars)? {
                    matched.push(e.to);
                }
            }
            None => fallbacks.push(e.to),
        }
    }
    match (matched.as_slice(), fallbacks.as_slice()) {
        ([one], _) => Ok(Next::Advance(*one)),
        ([], [one]) => Ok(Next::Advance(*one)),
        _ => Ok(Next::Pick(
            edges
                .iter()
                .map(|e| (e.to, e.label.clone().unwrap_or_else(|| "(tanpa syarat)".into())))
                .collect(),
        )),
    }
}

/// Cocokkan pola status: `2xx` (kelas) atau angka persis (`200`).
fn status_matches(pattern: &str, status: u16) -> bool {
    let p = pattern.trim();
    if let Some(class) = p.strip_suffix("xx") {
        return class.parse::<u16>().map(|c| status / 100 == c).unwrap_or(false);
    }
    p.parse::<u16>().map(|s| s == status).unwrap_or(false)
}

/// Jalankan satu langkah. Mutasi `ctx.vars` hanya lewat `capture`.
pub fn run_step(step: &FlowStep, ctx: &mut Ctx, client: &reqwest::blocking::Client) -> StepReport {
    let cfg = &step.cfg;
    if cfg.manual || cfg.request.is_none() {
        return StepReport::manual();
    }
    if let Some(expr) = &cfg.skip_if {
        match eval_skip_if(expr, &ctx.vars) {
            Ok(true) => return StepReport::skipped(format!("skip_if: {expr}")),
            Ok(false) => {}
            Err(e) => return StepReport::failed(e.to_string()),
        }
    }

    match execute(step, ctx, client) {
        Ok(rep) => rep,
        Err(e) => StepReport::failed(format!("{e:#}")),
    }
}

fn execute(step: &FlowStep, ctx: &mut Ctx, client: &reqwest::blocking::Client) -> Result<StepReport> {
    let cfg = &step.cfg;
    let request = cfg.request.as_deref().expect("dicek pemanggil");
    let (method, path) = request
        .split_once(' ')
        .with_context(|| format!("request harus `METHOD /path`: {request}"))?;
    let method: reqwest::Method = method.trim().parse().context("HTTP method tidak dikenal")?;
    let url = format!("{}{}", ctx.base_url, template(path.trim(), &ctx.vars)?);

    let mut req = client.request(method, &url);
    if let Some(profile) = &cfg.auth {
        let token = ctx
            .tokens
            .get(profile)
            .with_context(|| format!("token profil auth '{profile}' tidak ada di env file"))?;
        req = req.bearer_auth(token);
    }
    for (k, v) in &cfg.headers {
        req = req.header(k, template(v, &ctx.vars)?);
    }
    if let Some(body) = &cfg.body {
        let json: Value = serde_json::to_value(body).context("body YAML → JSON")?;
        req = req.json(&template_json(&json, &ctx.vars)?);
    }

    let t0 = Instant::now();
    let resp = req.send().context("kirim request")?;
    let http_status = resp.status().as_u16();
    let ms = t0.elapsed().as_millis();
    let text = resp.text().unwrap_or_default();
    let body: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));

    let mut notes = Vec::new();

    // Capture dulu (gaya Postman), lalu assert.
    for (var, path) in &cfg.capture {
        match dot_get(&body, path) {
            Some(v) => {
                let s = value_to_var(v);
                notes.push(format!("capture {var} = {s}"));
                ctx.vars.insert(var.clone(), s);
            }
            None => notes.push(format!("capture {var}: path `{path}` tidak ada di response")),
        }
    }

    let mut failures: Vec<String> = Vec::new();
    let mut status_asserted = false;
    for (key, expected) in &cfg.assert {
        let expected = template(&yaml_to_var_string(expected), &ctx.vars)?;
        if key == "status" {
            status_asserted = true;
            if !status_matches(&expected, http_status) {
                failures.push(format!("status {http_status} ≠ {expected}"));
            }
            continue;
        }
        match dot_get(&body, key) {
            Some(actual) if value_to_var(actual) == expected => {}
            Some(actual) => failures.push(format!("{key} = {} ≠ {expected}", value_to_var(actual))),
            None => failures.push(format!("{key}: tidak ada di response (harap {expected})")),
        }
    }
    // Tanpa assert status eksplisit → wajib 2xx (default aman untuk mode test).
    if !status_asserted && !(200..300).contains(&http_status) {
        failures.push(format!("HTTP {http_status} (bukan 2xx)"));
    }

    let outcome = if failures.is_empty() {
        Outcome::Passed
    } else {
        Outcome::Failed(failures.join("; "))
    };
    Ok(StepReport { outcome, http_status: Some(http_status), ms, body: Some(body), notes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn template_replaces_and_reports_missing() {
        let v = vars(&[("a", "1"), ("b", "x")]);
        assert_eq!(template("/o/{{a}}/i/{{b}}", &v).unwrap(), "/o/1/i/x");
        let err = template("/o/{{zzz}}", &v).unwrap_err().to_string();
        assert!(err.contains("zzz"));
    }

    #[test]
    fn dot_get_walks_objects_and_arrays() {
        let j: Value = serde_json::from_str(r#"{"data":{"order_lists":[{"id":"OL1"}],"n":5}}"#).unwrap();
        assert_eq!(dot_get(&j, "data.order_lists.0.id").unwrap(), "OL1");
        assert_eq!(dot_get(&j, "data.n").unwrap(), 5);
        assert!(dot_get(&j, "data.zzz").is_none());
    }

    #[test]
    fn skip_if_eq_and_neq() {
        let v = vars(&[("pay_mode", "digital")]);
        assert!(eval_skip_if("pay_mode != cod", &v).unwrap());
        assert!(!eval_skip_if("pay_mode == cod", &v).unwrap());
        assert!(eval_skip_if("tidak_ada == ", &v).unwrap()); // var kosong == ""
    }

    #[test]
    fn status_patterns() {
        assert!(status_matches("2xx", 201));
        assert!(!status_matches("2xx", 404));
        assert!(status_matches("409", 409));
    }

    #[test]
    fn choose_next_rules() {
        use crate::flow::FlowEdge;
        let e = |to: usize, cond: Option<&str>| FlowEdge {
            from: 0,
            to,
            cond: cond.map(str::to_string),
            label: cond.map(str::to_string),
        };
        let v = vars(&[("pay_mode", "cod")]);
        // Tanpa edge → End.
        assert_eq!(choose_next(&[], &v).unwrap(), Next::End);
        // Satu edge tanpa syarat → Advance.
        let a = e(1, None);
        assert_eq!(choose_next(&[&a], &v).unwrap(), Next::Advance(1));
        // Kondisi cocok menang atas fallback.
        let c1 = e(2, Some("pay_mode == cod"));
        let c2 = e(3, None);
        assert_eq!(choose_next(&[&c1, &c2], &v).unwrap(), Next::Advance(2));
        // Tak ada yang cocok → fallback tunggal.
        let c3 = e(4, Some("pay_mode == digital"));
        assert_eq!(choose_next(&[&c3, &c2], &v).unwrap(), Next::Advance(3));
        // Tak ada cocok & tak ada fallback → Pick (ambigu).
        let c4 = e(5, Some("pay_mode == transfer"));
        assert!(matches!(choose_next(&[&c3, &c4], &v).unwrap(), Next::Pick(_)));
    }

    /// Server HTTP mock 1-request (std, tanpa dependency test tambahan).
    fn mock_server(response_json: &'static str, status_line: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_json}",
                    response_json.len()
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    fn step(request: &str, cfg_extra: impl FnOnce(&mut crate::config::StepConfig)) -> FlowStep {
        let mut cfg = crate::config::StepConfig { request: Some(request.into()), ..Default::default() };
        cfg_extra(&mut cfg);
        FlowStep { node_id: "t".into(), title: "t".into(), cfg, unconfigured: false }
    }

    #[test]
    fn run_step_captures_and_asserts() {
        let base = mock_server(r#"{"success":true,"data":{"id":"ORD1","status":"pending"}}"#, "201 Created");
        let mut ctx = Ctx { base_url: base, tokens: BTreeMap::new(), vars: BTreeMap::new() };
        let s = step("POST /api/v1/customer/orders", |c| {
            c.capture.insert("order_id".into(), "data.id".into());
            c.assert.insert("status".into(), serde_yaml::Value::String("2xx".into()));
            c.assert.insert("data.status".into(), serde_yaml::Value::String("pending".into()));
        });
        let rep = run_step(&s, &mut ctx, &reqwest::blocking::Client::new());
        assert_eq!(rep.outcome, Outcome::Passed, "notes={:?}", rep.notes);
        assert_eq!(ctx.vars["order_id"], "ORD1");
    }

    #[test]
    fn run_step_fails_on_wrong_status_value() {
        let base = mock_server(r#"{"data":{"status":"accepted"}}"#, "200 OK");
        let mut ctx = Ctx { base_url: base, tokens: BTreeMap::new(), vars: BTreeMap::new() };
        let s = step("GET /x", |c| {
            c.assert.insert("data.status".into(), serde_yaml::Value::String("pending".into()));
        });
        let rep = run_step(&s, &mut ctx, &reqwest::blocking::Client::new());
        match rep.outcome {
            Outcome::Failed(msg) => assert!(msg.contains("accepted"), "{msg}"),
            other => panic!("harap Failed, dapat {other:?}"),
        }
    }

    #[test]
    fn run_step_default_requires_2xx() {
        let base = mock_server(r#"{"success":false}"#, "502 Bad Gateway");
        let mut ctx = Ctx { base_url: base, tokens: BTreeMap::new(), vars: BTreeMap::new() };
        let s = step("GET /x", |_| {});
        let rep = run_step(&s, &mut ctx, &reqwest::blocking::Client::new());
        assert!(matches!(rep.outcome, Outcome::Failed(_)));
        assert_eq!(rep.http_status, Some(502));
    }
}
