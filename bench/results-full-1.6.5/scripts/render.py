"""Render the numerical summary as a standalone benchmark report."""
import html
import json
from pathlib import Path
import sys


def pct(value):
    return f'{100 * value:.1f}%'


def ratio(values):
    return f'{values[0]:.2f}× [{values[1]:.2f}, {values[2]:.2f}]'


def table(headers, rows):
    assert headers and rows
    assert all(len(row) == len(headers) for row in rows)
    heading = ''.join(f'<th scope="col">{html.escape(str(cell))}</th>' for cell in headers)
    body = ''.join('<tr>' + ''.join(f'<td>{html.escape(str(cell))}</td>' for cell in row) + '</tr>' for row in rows)
    return f'<div class="table-wrap"><table><thead><tr>{heading}</tr></thead><tbody>{body}</tbody></table></div>'


def arms_rows(data):
    assert set(data['arms']) == {'old', 'new'}
    assert set(data['aether_arms']) == {'old', 'new'}
    rows = []
    for arm in ('old', 'new'):
        for label, values in [('Archon', data['arms'][arm]), ('Direct Aether', data['aether_arms'][arm])]:
            rows.append((arm.upper(), label, f"{values['pass']}/{values['graded']}",
                         pct(values['rate']), '–'.join(pct(x) for x in values['ci']),
                         f"{values['wall_mean']:.0f} s", f"${values['cost_mean']:.4f}",
                         f"{values['reviewers_per_item']:.2f}", values['infra']))
    return rows


def comparison_rows(comparison):
    assert 'archon' in comparison and 'aether' in comparison
    assert all('mcnemar' in comparison[name] for name in ('archon', 'aether'))
    rows = []
    for arm, label in [('archon', 'Archon'), ('aether', 'Direct Aether')]:
        c = comparison[arm]
        m = c['mcnemar']
        rows.append((label, f"{c['diff'][0] * 100:+.1f} pp [{c['diff'][1] * 100:+.1f}, {c['diff'][2] * 100:+.1f}]",
                     f"{m['old_only']}/{m['new_only']} of {m['pairs']} (p={m['p']:.3f})",
                     f"{ratio(c['wall_ratio'])} (p={c['wall_p']:.3f})",
                     f"{ratio(c['cost_ratio'])} (p={c['cost_p']:.3f})"))
    return rows


def task_rows(comparison):
    tasks = comparison['tasks_archon']
    assert tasks and len({task['task'] for task in tasks}) == len(tasks)
    return [(task['task'], f"{task['old']['pass']}/{task['old']['graded']}",
             f"{task['new']['pass']}/{task['new']['graded']}",
             f"{task['old']['wall_mean']:.0f} s", f"{task['new']['wall_mean']:.0f} s") for task in tasks]


def render_model(name, data):
    assert name and 'comparison' in data
    assert 'arms' in data and 'aether_arms' in data
    comparison = data['comparison']['archon']
    tiles = ''.join(f'<div class="tile"><small>{label}</small><strong>{html.escape(value)}</strong></div>' for label, value in [
        ('Pass difference', f"{comparison['diff'][0] * 100:+.1f} pp"),
        ('Wall ratio', ratio(comparison['wall_ratio'])),
        ('Cost ratio', ratio(comparison['cost_ratio'])),
        ('McNemar p', f"{comparison['mcnemar']['p']:.3f}")])
    arms = table(['Version', 'Arm', 'Passes', 'Pass rate', '95% interval', 'Mean wall', 'Mean cost', 'Reviewers/item', 'Infra'], arms_rows(data))
    comparisons = table(['Arm', 'Pass difference, 95% CI', 'Old-only / new-only', 'Wall ratio, 95% CI', 'Cost ratio, 95% CI'], comparison_rows(data['comparison']))
    tasks = table(['Task', 'Old pass', 'New pass', 'Old wall', 'New wall'], task_rows(data['comparison']))
    return f'<section><h2>{html.escape(name)}</h2><div class="tiles">{tiles}</div>{arms}<h3>New versus old</h3>{comparisons}<h3>Tasks: Archon</h3>{tasks}</section>'


def render(summary):
    assert summary['verdict'] in {'REGRESSION', 'NO REGRESSION'}
    assert summary['models']
    reasons = summary['reasons'] or ['No measured difference met the predeclared significance criteria.']
    details = ''.join(f'<li>{html.escape(reason)}</li>' for reason in reasons)
    models = ''.join(render_model(name.title(), data) for name, data in summary['models'].items())
    return f'''<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>Archon 1.6.5 benchmark</title><style>
:root{{font-family:system-ui,sans-serif;color:#20231f;background:#f5f5f3}}body{{margin:0}}main{{max-width:1440px;margin:auto;padding:clamp(1rem,3vw,3rem)}}h1{{font-size:clamp(2rem,5vw,4rem);margin:.3rem 0}}h2{{margin-top:2.5rem}}h3{{margin-top:2rem}}.banner{{background:#1c1e1b;color:white;padding:2rem;border-radius:1rem}}.status{{display:inline-block;background:#285dce;color:white;padding:.5rem .8rem;border-radius:.5rem;font-weight:800}}.lead{{max-width:70ch;line-height:1.5}}section{{background:white;margin-top:1.5rem;padding:1.5rem;border-radius:1rem;min-width:0}}.tiles{{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(100%,180px),1fr));gap:.75rem;margin:1rem 0}}.tile{{background:#eeeeec;padding:1rem;border-radius:.7rem;min-width:0}}.tile small,.tile strong{{display:block}}.tile strong{{font-size:1.2rem;margin-top:.4rem;overflow-wrap:anywhere}}.table-wrap{{overflow-x:auto}}table{{width:100%;border-collapse:collapse;text-align:left;font-size:.9rem}}th,td{{padding:.7rem;border-bottom:1px solid #e3e5e0;vertical-align:top}}th{{color:#444c43;white-space:nowrap}}td{{overflow-wrap:anywhere}}.notes{{line-height:1.6;max-width:90ch}}@media(max-width:600px){{section{{padding:1rem}}}}
</style></head><body><main><div class="banner"><span class="status">{html.escape(summary['verdict'])}</span><h1>Archon 1.6.5 benchmark</h1><p class="lead">Full suite, three trials per task, medium effort. Published Archon 1.6.4 versus the candidate on Aether 1.4.3.</p><ul>{details}</ul></div>{models}<section class="notes"><h2>Reading these results</h2><p>Intervals are 95% Wilson for pass rates and percentile bootstrap for ratios of means. McNemar is an exact paired test; wall and cost p-values use Mann–Whitney. Three trials per task leave substantial uncertainty. Cost includes billed and estimated usage.</p><p>See README.md and summary.json in this directory for provenance, run data, exclusions, flipped tasks, and limitations.</p></section></main></body></html>'''


def main():
    directory = Path(sys.argv[1])
    summary = json.loads((directory / 'summary.json').read_text())
    assert directory.is_dir()
    (directory / 'index.html').write_text(render(summary))


if __name__ == '__main__':
    main()
