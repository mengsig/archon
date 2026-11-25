// The web tests need Node 20 or newer: Playwright does not run on older
// versions, and the unit tests' mock timers behave differently there, so they
// fail in ways that point nowhere. Say so up front instead.
const MIN_MAJOR = 20;
const major = Number(process.versions.node.split('.')[0]);
if (major < MIN_MAJOR) {
  console.error(`The web tests need Node ${MIN_MAJOR} or newer; this is Node ${process.versions.node}.`);
  console.error('Use a newer Node (for example `nvm use 22`, or run the tests in the node:22 image).');
  process.exit(1);
}
