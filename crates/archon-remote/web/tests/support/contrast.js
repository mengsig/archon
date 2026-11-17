// Checks rendered text against its composited ancestor backgrounds, including SVG labels.
export async function textContrastProblems(page) {
  return page.evaluate(() => {
    const rgba = (value) => {
      const parts = value.match(/[\d.]+/g)?.map(Number);
      return parts?.length >= 3 ? [parts[0], parts[1], parts[2], parts[3] ?? 1] : null;
    };
    const blend = (front, back) => front.slice(0, 3).map((channel, i) => channel * front[3] + back[i] * (1 - front[3]));
    const luminance = (color) => color.slice(0, 3).map((channel) => channel / 255)
      .map((channel) => channel <= .04045 ? channel / 12.92 : ((channel + .055) / 1.055) ** 2.4)
      .reduce((sum, channel, i) => sum + channel * [.2126, .7152, .0722][i], 0);
    const background = (element) => {
      const layers = [];
      for (let node = element; node; node = node.parentElement) layers.push(rgba(getComputedStyle(node).backgroundColor) ?? [0, 0, 0, 0]);
      return layers.reverse().reduce((back, front) => blend(front, back), [255, 255, 255]);
    };
    const problems = [];
    for (const element of document.querySelectorAll('body *')) {
      if (!element.checkVisibility?.({ checkOpacity: true, checkVisibilityCSS: true }) || element.closest('[aria-hidden="true"], [disabled], .sr-only')) continue;
      const text = [...element.childNodes].filter((node) => node.nodeType === Node.TEXT_NODE).map((node) => node.textContent).join('').trim();
      if (!text || element.tagName.toLowerCase() === 'title') continue;
      const style = getComputedStyle(element);
      const foreground = rgba(element instanceof SVGElement ? style.fill : style.color);
      if (!foreground || Number(style.opacity) < 1) continue;
      let back = background(element);
      if (element.closest('.graph-node')) back = rgba(getComputedStyle(element.closest('.graph-node').querySelector('rect')).fill);
      const front = blend(foreground, back);
      const l1 = luminance(front), l2 = luminance(back);
      const ratio = (Math.max(l1, l2) + .05) / (Math.min(l1, l2) + .05);
      const large = parseFloat(style.fontSize) >= 24 || (parseFloat(style.fontSize) >= 18.66 && Number(style.fontWeight) >= 700);
      if (ratio + .02 < (large ? 3 : 4.5)) problems.push({ text: text.slice(0, 60), ratio: +ratio.toFixed(2), color: style.color, background: back });
    }
    return problems;
  });
}
