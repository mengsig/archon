"""Render the benchmark comparison as Markdown."""
import json
import sys
from pathlib import Path


def percent(value):
    return 'n/a' if value is None else f'{100 * value:.1f}%'


def ratio(values):
    return 'n/a' if any(value is None for value in values) else f'{values[0]:.2f}× [{values[1]:.2f}, {values[2]:.2f}]'


def formatted(value, pattern, prefix='', suffix=''):
    return 'n/a' if value is None else prefix + format(value, pattern) + suffix


def arm_table(data):
    assert set(data['arms']) == {'old', 'new'}
    assert set(data['aether_arms']) == {'old', 'new'}
    lines = ['| Version | Arm | Pass / graded | Pass % [95% CI] | Wall mean | Cost / trial | Reviewers / item | Infra |',
             '|---|---|---:|---|---:|---:|---:|---:|']
    for version in ('old', 'new'):
        for label, arms in [('Archon', data['arms']), ('Direct Aether', data['aether_arms'])]:
            a = arms[version]
            lines.append(f"| {version.upper()} | {label} | {a['pass']}/{a['graded']} | {percent(a['rate'])} [{percent(a['ci'][0])}, {percent(a['ci'][1])}] | {formatted(a['wall_mean'], '.0f', suffix=' s')} | {formatted(a['cost_mean'], '.4f', prefix='$')} | {a['reviewers_per_item']:.2f} | {a['infra']} |")
    return '\n'.join(lines)


def comparison_table(data):
    assert 'archon' in data and 'aether' in data
    assert all('mcnemar' in data[arm] for arm in ('archon', 'aether'))
    lines = ['| Arm | Pass difference [95% CI] | OLD-only / NEW-only | McNemar p | Wall ratio [95% CI] (p) | Cost ratio [95% CI] (p) |',
             '|---|---|---|---:|---|---|']
    for arm, label in [('archon', 'Archon'), ('aether', 'Direct Aether')]:
        c = data[arm]
        m = c['mcnemar']
        d = c['diff']
        lines.append(f"| {label} | {d[0] * 100:+.1f} pp [{d[1] * 100:+.1f}, {d[2] * 100:+.1f}] | {m['old_only']} / {m['new_only']} of {m['pairs']} | {m['p']:.3f} | {ratio(c['wall_ratio'])} ({formatted(c['wall_p'], '.3f')}) | {ratio(c['cost_ratio'])} ({formatted(c['cost_p'], '.3f')}) |")
    return '\n'.join(lines)


def task_table(tasks):
    assert tasks and len({task['task'] for task in tasks}) == len(tasks)
    assert all(0 <= task[arm]['pass'] <= task[arm]['graded'] for task in tasks for arm in ('old', 'new'))
    lines = ['| Task | OLD pass | NEW pass | OLD wall | NEW wall |', '|---|---:|---:|---:|---:|']
    for task in tasks:
        old, new = task['old'], task['new']
        ow = 'n/a' if old['wall_mean'] is None else f"{old['wall_mean']:.0f} s"
        nw = 'n/a' if new['wall_mean'] is None else f"{new['wall_mean']:.0f} s"
        lines.append(f"| {task['task']} | {old['pass']}/{old['graded']} | {new['pass']}/{new['graded']} | {ow} | {nw} |")
    return '\n'.join(lines)


def findings(data):
    assert 'infra_errors' in data and 'comparison' in data
    assert all(key in data['comparison'] for key in ('flips', 'slow', 'aether_flips', 'aether_slow'))
    lines = []
    for label, key in [('Archon', 'flips'), ('Direct Aether', 'aether_flips')]:
        for flip in data['comparison'][key]:
            pairs = flip['pairs']
            lines.append(f"- {label}, {flip['task']}: {flip['old']} to {flip['new']}; {pairs['old_only']} OLD-only and {pairs['new_only']} NEW-only passes.")
            for version, history in [('OLD', flip['old_history']), ('NEW', flip['history'])]:
                for trial in history:
                    if not trial['success'] and trial['first_error'] != 'none recorded':
                        lines.append(f"  - {version} trial {trial['trial']}: {trial['first_error']}")
    for label, key in [('Archon', 'slow'), ('Direct Aether', 'aether_slow')]:
        for slow in data['comparison'][key]:
            lines.append(f"- {label}, {slow['task']}: mean wall {slow['old_mean']:.0f} s to {slow['new_mean']:.0f} s ({slow['ratio']:.2f}×, p={formatted(slow['p'], '.3f')}).")
            for pair in slow['slow_pairs']:
                lines.append(f"  - Trial {pair['trial']}: {pair['old_wall']:.0f} s to {pair['new_wall']:.0f} s.")
    for version, errors in data['infra_errors'].items():
        for error in errors:
            lines.append(f"- Infra, {version.upper()} {error['arm']} {error['task']} trial {error['trial']}: {error['error']}")
    return '\n'.join(lines) if lines else '- No paired flips or infrastructure errors.'


def model_section(name, data):
    assert name and 'comparison' in data
    assert 'archon' in data['comparison'] and 'arms' in data and 'aether_arms' in data
    c = data['comparison']['archon']
    loss = c['diff'][2] < 0 or (c['mcnemar']['p'] < .05 and c['mcnemar']['old_only'] > c['mcnemar']['new_only'])
    cost = c['cost_ratio'][1] > 1 and c['cost_p'] < .05
    wall = c['wall_ratio'][1] > 1 and c['wall_p'] < .05
    gaps = ', '.join(f"{version.upper()} {label}: {arm['cost_gaps']}" for label, arms in [('Archon', data['arms']), ('Aether', data['aether_arms'])] for version, arm in arms.items() if arm['cost_gaps']) or 'none reported'
    caveat = ' Some recorded totals are floors, so actual cost differences remain uncertain.' if gaps != 'none reported' else ''
    statement = f"Cost gaps: {gaps}.{caveat}\n\nSignificant pass-rate drop: {'yes' if loss else 'no'}; significant recorded-cost increase: {'yes' if cost else 'no'}; significant slowdown: {'yes' if wall else 'no'}."
    return f"## {name}\n\n{statement}\n\n{arm_table(data)}\n\n### NEW versus OLD\n\n{comparison_table(data['comparison'])}\n\n### Archon per task\n\n{task_table(data['comparison']['tasks_archon'])}\n\n### Paired flips, slower tasks and infrastructure\n\n{findings(data)}\n"


def method(summary):
    models = summary['models']
    assert models and all('starts' in data for data in models.values())
    hashes = next(iter(models.values()))['starts']
    old, new = hashes['old']['hashes'], hashes['new']['hashes']
    assert old['aether'] == new['aether'], 'Aether binaries differ'
    return f"""## Method and provenance

Each run uses every task in `bench/tasks`, three trials per task and arm, medium effort, and Archon and direct-Aether arms. Design sessions remain at their default. Each run uses a private Aether daemon and isolated workspaces; Luna runs used `--parallel 1`. Haiku's initial serial runs were stopped on operator direction and restarted together at `--parallel 3`; the earlier cells are excluded from the final comparison but retained under `runs/probes/`.

OLD is published Archon v1.6.4. NEW is built with `cargo build --release --locked -p archon -p archon-bench` at `c4bcaa9f8eff80ca81917f9ddaf65a866abb3690`. The `archon_commit` in a run's metadata is the reporting checkout's HEAD at run time, which can include later report-only commits; the candidate binary is the recorded build at `c4bcaa9f`. Both use published Aether v1.4.3. The x86_64 musl archives passed their release SHA256SUMS checks. OLD uses the v1.6.4-tag harness because the candidate harness speaks a newer incompatible Archon protocol; NEW uses the candidate harness. The candidate harness also includes the helper-event handling fix from #459, so observed differences may include harness effects. Task definitions are identical across those versions.

| Binary | SHA-256 |
|---|---|
| Archon v1.6.4 | `{old['archon']}` |
| Candidate Archon | `{new['archon']}` |
| Aether v1.4.3 | `{old['aether']}` |
| v1.6.4 harness | `{old['archon-bench']}` |
| Candidate harness | `{new['archon-bench']}` |

The Archon and Aether release archive SHA-256 values are `12a79a01b3f2747a5918db45b4868e4a324a27733127bdff0468eae49476b091` and `25cf7df3db41fdab660fc5a5781e82f0f7b4de34990e945dbf62778a8846de5f`, respectively.

Intervals are 95% Wilson for rates, Newcombe hybrid for rate differences, and 4,000-draw percentile bootstrap intervals for ratios of means. McNemar is exact two-sided on task/trial pairs graded in both runs; wall and cost p-values are two-sided Mann–Whitney. A regression is a significant pass-rate loss, or a wall or cost ratio interval above 1 with p < 0.05. Three trials per cell leave substantial uncertainty. Task slowdown notes are descriptive: they flag a mean at least 1.5× and 30 seconds higher, or a paired trial at least 2× and 60 seconds higher. Costs include billed and estimated usage; estimates can be floors.

Two Luna setup probes were excluded: the candidate harness could not connect to the OLD daemon (protocol 26 versus 24), and an initial NEW run was stopped to correct its individual budget. Their private Aether databases recorded approximately $0.002024 and $0.006364, respectively; both are included in the combined model guard. The failed protocol cell is an infrastructure error, not a graded outcome. Agents ran without the optional systemd resource scope because the isolated environment had no user bus. The Luna guard log includes one intentional monitor restart; it did not interrupt a valid trial. The excluded Haiku serial runs recorded $1.227877, included in the combined Haiku budget. Luna used the original per-run agent temp roots, while later runs use shorter isolated roots; the recorded candidate binary did not change.

The trial records and grading evidence are in `runs/`; occurrences of the configured Git email are redacted in copied logs. `summary.json` contains the numerical results and per-run binary hashes; `index.html` is the visual report. Scripts in `scripts/` document the run and analysis. From the repository root, regenerate the numeric and visual reports with `python3 bench/results-full-1.6.5/scripts/analyze.py bench/results-full-1.6.5/runs bench/results-full-1.6.5`, then run `render.py` and `markdown.py` with `bench/results-full-1.6.5` as their argument.
"""


def render(summary):
    assert summary['verdict'] in ('NO REGRESSION', 'REGRESSION')
    assert summary['models']
    lines = [f"# Archon 1.6.5 full benchmark\n\n**{summary['verdict']}**\n"]
    lines += [f'- {reason}' for reason in summary['reasons']]
    lines.append('\n[Visual report](index.html) · [Numeric summary](summary.json)\n')
    for name, data in summary['models'].items():
        model_id = data['meta']['new']['models']['archon']
        lines.append(model_section(f'{name.title()} ({model_id})', data))
    if summary.get('budgets'):
        lines.append('## Combined model spend\n')
        for name, budget in summary['budgets'].items():
            lines.append(f"- {name.title()}: ${budget['total_usd']:.4f} including probes and all private daemon usage.")
    lines.append(method(summary))
    return '\n'.join(lines)


def main():
    directory = Path(sys.argv[1])
    summary = json.loads((directory / 'summary.json').read_text())
    assert directory.is_dir() and summary
    (directory / 'README.md').write_text(render(summary))


if __name__ == '__main__':
    main()
