/**
 * Temporary local QA override. Set to false before shipping.
 *
 * This simulates an active Ultra entitlement for exercising premium flows
 * without storing or bypassing a real license key.
 */
export const FORCE_ULTRA_FOR_TESTING = true;

export const QA_ULTRA_LICENSE_DETAILS = {
  isPremium: true,
  plan: 'ultra',
  provider: 'natively_api',
} as const;
