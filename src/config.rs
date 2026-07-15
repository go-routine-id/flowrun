//! Schema file definisi: sidecar `flow.yaml` (config runner per node-id) dan
//! env file (`dev.yaml`, gitignored) berisi base_url + token + seed vars.
//! Visual (graf) hidup terpisah di `flow.mmd` — lihat `flow.rs`.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Sidecar runner untuk satu flow. Key `steps` = node-id di `flow.mmd`.
#[derive(Debug, Deserialize, Default)]
pub struct FlowConfig {
    /// Nama profil auth yang dipakai flow ini (token diambil dari env file).
    #[serde(default)]
    pub auth_profiles: Vec<String>,
    /// Vars default flow (bisa dioverride env file / --var).
    #[serde(default)]
    pub vars: BTreeMap<String, serde_yaml::Value>,
    #[serde(default)]
    pub steps: BTreeMap<String, StepConfig>,
}

/// Config eksekusi satu node. Node yang ada di `.mmd` tapi tak punya entry di
/// sini diperlakukan sebagai langkah manual (visual-only, di-pause/skip).
#[derive(Debug, Deserialize, Default, Clone)]
pub struct StepConfig {
    /// Nama profil auth (mis. `customer` / `owner`); token dari env file.
    pub auth: Option<String>,
    /// `"METHOD /path"` — path digabung ke base_url, mendukung `{{var}}`.
    pub request: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Body request (YAML → JSON), string di dalamnya mendukung `{{var}}`.
    pub body: Option<serde_yaml::Value>,
    /// `var: dot.path` — ambil nilai dari response JSON ke context vars.
    #[serde(default)]
    pub capture: BTreeMap<String, String>,
    /// `status: 2xx|200` dan/atau `dot.path: nilai-harapan` (equality).
    #[serde(default)]
    pub assert: BTreeMap<String, serde_yaml::Value>,
    /// Ekspresi skip sederhana: `var == nilai` / `var != nilai`.
    pub skip_if: Option<String>,
    /// Langkah eksternal (mis. bayar di gateway) — tak dieksekusi, hanya pause.
    #[serde(default)]
    pub manual: bool,
    /// Catatan bebas, ditampilkan di CLI.
    pub note: Option<String>,
}

/// Env file per-deployment (gitignored): base_url + token per profil + seed vars.
#[derive(Debug, Deserialize, Default)]
pub struct EnvConfig {
    pub base_url: String,
    /// profil auth → bearer token.
    #[serde(default)]
    pub tokens: BTreeMap<String, String>,
    #[serde(default)]
    pub vars: BTreeMap<String, serde_yaml::Value>,
}

pub fn load_flow_config(path: &Path) -> Result<FlowConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("baca sidecar config {}", path.display()))?;
    serde_yaml::from_str(&raw).with_context(|| format!("parse YAML {}", path.display()))
}

pub fn load_env_config(path: &Path) -> Result<EnvConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("baca env file {}", path.display()))?;
    serde_yaml::from_str(&raw).with_context(|| format!("parse YAML {}", path.display()))
}

/// Normalisasi nilai YAML (string/angka/bool) menjadi string var.
pub fn yaml_to_var_string(v: &serde_yaml::Value) -> String {
    match v {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Null => String::new(),
        other => serde_yaml::to_string(other).unwrap_or_default().trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sidecar_minimal() {
        let y = r#"
auth_profiles: [customer, owner]
vars: { pay_mode: cod, qty: 3 }
steps:
  n01:
    auth: customer
    request: POST /api/v1/customer/orders
    body: { outlet_id: "{{outlet_id}}", items: [{ item_id: "{{item_id}}", quantity: 3 }] }
    capture: { order_id: data.id }
    assert: { status: 2xx, "data.status": pending }
  n10:
    auth: owner
    request: POST /api/v1/tenant/orders/{{order_id}}/confirm-cod
    skip_if: "pay_mode != cod"
"#;
        let cfg: FlowConfig = serde_yaml::from_str(y).unwrap();
        assert_eq!(cfg.auth_profiles, vec!["customer", "owner"]);
        assert_eq!(yaml_to_var_string(&cfg.vars["qty"]), "3");
        let n01 = &cfg.steps["n01"];
        assert_eq!(n01.request.as_deref(), Some("POST /api/v1/customer/orders"));
        assert_eq!(n01.capture["order_id"], "data.id");
        assert_eq!(yaml_to_var_string(&n01.assert["status"]), "2xx");
        assert_eq!(cfg.steps["n10"].skip_if.as_deref(), Some("pay_mode != cod"));
    }

    #[test]
    fn parse_env() {
        let y = r#"
base_url: https://api.example.test
tokens: { customer: tokC, owner: tokO }
vars: { outlet_id: abc }
"#;
        let env: EnvConfig = serde_yaml::from_str(y).unwrap();
        assert_eq!(env.base_url, "https://api.example.test");
        assert_eq!(env.tokens["owner"], "tokO");
    }
}
