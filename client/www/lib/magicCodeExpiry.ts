export const DEFAULT_MAGIC_CODE_EXPIRY_MINUTES = 1440;

export function effectiveMagicCodeExpiryMinutes(
  minutes: number | null | undefined,
) {
  return minutes ?? DEFAULT_MAGIC_CODE_EXPIRY_MINUTES;
}

export function magicCodeExpirationLabel(minutes: number | null | undefined) {
  const effective = effectiveMagicCodeExpiryMinutes(minutes);
  if (effective >= 60) {
    const hours = Math.floor(effective / 60);
    return `${hours} hour${hours > 1 ? 's' : ''}`;
  }
  return `${effective} minute${effective > 1 ? 's' : ''}`;
}
