export function storyProgress(sectionTop, sectionHeight, viewportHeight) {
  const lead = Math.min(220, viewportHeight * 0.24);
  const distance = Math.max(1, sectionHeight - viewportHeight);
  return Math.max(0, Math.min(1, (lead - sectionTop) / (distance + lead)));
}

export function storyPhase(progress) {
  return Math.max(0, Math.min(3, Math.floor(progress * 4)));
}
