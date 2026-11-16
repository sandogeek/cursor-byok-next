export const motion = {
  instant: "duration-120 ease-out",
  fast: "duration-180 ease-soft",
  base: 240,
  distance: 8,
} as const;

export const controlMotion = `transition ${motion.fast} motion-reduce:transition-none motion-reduce:active:scale-100`;
export const colorMotion = `transition-colors ${motion.instant} motion-reduce:transition-none`;
