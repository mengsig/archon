// What this paired device may do. Fails closed: any role but `operator`
// gets the read-only views; the daemon refuses an observer's changes anyway.
export function canOperate(device) {
  if (!device || typeof device.id !== 'string' || !device.id) throw new TypeError('a paired session names its device');
  if (device.role !== undefined && typeof device.role !== 'string') throw new TypeError(`a device role is a string, got ${typeof device.role}`);
  return device.role === 'operator';
}

// For a screen that must never be opened from an observer device.
export function assertOperator(ctx, what) {
  if (typeof ctx?.canOperate !== 'boolean') throw new TypeError(`${what} needs the app context`);
  if (!ctx.canOperate) throw new Error(`${what} is an operator action; this device is an observer`);
}
