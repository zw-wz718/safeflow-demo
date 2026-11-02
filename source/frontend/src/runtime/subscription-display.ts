// Lossless UUID encoding: 128 bits become 26 base-32 digits, with no truncated
// digest or mutable sequence. Presentation IDs are never sent to the backend.
export function subscriptionRecordId(nonce: string, _history?: unknown): string {
  if (typeof nonce !== 'string' || !nonce) return '记录信息待核对';
  if (/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(nonce)) {
    let value = BigInt('0x' + nonce.replaceAll('-', ''));
    const alphabet = '0123456789ABCDEFGHJKMNPQRSTVWXYZ';
    let label = '';
    for (let i = 0; i < 26; i++) {
      label = alphabet[Number(value & 31n)] + label;
      value >>= 5n;
    }
    return 'SUB-' + label;
  }
  // Preserve an older non-UUID selector verbatim for recovery while encoding
  // its complete UTF-16 units for an unambiguous presentation fallback.
  return (
    'SUB-L-' +
    Array.from({ length: nonce.length }, (_, index) =>
      nonce.charCodeAt(index).toString(16).padStart(4, '0'),
    )
      .join('')
      .toUpperCase()
  );
}
