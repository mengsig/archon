"""Redact personal email addresses from bench output before it is committed."""

import re

PLACEHOLDER = b'[redacted-email]'
EMAIL = re.compile(rb'(?<![A-Za-z0-9._%+-])[A-Za-z0-9][A-Za-z0-9._%+-]*@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}')
KEPT_DOMAINS = (b'anthropic.com', b'users.noreply.github.com', b'example.com',
                b'example.invalid', b'example.test', b'example.org', b'example.net')


KEPT_ADDRESSES = (b'archon@openai.com',)


def is_kept(address):
    if address.lower() in KEPT_ADDRESSES:
        return True
    local, _, domain = address.lower().partition(b'@')
    if local == b'noreply' or domain.startswith(b'noreply.'):
        return True
    return any(domain == kept or domain.endswith(b'.' + kept) for kept in KEPT_DOMAINS)


def redact_bytes(data):
    assert isinstance(data, bytes)
    result = EMAIL.sub(lambda match: match.group(0) if is_kept(match.group(0)) else PLACEHOLDER, data)
    assert not any(not is_kept(found) for found in EMAIL.findall(result))
    return result
