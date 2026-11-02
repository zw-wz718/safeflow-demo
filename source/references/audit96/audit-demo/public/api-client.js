export class ApiError extends Error {
  constructor(message, status, code = null) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
  }
}

export async function callApi(path, options, fetchImpl = globalThis.fetch) {
  const response = await fetchImpl(path, options);
  let value;
  let body;
  try { body = await response.text(); }
  catch {
    throw new ApiError(`HTTP ${response.status} response body could not be read`, response.status, 'INVALID_JSON_RESPONSE');
  }
  try { value = JSON.parse(body); }
  catch {
    throw new ApiError(`HTTP ${response.status} returned invalid or incomplete JSON`, response.status, 'INVALID_JSON_RESPONSE');
  }
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new ApiError(`HTTP ${response.status} response must be a JSON object`, response.status, 'INVALID_JSON_RESPONSE');
  }
  if (!response.ok) {
    throw new ApiError(value.message ?? value.error ?? `HTTP ${response.status}`, response.status, value.error ?? null);
  }
  return value;
}

// A missing participant receipt is expected for base/runtime events; all other
// HTTP and network errors continue to the caller for visible feedback.
export async function loadOptionalReceipt(load, renderReceipt) {
  try {
    const receipt = await load();
    renderReceipt(receipt ?? null);
    return { available: Boolean(receipt) };
  } catch (error) {
    if (error instanceof ApiError && error.status === 404 && error.code === 'AUDIT_NOT_FOUND') {
      renderReceipt(null);
      return { available: false };
    }
    throw error;
  }
}
