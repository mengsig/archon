#!/usr/bin/env python3
import json
import pathlib
import sys

root = pathlib.Path(__file__).parent
state_file = root / 'prs.json'
state = json.loads(state_file.read_text()) if state_file.exists() else {}
args = sys.argv[1:]
with (root / 'gh-calls.jsonl').open('a') as log:
    log.write(json.dumps(args) + '\n')
assert args[0] == 'pr', args
branch = args[args.index('--head') + 1] if args[1] == 'list' else args[2]
if branch not in state:
    number = len(state) + 1
    state[branch] = dict(url=f'https://github.com/o/r/pull/{number}', number=number,
        headRefName=branch, baseRefName='main', state='OPEN', body='Change details.', isDraft=True)
pr = state[branch]
if args[1] == 'list':
    print(json.dumps([pr] if pr['state'] == 'OPEN' else []))
elif args[1] == 'view':
    print(json.dumps(pr))
elif args[1] == 'edit':
    if '--base' in args:
        pr['baseRefName'] = args[args.index('--base') + 1]
    if '--body-file' in args:
        pr['body'] = pathlib.Path(args[args.index('--body-file') + 1]).read_text()
elif args[1] == 'ready':
    pr['isDraft'] = '--undo' in args
else:
    raise AssertionError(args)
state_file.write_text(json.dumps(state))
