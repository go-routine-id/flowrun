# flowrun

Flow-runner dinamis: **visual mermaid** (dirender [flowmaid](https://crates.io/crates/flowmaid)) + **engine runner HTTP terpisah**. Definisikan alur API multi-langkah sebagai graf, lalu jalankan *next-next* secara interaktif — atau `--auto` sebagai smoke-test/CI (exit code ≠ 0 saat gagal).

Satu flow = dua file (visual & eksekusi sengaja dipisah):

| File | Isi |
|---|---|
| `flow.mmd` | Graf **mermaid murni** — bisa dirender di mana pun (GitHub, editor, flowmaid) |
| `flow.yaml` | Config runner per **node-id**: method, url, body, auth, capture, assert |

Plus satu **env file** per-deployment (gitignored): `base_url`, token per profil auth, seed vars.

## Quickstart

```sh
cargo build --release

# contoh bawaan: alur order WACCA (customer ↔ owner) sampai selesai
cd examples/wacca-order
cp env.sample.yaml dev.yaml   # isi host + token + uuid seed

# interaktif (next-next) + SVG status live + preview browser
flowrun run -f flow.mmd -c flow.yaml -e dev.yaml --svg status.svg --serve 127.0.0.1:8787

# mode test / CI: jalankan semua, stop-on-fail, exit code ≠ 0 bila gagal
flowrun run -f flow.mmd -c flow.yaml -e dev.yaml --auto

# render diagram saja
flowrun render -f flow.mmd -o flow.svg
```

Mode interaktif: `Enter`=run · `s`=skip · `r`=retry (setelah gagal) · `vars`=lihat context · `q`=quit. Node di SVG berubah warna: 🟡 current · 🟢 ok · 🔴 fail · abu = skip · ungu = manual.

## Desktop (egui) — opsional

Jendela native (bukan browser): canvas graf digambar dari **flowmaid `scene` API** (tata-letak Sugiyama), egui hanya render + interaktivitas. Tombol Next/Auto/Skip/Reset, klik node → panel response.

```sh
cargo run --features gui --bin flowrun-gui -- \
  -f examples/wacca-order/flow.mmd -c examples/wacca-order/flow.yaml -e examples/wacca-order/dev.yaml
```

Pembagian peran: **flowmaid** = layout & bentuk graf · **egui** = render + kontrol · **engine flowrun** = eksekusi HTTP (worker thread, UI tetap responsif). Feature `gui` opsional → CLI inti tetap ringan tanpa dependensi GUI.

## Skema `flow.yaml`

```yaml
auth_profiles: [customer, owner]   # token diambil dari env file
vars: { pay_mode: cod }            # default vars (dioverride env / --var)

steps:
  n01:                             # ← node-id di flow.mmd
    auth: customer                 # profil auth → Authorization: Bearer <token>
    request: POST /api/v1/customer/orders
    headers: { X-Contoh: "{{var}}" }
    body:                          # YAML → JSON; string mendukung {{var}}
      outlet_id: "{{outlet_id}}"
    capture:                       # response JSON → context vars (dot-path)
      order_id: data.id
      order_list_id: data.order_lists.0.id
    assert:                        # gagal → langkah merah (+ exit ≠ 0 di --auto)
      status: 2xx                  # kelas (2xx) atau persis (409)
      "data.status": pending       # equality dot-path
    skip_if: "pay_mode != cod"     # ekspresi sederhana ==/!=
    note: catatan bebas untuk operator
  n99:
    manual: true                   # langkah eksternal (mis. bayar di gateway)
```

Node di `.mmd` **tanpa** entry di `flow.yaml` otomatis dianggap `manual` (visual-only). Vars mengalir antar langkah via `capture` → dipakai `{{var}}` di url/body/header/assert langkah berikutnya.

## Env file (`dev.yaml` — jangan di-commit)

```yaml
base_url: "https://host-dev-kamu"
tokens: { customer: "eyJ...", owner: "eyJ..." }
vars: { outlet_id: "uuid", item_id: "uuid" }
```

## Batasan v0.1

- Jalur **linear** (tiap node satu outgoing edge). Branching/edge kondisional → v0.2.
- `skip_if` hanya `==` / `!=`; capture hanya dot-path (tanpa filter).
- Report junit/json untuk CI → v0.2.

## Lisensi

GPL-3.0-or-later (mengikuti dependensi [flowmaid](https://github.com/go-routine-id/flowmaid)).
