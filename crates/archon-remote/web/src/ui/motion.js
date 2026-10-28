// Animation helpers that settle at once when motion is reduced.
const REDUCED = '(prefers-reduced-motion: reduce)';
const FALLBACK_MS = 450;

export function reducedMotion() {
  return matchMedia(REDUCED).matches;
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

// Fold an element's height to zero and fade it, then remove it.
export async function collapseOut(el) {
  if (!el.isConnected) return;
  el.style.height = `${el.getBoundingClientRect().height}px`;
  el.getBoundingClientRect();
  el.classList.add('is-leaving');
  el.style.height = '0px';
  await settled(el);
  el.remove();
}

export function nextFrame() {
  return new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
}
