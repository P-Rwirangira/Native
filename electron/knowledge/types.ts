/** Shared document kinds used by both the optional premium engine and core fallback. */
export enum DocType {
  RESUME = 'resume',
  JD = 'jd',
}

export type ProfileDocType = DocType.RESUME | DocType.JD;
