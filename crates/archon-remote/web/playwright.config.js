import { defineConfig, devices } from '@playwright/test';

export default defineConfig({
  testDir: './tests',
  testMatch: '*.spec.js',
  workers: 1,
  fullyParallel: false,
  forbidOnly: Boolean(process.env.CI),
  timeout: 60_000,
  expect: { timeout: 10_000 },
  reporter: process.env.CI ? 'line' : 'list',
  use: {
    ...devices['Desktop Chrome'],
    viewport: { width: 390, height: 844 },
    deviceScaleFactor: 1,
    hasTouch: true,
    isMobile: false,
    trace: 'retain-on-failure',
  },
  projects: [{ name: 'chromium' }],
});
