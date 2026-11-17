// Animation helpers that settle at once when motion is reduced.
const REDUCED = '(prefers-reduced-motion: reduce)';
const FALLBACK_MS = 450;

export function reducedMotion() {
  return typeof matchMedia === 'undefined' || matchMedia(REDUCED).matches;
}

// Resolves when el's transition or animation ends, or after a fallback delay.
export function settled(el, ms = FALLBACK_MS) {
  if (reducedMotion()) return Promise.resolve();
  return new Promise((resolve) => {
    let done = false;
    const finish = (event) => {
      if (done || (event && event.target !== el)) return;
      done = true;
      el.removeEventListener('transitionend', finish);
      el.removeEventListener('animationend', finish);
      clearTimeout(timer);
      resolve();
    };
    el.addEventListener('transitionend', finish);
    el.addEventListener('animationend', finish);
    const timer = setTimeout(finish, ms);
  });
}

// Fade an element out without animating layout.
export async function collapseOut(el) {
  if (!el.isConnected) return;
  el.classList.add('is-leaving');
  await settled(el);
  el.remove();
}

export function nextFrame() {
  return new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
}

export function animateMove(element, from, to, duration = 280) {
  if (!element || typeof element.animate !== 'function' || reducedMotion()) return;
  if (!Array.isArray(from) || !Array.isArray(to) || ![...from, ...to, duration].every(Number.isFinite)) throw new TypeError('Motion needs finite coordinates and duration');
  if (from.length !== 2 || to.length !== 2 || duration < 0) throw new RangeError('Motion needs two-dimensional positions');
  const animation = element.animate([
    { transform: `translate(${from[0]}px, ${from[1]}px)` },
    { transform: `translate(${to[0]}px, ${to[1]}px)` },
  ], { duration, easing: 'cubic-bezier(.22,1,.36,1)' });
  const media = matchMedia(REDUCED);
  const stop = () => { if (media.matches) animation.cancel(); };
  const cleanup = () => media.removeEventListener('change', stop);
  media.addEventListener('change', stop);
  animation.addEventListener('finish', cleanup, { once: true });
  animation.addEventListener('cancel', cleanup, { once: true });
}
