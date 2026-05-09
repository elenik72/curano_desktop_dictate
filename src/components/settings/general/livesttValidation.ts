const LIVESTT_SERVER_URL_ERROR_KEY =
  "settings.transcriptionBackend.livestt.serverUrl.error";

export const MIN_FINALIZE_TIMEOUT_MS = 1;
export const MAX_FINALIZE_TIMEOUT_MS = 120000;
export const MAX_LIVESTT_TEXT_CHARS = 10000;
export const MAX_LIVESTT_TERMS = 1000;
export const MAX_LIVESTT_TERM_CHARS = 200;
export const MAX_LIVESTT_GENERAL_ENTRIES = 50;
export const MAX_LIVESTT_GENERAL_KEY_CHARS = 200;
export const MAX_LIVESTT_GENERAL_VALUE_CHARS = 1000;

export interface LiveSttServerUrlValidationResult {
  normalized: string;
  isEmpty: boolean;
  isValid: boolean;
}

const isAllowedLiveSttHttpHost = (host: string): boolean => {
  const normalizedHost = host.toLowerCase();
  return (
    normalizedHost === "localhost" ||
    normalizedHost === "127.0.0.1" ||
    normalizedHost === "::1" ||
    normalizedHost === "[::1]"
  );
};

export const normalizeLiveSttServerUrlInput = (
  input: string,
): LiveSttServerUrlValidationResult => {
  const trimmed = input.trim();
  if (!trimmed) {
    return { normalized: "", isEmpty: true, isValid: true };
  }

  try {
    const url = new URL(trimmed);

    if (url.protocol === "http:") {
      if (!isAllowedLiveSttHttpHost(url.hostname)) {
        return { normalized: "", isEmpty: false, isValid: false };
      }
    } else if (url.protocol !== "https:") {
      return { normalized: "", isEmpty: false, isValid: false };
    }

    if ((url.pathname && url.pathname !== "/") || url.search || url.hash) {
      return { normalized: "", isEmpty: false, isValid: false };
    }

    return { normalized: url.origin, isEmpty: false, isValid: true };
  } catch {
    return { normalized: "", isEmpty: false, isValid: false };
  }
};

export const validateLiveSttServerUrlInput = (value: string): string | null => {
  return normalizeLiveSttServerUrlInput(value).isValid
    ? null
    : LIVESTT_SERVER_URL_ERROR_KEY;
};

export const isLiveSttServerUrlValidForLogin = (value: string): boolean => {
  const validation = normalizeLiveSttServerUrlInput(value);
  return !validation.isEmpty && validation.isValid;
};

export const parseConsultationIdInput = (value: string): string | null => {
  const trimmed = value.trim();
  if (trimmed === "") {
    return "";
  }

  const parsed = Number(trimmed);
  if (!/^\d+$/.test(trimmed) || !Number.isSafeInteger(parsed) || parsed < 1) {
    return null;
  }

  return String(parsed);
};

export const normalizeLiveSttTextInput = (
  value: string,
): { trimmed: string; isValid: boolean } => {
  const trimmed = value.trim();
  if ([...trimmed].length > MAX_LIVESTT_TEXT_CHARS) {
    return { trimmed, isValid: false };
  }
  return { trimmed, isValid: true };
};

export type GeneralEntry = { key: string; value: string };

export type GeneralValidationError =
  | "tooMany"
  | "keyTooLong"
  | "valueTooLong"
  | "partialRow"
  | "duplicateKey";

export interface GeneralValidationResult {
  entries: GeneralEntry[];
  error: GeneralValidationError | null;
  errorRowIndex: number | null;
  errorKey: string | null;
}

export const normalizeLiveSttGeneralEntries = (
  entries: readonly GeneralEntry[],
): GeneralValidationResult => {
  const result: GeneralEntry[] = [];
  const seen = new Set<string>();

  for (let i = 0; i < entries.length; i += 1) {
    const raw = entries[i];
    const key = raw.key.trim();
    const value = raw.value.trim();

    if (key === "" && value === "") {
      continue;
    }

    if (key === "" || value === "") {
      return {
        entries: [],
        error: "partialRow",
        errorRowIndex: i,
        errorKey: null,
      };
    }

    if ([...key].length > MAX_LIVESTT_GENERAL_KEY_CHARS) {
      return {
        entries: [],
        error: "keyTooLong",
        errorRowIndex: i,
        errorKey: null,
      };
    }

    if ([...value].length > MAX_LIVESTT_GENERAL_VALUE_CHARS) {
      return {
        entries: [],
        error: "valueTooLong",
        errorRowIndex: i,
        errorKey: null,
      };
    }

    if (seen.has(key)) {
      return {
        entries: [],
        error: "duplicateKey",
        errorRowIndex: i,
        errorKey: key,
      };
    }
    seen.add(key);

    result.push({ key, value });
  }

  if (result.length > MAX_LIVESTT_GENERAL_ENTRIES) {
    return {
      entries: [],
      error: "tooMany",
      errorRowIndex: null,
      errorKey: null,
    };
  }

  return { entries: result, error: null, errorRowIndex: null, errorKey: null };
};

export type AddTermResult =
  | { kind: "added"; term: string }
  | { kind: "empty" }
  | { kind: "duplicate" }
  | { kind: "tooLong" }
  | { kind: "tooMany" };

export const tryAddLiveSttTerm = (
  existing: readonly string[],
  rawValue: string,
): AddTermResult => {
  const trimmed = rawValue.trim();
  if (trimmed === "") {
    return { kind: "empty" };
  }
  if ([...trimmed].length > MAX_LIVESTT_TERM_CHARS) {
    return { kind: "tooLong" };
  }
  if (existing.includes(trimmed)) {
    return { kind: "duplicate" };
  }
  if (existing.length >= MAX_LIVESTT_TERMS) {
    return { kind: "tooMany" };
  }
  return { kind: "added", term: trimmed };
};

export const validateFinalizeTimeoutInput = (value: string): number | null => {
  const parsed = Number(value.trim());
  if (
    !Number.isInteger(parsed) ||
    parsed < MIN_FINALIZE_TIMEOUT_MS ||
    parsed > MAX_FINALIZE_TIMEOUT_MS
  ) {
    return null;
  }

  return parsed;
};
