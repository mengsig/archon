// The HTTP half of the server contract: health, session and pairing.
export class ApiError extends Error {
  constructor(code, message, status, retryAfterSecs = null, options = undefined) {
    super(message || code, options);
    this.name = 'ApiError';
    Object.assign(this, { code, status, retryAfterSecs });
  }
}

async function call(method, path, body) {
  if (!path.startsWith('/api/')) throw new Error(`not an API path: ${path}`);
  const init = { method, credentials: 'same-origin', cache: 'no-store', headers: {} };
  if (body !== undefined) {
    init.headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(body);
  }
  let response;
  try {
    response = await fetch(path, init);
  } catch (err) {
    throw new ApiError('network', 'The host cannot be reached.', 0, null, { cause: err });
  }
  const payload = await response.json().catch(() => null);
  if (response.ok) return payload ?? {};
  const retry = Number.parseInt(response.headers.get('Retry-After') ?? '', 10);
  throw new ApiError(
    payload?.error?.code ?? `http_${response.status}`,
    payload?.error?.message ?? `The host answered ${response.status}.`,
    response.status,
    Number.isFinite(retry) ? retry : null,
  );
}

export function health() {
  return call('GET', '/api/health');
}

// The paired session, or null when this device is not (or no longer) paired.
export async function session() {
  try {
    const value = await call('GET', '/api/session');
    if (!value.device || !Number.isInteger(value.protocol_version)) {
      throw new ApiError('bad_session', 'The host sent a session without a device or protocol version.', 200);
    }
    return value;
  } catch (err) {
    if (err instanceof ApiError && err.status === 401) return null;
    throw err;
  }
}

export function claim(code, deviceName) {
  return call('POST', '/api/pair/claim', { code, device_name: deviceName });
}

export function forget() {
  return call('POST', '/api/session/forget', {});
}
