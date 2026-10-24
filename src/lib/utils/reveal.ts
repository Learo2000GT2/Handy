/**
 * Scrolls `element` just far enough to show it (not at all when it is
 * already in view), smoothly unless the user asked for less motion. Used
 * where an action's result lands below the fold of the default window.
 */
export const revealElement = (element: Element | null): void => {
  if (!element) return;
  let reduceMotion = false;
  try {
    reduceMotion = window.matchMedia(
      "(prefers-reduced-motion: reduce)",
    ).matches;
  } catch {
    // No media queries: scroll smoothly.
  }
  element.scrollIntoView({
    block: "nearest",
    behavior: reduceMotion ? "auto" : "smooth",
  });
};
