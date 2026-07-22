import type { Session } from '@opencode-ai/sdk/v2';

export type SessionMetadataRecord = Record<string, unknown>;

type GridforgeMetadata = {
  kind?: 'review';
  originalSessionID?: string;
  reviewSessionID?: string;
};

const isRecord = (value: unknown): value is Record<string, unknown> =>
  Boolean(value && typeof value === 'object' && !Array.isArray(value));

export const getSessionMetadata = (session: Session | null | undefined): SessionMetadataRecord => {
  const metadata = (session as (Session & { metadata?: unknown }) | null | undefined)?.metadata;
  return isRecord(metadata) ? metadata : {};
};

const getGridforgeMetadata = (metadata: SessionMetadataRecord): GridforgeMetadata => {
  const value = metadata.gridforge;
  return isRecord(value) ? value as GridforgeMetadata : {};
};

export const getReviewSessionID = (session: Session | null | undefined): string | null => {
  const value = getGridforgeMetadata(getSessionMetadata(session)).reviewSessionID;
  return typeof value === 'string' && value.trim().length > 0 ? value : null;
};

export const getOriginalSessionID = (session: Session | null | undefined): string | null => {
  const value = getGridforgeMetadata(getSessionMetadata(session)).originalSessionID;
  return typeof value === 'string' && value.trim().length > 0 ? value : null;
};

export const isReviewSession = (session: Session | null | undefined): boolean =>
  getGridforgeMetadata(getSessionMetadata(session)).kind === 'review' && Boolean(getOriginalSessionID(session));

export const withReviewSessionLink = (
  metadata: SessionMetadataRecord,
  reviewSessionID: string,
): SessionMetadataRecord => {
  const current = getGridforgeMetadata(metadata);
  return {
    ...metadata,
    gridforge: {
      ...current,
      reviewSessionID,
    },
  };
};

export const withReviewSessionMarker = (
  metadata: SessionMetadataRecord,
  originalSessionID: string,
): SessionMetadataRecord => {
  const current = getGridforgeMetadata(metadata);
  return {
    ...metadata,
    gridforge: {
      ...current,
      kind: 'review' as const,
      originalSessionID,
    },
  };
};

export const withoutReviewSessionLink = (
  metadata: SessionMetadataRecord,
  reviewSessionID: string,
): SessionMetadataRecord => {
  const current = getGridforgeMetadata(metadata);
  if (current.reviewSessionID !== reviewSessionID) return metadata;

  const restGridforge = { ...current };
  delete restGridforge.reviewSessionID;
  const next: SessionMetadataRecord = { ...metadata };
  if (Object.keys(restGridforge).length > 0) {
    next.gridforge = restGridforge;
  } else {
    delete next.gridforge;
  }
  return next;
};
