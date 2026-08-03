//! Static dashboard: one self-contained HTML page that calls `/api/summary`.

pub const HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>Smartflow Halo — Savings</title>
<style>
  :root { --bg:#0b1020; --card:#131a2e; --border:#26304a; --ink:#e7ecf7; --muted:#93a0bd; --teal:#39d3bb; --green:#4ade80; }
  * { box-sizing: border-box; }
  body { margin:0; background:var(--bg); color:var(--ink); font:15px/1.5 -apple-system,Segoe UI,Roboto,sans-serif; }
  header { padding:28px 32px; border-bottom:1px solid var(--border); }
  h1 { margin:0; font-size:20px; letter-spacing:.2px; }
  .sub { color:var(--muted); font-size:13px; margin-top:4px; }
  main { max-width:1000px; margin:0 auto; padding:28px 32px; }
  .cards { display:grid; grid-template-columns:repeat(4,1fr); gap:16px; margin-bottom:28px; }
  .card { background:var(--card); border:1px solid var(--border); border-radius:14px; padding:18px; }
  .card .label { color:var(--muted); font-size:12px; text-transform:uppercase; letter-spacing:.6px; }
  .card .value { font-size:26px; font-weight:650; margin-top:8px; }
  .card.savings .value { color:var(--green); }
  table { width:100%; border-collapse:collapse; background:var(--card); border:1px solid var(--border); border-radius:14px; overflow:hidden; margin-bottom:24px; }
  th,td { text-align:left; padding:11px 16px; border-bottom:1px solid var(--border); font-variant-numeric:tabular-nums; }
  th { color:var(--muted); font-size:12px; text-transform:uppercase; letter-spacing:.5px; }
  tr:last-child td { border-bottom:none; }
  td.num { text-align:right; }
  .money { color:var(--teal); }
  select { background:var(--card); color:var(--ink); border:1px solid var(--border); border-radius:8px; padding:6px 10px; }
  h2 { font-size:14px; color:var(--muted); text-transform:uppercase; letter-spacing:.6px; margin:24px 0 10px; }
</style>
</head>
<body>
<header>
  <h1>Smartflow Halo — Savings</h1>
  <div class="sub">Metadata only. Prompts, responses, tool arguments and vectors never reach this relay.</div>
</header>
<main>
  <div style="margin-bottom:18px">
    Window:
    <select id="win" onchange="load()">
      <option value="24">Last 24 hours</option>
      <option value="168" selected>Last 7 days</option>
      <option value="720">Last 30 days</option>
      <option value="0">All time</option>
    </select>
  </div>
  <div class="cards">
    <div class="card savings"><div class="label">Estimated saved</div><div class="value" id="saved">—</div></div>
    <div class="card"><div class="label">Actual spend</div><div class="value money" id="spend">—</div></div>
    <div class="card"><div class="label">Requests</div><div class="value" id="reqs">—</div></div>
    <div class="card"><div class="label">Cache hit rate</div><div class="value" id="hit">—</div></div>
  </div>
  <h2>By agent</h2>
  <table><thead><tr><th>Agent</th><th class="num">Requests</th><th class="num">Hits</th><th class="num">Spend</th><th class="num">Saved</th></tr></thead><tbody id="agents"></tbody></table>
  <h2>By model</h2>
  <table><thead><tr><th>Model</th><th class="num">Requests</th><th class="num">Spend</th><th class="num">Saved</th></tr></thead><tbody id="models"></tbody></table>
</main>
<script>
const usd = n => '$' + (n||0).toFixed(4);
async function load() {
  const hours = document.getElementById('win').value;
  const r = await fetch('/api/summary?hours=' + hours);
  const d = await r.json();
  const t = d.total || {};
  document.getElementById('saved').textContent = usd(t.savings);
  document.getElementById('spend').textContent = usd(t.actual_cost);
  document.getElementById('reqs').textContent = (t.requests||0).toLocaleString();
  const hit = t.requests ? (100*t.cache_hits/t.requests) : 0;
  document.getElementById('hit').textContent = hit.toFixed(1) + '%';
  const ab = document.getElementById('agents'); ab.innerHTML='';
  (d.by_agent||[]).forEach(a => {
    ab.innerHTML += `<tr><td>${a.name}</td><td class="num">${a.requests}</td><td class="num">${a.cache_hits}</td><td class="num money">${usd(a.actual_cost)}</td><td class="num">${usd(a.savings)}</td></tr>`;
  });
  const mb = document.getElementById('models'); mb.innerHTML='';
  (d.by_model||[]).filter(m=>m.name).forEach(m => {
    mb.innerHTML += `<tr><td>${m.name}</td><td class="num">${m.requests}</td><td class="num money">${usd(m.actual_cost)}</td><td class="num">${usd(m.savings)}</td></tr>`;
  });
}
load();
</script>
</body>
</html>"#;
