Synthetic pages in a legacy character encoding, written for the charset tests (no recording):

- `windows-1252.html`: a page in windows-1252 that names its encoding only in its
  `Content-Type` header (the test sets `charset=windows-1252`).
- `shift_jis.html`: a page in Shift_JIS that names its encoding in a `<meta http-equiv>` in its
  head, served without a charset.
