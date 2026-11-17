import { h } from '../../ui/dom.js';
import { usd } from '../../core/format.js';

export function spendSeries(report) {
  if (!Array.isArray(report?.rows) || !report.range) throw new TypeError('Spend report needs rows and a date range');
  const start = Date.parse(report.range.since);
  const end = Date.parse(report.range.until);
  const count = Math.round((end - start) / 86400000) + 1;
  if (!Number.isFinite(count) || count < 1 || count > 366) throw new RangeError('Spend chart needs a range of 1–366 days');
  const days = new Map();
  for (const row of report.rows) {
    if (!/^\d{4}-\d{2}-\d{2}$/.test(row.key) || !Number.isFinite(row.counted_usd) || row.counted_usd < 0) {
      throw new TypeError('Spend days need ISO dates and nonnegative recorded amounts');
    }
    if (days.has(row.key)) throw new Error(`Duplicate spend day ${row.key}`);
    days.set(row.key, row.counted_usd);
  }
  return Array.from({ length: count }, (_, index) => {
    const day = new Date(start + index * 86400000).toISOString().slice(0, 10);
    return { day, value: days.get(day) ?? 0 };
  });
}

function svg(tag, props, text = null) {
  const element = document.createElementNS('http://www.w3.org/2000/svg', tag);
  for (const [key, value] of Object.entries(props)) element.setAttribute(key, String(value));
  if (text !== null) element.textContent = text;
  return element;
}

export function spendChart(report) {
  const days = spendSeries(report);
  const max = Math.max(1, ...days.map((day) => day.value));
  const points = days.map((day, i) => ({ x: 8 + (i / Math.max(1, days.length - 1)) * 584, y: 140 - day.value / max * 124 }));
  const chart = svg('svg', { class: 'spend-chart', viewBox: '0 0 600 150', role: 'img', 'aria-label': `Daily spend, ${days[0].day} to ${days.at(-1).day}. Values in the table below.` });
  for (const y of [16, 78, 140]) chart.append(svg('path', { d: `M8 ${y}H592`, class: 'chart-grid' }));
  chart.append(svg('path', { d: points.map((p, i) => `${i ? 'L' : 'M'}${p.x} ${p.y}`).join(' '), class: 'chart-line' }));
  points.forEach((point, index) => {
    const dot = svg('circle', { cx: point.x, cy: point.y, r: 4, class: 'chart-dot' });
    dot.append(svg('title', {}, `${days[index].day}: ${usd(days[index].value)}`));
    chart.append(dot);
  });
  return h('div', {},
    h('p', { class: 'chart-total', text: usd(report.total.counted_usd) }),
    h('p', { class: 'chart-caption', text: `Recorded usage · ${days.length} days · all repositories` }), chart,
    h('div', { class: 'chart-axis' }, h('span', { text: days[0].day }), h('span', { text: days.at(-1).day })),
    h('details', { class: 'chart-data' }, h('summary', {}, 'View daily amounts'),
      h('table', { class: 'chart-table' }, h('caption', { class: 'sr-only', text: 'Daily spend in US dollars' }),
        h('thead', {}, h('tr', {}, h('th', { scope: 'col', text: 'Date' }), h('th', { scope: 'col', text: 'Spend' }))),
        h('tbody', {}, days.map((day) => h('tr', {}, h('td', { text: day.day }), h('td', { text: usd(day.value) })))))));
}
