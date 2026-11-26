"""Redact personal email addresses from bench output before it is committed."""

import re

PLACEHOLDER = b'[redacted-email]'
EMAIL = re.compile(rb'(?<![A-Za-z0-9._%+-])(?=[._%+-]*[A-Za-z0-9])[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2}[A-Za-z0-9-]*')
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


def personal_addresses(data):
    return [found for found in EMAIL.findall(data) if not is_kept(found)]


def personal_addresses_any_encoding(data):
    # Dropping NULs exposes ASCII text stored as UTF-16, as SQLite may do.
    return personal_addresses(data) + personal_addresses(data.replace(b'\0', b''))


def redact_bytes(data):
    assert isinstance(data, bytes)
    result = EMAIL.sub(lambda match: match.group(0) if is_kept(match.group(0)) else PLACEHOLDER, data)
    assert not personal_addresses(result)
    return result
