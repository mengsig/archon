#!/usr/bin/env python3
"""A GitHub that keeps pull requests in `prs.json` beside itself.

Without `gh-explicit` a pull request exists as soon as its branch is asked
about, as if a ship agent had opened it; with it, only `pr create` opens one.
"""
import json
import pathlib
import sys

root = pathlib.Path(__file__).parent
state_file = root / 'prs.json'
state = json.loads(state_file.read_text()) if state_file.exists() else {}
explicit = (root / 'gh-explicit').exists()
args = sys.argv[1:]
with (root / 'gh-calls.jsonl').open('a') as log:
    log.write(json.dumps(args) + '\n')


def flag(name):
    return args[args.index(name) + 1]


def opened(branch, base='main', draft=True, body='Change details.', title=''):
    number = len(state) + 1
    state[branch] = dict(url=f'https://github.com/o/r/pull/{number}', number=number,
        headRefName=branch, baseRefName=base, state='OPEN', body=body, title=title,
        isDraft=draft, autoMerge=False)
    return state[branch]


def named(ref):
    """The pull request of a branch or URL; opened on demand unless explicit."""
    for pr in state.values():
        if ref in (pr['headRefName'], pr['url']):
            return pr
    if explicit:
        sys.exit(f'no pull requests found for branch "{ref}"')
    return opened(ref)


if args[:2] == ['repo', 'view']:
    print(json.dumps(dict(squashMergeAllowed=True, mergeCommitAllowed=True)))
    sys.exit(0)
assert args[0] == 'pr', args
if args[1] == 'create':
    assert flag('--head') not in state, 'a pull request for the branch already exists'
    pr = opened(flag('--head'), flag('--base'), '--draft' in args, sys.stdin.read(),
        flag('--title'))
    print(pr['url'])
elif args[1] == 'list':
    wanted = flag('--state')
    if '--head' in args:
        known = flag('--head') in state or not explicit
        prs = [named(flag('--head'))] if known else []
    else:
        prs = list(state.values())
    print(json.dumps([pr for pr in prs if wanted == 'all' or pr['state'] == 'OPEN']))
elif args[1] == 'view':
    print(json.dumps(named(args[2])))
elif args[1] == 'edit':
    pr = named(args[2])
    if '--base' in args:
        pr['baseRefName'] = flag('--base')
    if '--body-file' in args:
        pr['body'] = pathlib.Path(flag('--body-file')).read_text()
elif args[1] == 'ready':
    named(args[2])['isDraft'] = '--undo' in args
elif args[1] == 'merge':
    assert '--auto' in args, args
    pr = named(args[2])
    assert not pr['isDraft'], 'GitHub refuses auto-merge on a draft'
    pr['autoMerge'] = True
else:
    raise AssertionError(args)
state_file.write_text(json.dumps(state))
