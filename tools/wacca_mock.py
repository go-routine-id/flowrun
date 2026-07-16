#!/usr/bin/env python3
"""Mock wacca-service untuk demo flowrun — response envelope & shape persis
kontrak backend (success/data), path sesuai examples/wacca-order/flow.yaml."""
import json, re
from http.server import BaseHTTPRequestHandler, HTTPServer

ORDER = "0d3m0000-1111-7000-8000-00000000demo"
OL = "0d3m0000-2222-7000-8000-0000000000ol"

ROUTES = [
    ("GET", r"^/api/v1/payment-methods$",
     {"data": [{"id": "pm-cod", "name": "COD", "code": "cod", "is_active": True}]}),
    ("POST", r"^/api/v1/customer/orders$",
     {"data": {"id": ORDER, "status": "pending", "order_number": "WCC-DEMO-001"}}),
    ("GET", r"^/api/v1/tenant/orders$", {"data": {"items": [{"id": ORDER}]}}),
    ("GET", rf"^/api/v1/tenant/orders/{ORDER}$",
     {"data": {"id": ORDER, "status": "pending", "order_lists": [{"id": OL, "name": "Cuci Reguler"}]}}),
    ("POST", rf"^/api/v1/tenant/orders/{ORDER}/accept$", {"data": {"id": ORDER, "status": "accepted"}}),
    ("POST", rf"^/api/v1/tenant/orders/{ORDER}/reject$", {"data": {"id": ORDER, "status": "cancelled"}}),
    ("PATCH", rf"^/api/v1/tenant/order-list/{OL}/set-final-quantity$",
     {"data": {"id": OL, "quantity": 3.5, "actual_pcs": 6}}),
    ("POST", rf"^/api/v1/tenant/orders/{ORDER}/final-price$",
     {"data": {"id": ORDER, "status": "price_proposed", "final_total": 50000, "tax_amount": 5500}}),
    ("GET", rf"^/api/v1/customer/orders/{ORDER}$",
     {"data": {"id": ORDER, "status": "price_proposed", "final_total": 50000, "total_payable": 55500}}),
    ("PATCH", rf"^/api/v1/customer/orders/{ORDER}/add-payment-method$",
     {"data": {"id": ORDER, "payment_charge_status": None, "payment": {"grand_total": 55500}}}),
    ("POST", rf"^/api/v1/customer/orders/{ORDER}/approve-price$",
     {"data": {"id": ORDER, "status": "processing"}}),
    ("POST", rf"^/api/v1/tenant/orders/{ORDER}/confirm-cod$",
     {"data": {"id": ORDER, "status": "processing", "total_payable": 55500}}),
    ("PATCH", rf"^/api/v1/tenant/order-list/{OL}/status-finished$",
     {"data": {"id": OL, "status": "finished"}}),
    ("POST", rf"^/api/v1/customer/orders/{ORDER}/confirm-receipt$",
     {"data": {"id": ORDER, "status": "completed"}}),
    ("GET", rf"^/api/v1/customer/orders/{ORDER}/timeline$",
     {"data": {"steps": [{"key": "created", "status": "done"}, {"key": "completed", "status": "done"}]}}),
]

VALID_TOKENS = {"demo-customer-token", "demo-owner-token"}
import os
BUGGY = os.environ.get("BUGGY") == "1"  # simulasi regresi backend

class H(BaseHTTPRequestHandler):
    def _handle(self, method):
        tok = self.headers.get("Authorization", "").removeprefix("Bearer ").strip()
        if tok not in VALID_TOKENS:
            return self._send(401, {"success": False, "message": "invalid token"})
        for m, pat, body in ROUTES:
            if m == method and re.match(pat, self.path):
                out = {"success": True, "message": "success"}
                out.update(json.loads(json.dumps(body)))
                # BUGGY: approve-price "lupa" transisi status (regresi disimulasikan)
                if BUGGY and self.path.endswith("/approve-price"):
                    out["data"]["status"] = "price_proposed"
                return self._send(200 if method != "POST" or "orders$" not in pat else 201, out)
        return self._send(404, {"success": False, "message": f"no route {method} {self.path}"})

    def _send(self, code, obj):
        raw = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def log_message(self, fmt, *a):
        print(f"  mock: {self.command} {self.path} -> ok")

    do_GET = lambda s: s._handle("GET")
    do_POST = lambda s: s._handle("POST")
    do_PATCH = lambda s: s._handle("PATCH")

if __name__ == "__main__":
    import sys
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 18923
    print(f"mock wacca di http://127.0.0.1:{port}")
    HTTPServer(("127.0.0.1", port), H).serve_forever()
