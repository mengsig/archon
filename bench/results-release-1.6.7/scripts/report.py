"""Render summary.json as the self-contained report.html.

Usage: python3 report.py <results dir> <make ci line>
"""
import html
import json
import math
import sys
from pathlib import Path

TIERS = {'haiku': 'Haiku · claude-code/claude-haiku-5-5', 'luna': 'Luna · codex/gpt-6-luna'}
ARMS = {'archon': 'Archon', 'aether': 'Direct Aether'}

STYLE = """
body{font-family:system-ui,-apple-system,Segoe UI,sans-serif;font-size:20px;line-height:1.5;margin:0;
background:#f6f7f9;color:#1b1f24}
main{max-width:1200px;margin:0 auto;padding:24px 32px 64px}
.banner{padding:28px 32px;border-radius:14px;color:#fff;margin:24px 0}
.banner h1{margin:0;font-size:44px}.banner p{margin:8px 0 0;font-size:22px}
.ok{background:#1f7a3f}.bad{background:#b3261e}
h2{font-size:32px;margin:40px 0 12px}h3{font-size:24px;margin:28px 0 8px}
table{border-collapse:collapse;width:100%;background:#fff;border-radius:10px;overflow:hidden;margin:8px 0 16px}
th,td{padding:10px 14px;text-align:right;border-bottom:1px solid #e3e6ea}
th:first-child,td:first-child{text-align:left}th{background:#eef1f4;font-weight:600}
.cards{display:grid;grid-template-columns:repeat(3,1fr);gap:16px}
.card{background:#fff;border-radius:12px;padding:18px 20px}
.card .label{color:#5a636e;font-size:17px}.card .value{font-size:30px;font-weight:700}
.card .sub{color:#5a636e;font-size:17px}
.down{color:#b3261e;font-weight:600}.up{color:#1f7a3f;font-weight:600}
.note{background:#fff;border-left:6px solid #8a94a0;padding:12px 18px;border-radius:6px}
"""


def num(value, pattern, fallback='n/a'):
    if value is None or (isinstance(value, float) and not math.isfinite(value)):
        return fallback
    return pattern.format(value)


def table(headers, rows):
    assert headers and all(len(row) == len(headers) for row in rows), 'ragged table'
    head = ''.join(f'<th>{html.escape(h)}</th>' for h in headers)
    body = ''.join('<tr>' + ''.join(f'<td>{cell}</td>' for cell in row) + '</tr>' for row in rows)
    return f'<table><thead><tr>{head}</tr></thead><tbody>{body}</tbody></table>'


def card(label, old, new, sub):
    return (f'<div class="card"><div class="label">{html.escape(label)}</div>'
            f'<div class="value">{old} → {new}</div><div class="sub">{sub}</div></div>')


def tier_cards(comparison):
    assert 'old' in comparison and 'new' in comparison
    o, n = comparison['old'], comparison['new']
    diff = comparison['diff']
    mc = comparison['mcnemar']
    cr, wr = comparison['cost_ratio'], comparison['wall_ratio']
    return '<div class="cards">' + ''.join([
        card('Archon pass rate', f"{o['pass']}/{o['graded']}", f"{n['pass']}/{n['graded']}",
             f"{num(diff[0] * 100 if diff[0] is not None else None, '{:+.1f}')} pp "
             f"[{num(diff[1] * 100 if diff[1] is not None else None, '{:+.1f}')}, "
             f"{num(diff[2] * 100 if diff[2] is not None else None, '{:+.1f}')}], McNemar p={num(mc['p'], '{:.2f}')}"),
        card('Archon cost / trial', num(o['cost_mean'], '${:.4f}'), num(n['cost_mean'], '${:.4f}'),
             f"ratio {num(cr[0], '{:.2f}')}× [{num(cr[1], '{:.2f}')}, {num(cr[2], '{:.2f}')}], p={num(comparison['cost_p'], '{:.2f}')}"),
        card('Archon minutes / trial', num(o['wall_mean'] / 60, '{:.1f}'), num(n['wall_mean'] / 60, '{:.1f}'),
             f"ratio {num(wr[0], '{:.2f}')}× [{num(wr[1], '{:.2f}')}, {num(wr[2], '{:.2f}')}], p={num(comparison['wall_p'], '{:.2f}')}"),
    ]) + '</div>'


def arm_rows(entry):
    rows = []
    for arm in ARMS:
        c = entry[arm]
        for version in ('old', 'new'):
            s = c[version]
            rows.append([f"{version.upper()} {ARMS[arm]}", f"{s['pass']}/{s['graded']}",
                         num(s['rate'] * 100, '{:.1f}%'), num(s['cost_mean'], '${:.4f}'),
                         num(s['cost_total'], '${:.2f}'), num(s['wall_mean'] / 60, '{:.1f}'),
                         str(s['timeouts']), str(s['infra'])])
    assert len(rows) == 4
    return rows


def task_rows(tasks):
    assert tasks, 'no per-task rows'
    rows = []
    for t in tasks:
        o, n = t['old'], t['new']
        flip = n['pass'] - o['pass']
        mark = f'<span class="down">{flip:+d}</span>' if flip < 0 else (f'<span class="up">{flip:+d}</span>' if flip > 0 else '0')
        rows.append([html.escape(t['task']), f"{o['pass']}/{o['graded']}", f"{n['pass']}/{n['graded']}", mark,
                     num(o['cost_mean'], '${:.4f}'), num(n['cost_mean'], '${:.4f}'),
                     num(o['wall_mean'] / 60 if o['wall'] else None, '{:.1f}'),
                     num(n['wall_mean'] / 60 if n['wall'] else None, '{:.1f}')])
    return rows


def flip_list(flips):
    if not flips:
        return '<p>No task changed between pass and fail on a paired trial.</p>'
    items = []
    for f in flips:
        p = f['pairs']
        items.append(f"<li><b>{html.escape(f['task'])}</b>: {f['old']} → {f['new']} "
                     f"({p['old_only']} pass→fail, {p['new_only']} fail→pass)</li>")
    return '<ul>' + ''.join(items) + '</ul>'


def timeout_causes(walltime, model, version):
    """Map (task, trial) of each timed-out Archon trial to its cause from walltime.json."""
    assert version in ('1.6.6', '1.6.7')
    entries = walltime.get(model, {}).get('timeouts', [])
    assert all({'version', 'task', 'arm', 'trial', 'cause'} <= set(e) for e in entries), 'malformed walltime.json'
    return {(e['task'], e['trial']): e['cause'] for e in entries if e['version'] == version and e['arm'] == 'archon'}


def parity_section(model, data, walltime):
    """Archon against direct Aether on the NEW run, with each shortfall's failure reason."""
    p = data['parity']['new']
    assert p['verdict'] in ('PARITY MET', 'PARITY MISSED') and p['tasks']
    old = data['parity']['old']
    causes = timeout_causes(walltime, model, '1.6.7')
    rows = [[html.escape(t['task']), t['archon'], t['aether'],
             f'<span class="down">{t["delta"]:+d}</span>' if t['delta'] < 0 else
             (f'<span class="up">{t["delta"]:+d}</span>' if t['delta'] > 0 else '0')] for t in p['tasks']]
    items = []
    for short in p['shortfalls']:
        for f in short['failures']:
            reason = f['reason']
            if reason == 'timeout' and (short['task'], f['trial']) in causes:
                reason = f"timeout: {causes[(short['task'], f['trial'])]}"
            items.append(f"<li><b>{html.escape(short['task'])}</b> #{f['trial']} "
                         f"({short['archon']}/{short['aether']}, {f['wall'] / 60:.1f} min): {html.escape(reason)}</li>")
    shortfalls = '<ul>' + ''.join(items) + '</ul>' if items else '<p>Archon passed at least as many trials as Aether on every task.</p>'
    return (f'<h3>Accuracy parity, NEW: {p["verdict"]} (Archon {p["archon"]}/{p["graded"]}, '
            f'Aether {p["aether"]}/{p["graded"]}; OLD {old["archon"]} vs {old["aether"]})</h3>'
            + table(['Task', 'Archon pass', 'Aether pass', 'Archon − Aether'], rows)
            + '<p>Tasks where Archon passed fewer trials than Aether:</p>' + shortfalls)


def tier_section(model, data, walltime):
    entry = data['comparison']
    headers = ['Task', 'OLD pass', 'NEW pass', 'Δ', 'OLD $/trial', 'NEW $/trial', 'OLD min', 'NEW min']
    return (f'<h2>{html.escape(TIERS[model])}</h2>' + tier_cards(entry['archon'])
            + table(['Version / arm', 'Pass', 'Rate', '$ / trial', '$ total', 'Min / trial', 'Timeouts', 'Infra'],
                    arm_rows(entry))
            + '<h3>Archon per task</h3>' + table(headers, task_rows(entry['tasks_archon']))
            + '<h3>Archon pass/fail flips</h3>' + flip_list(entry['flips'])
            + '<h3>Direct Aether per task</h3>' + table(headers, task_rows(entry['tasks_aether']))
            + '<h3>Direct Aether pass/fail flips</h3>' + flip_list(entry['aether_flips'])
            + parity_section(model, data, walltime))


def render(summary, ci_line, walltime):
    assert summary['verdict'] in ('NO REGRESSION', 'REGRESSION')
    assert summary['parity_verdict'] in ('PARITY MET', 'PARITY MISSED')
    assert summary['models'], 'no tiers in the summary'
    good = summary['verdict'] == 'NO REGRESSION'
    reasons = ''.join(f'<p>{html.escape(r)}</p>' for r in summary['reasons']) or \
        '<p>No pass-rate drop, cost rise or slowdown beyond noise on either tier.</p>'
    body = (f'<div class="banner {"ok" if good else "bad"}"><h1>Archon 1.6.7: {summary["verdict"]}</h1>{reasons}'
            f'<p>Accuracy parity with direct Aether: {summary["parity_verdict"]} (reported; the regression rule against '
            f'1.6.6 decides the release).</p></div>'
            '<p>OLD: published Archon v1.6.6 with Aether v1.4.3. NEW: Archon 1.6.7 candidate with Aether v1.4.4. '
            'All 11 tasks, 3 trials, arms archon and aether, effort high.</p>'
            f'<h2>make ci</h2><p class="note">{html.escape(ci_line)}</p>'
            + ''.join(tier_section(model, data, walltime) for model, data in summary['models'].items()))
    return (f'<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" '
            f'content="width=device-width,initial-scale=1"><title>Archon 1.6.7 release bench</title>'
            f'<style>{STYLE}</style></head><body><main>{body}</main></body></html>\n')


def main():
    out_dir = Path(sys.argv[1])
    ci_line = sys.argv[2]
    assert ci_line, 'the make ci result is required'
    summary = json.loads((out_dir / 'summary.json').read_text())
    walltime_file = out_dir / 'walltime.json'
    walltime = json.loads(walltime_file.read_text()) if walltime_file.is_file() else {}
    (out_dir / 'report.html').write_text(render(summary, ci_line, walltime))


if __name__ == '__main__':
    main()
