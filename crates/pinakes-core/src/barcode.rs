//! Barcodes as keys and facts store them: one form per number, check digit verified.

/// A barcode as keys and facts store it: GTIN-13, so a UPC-A (12 digits), an EAN-13, an EAN-8
/// and a GTIN-14 with a leading zero are one value each (zero-padded or the leading zero
/// dropped), with the check digit verified.
pub fn gtin13(code: &str) -> Option<String> {
    let code = code.trim();
    if !matches!(code.len(), 8 | 12 | 13 | 14) || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let padded = format!("{code:0>14}");
    let gtin = padded.strip_prefix('0')?;
    let (body, check) = gtin.split_at(12);
    // Weights 3 and 1 from the right of the body.
    let sum: u32 = body
        .bytes()
        .rev()
        .enumerate()
        .map(|(i, b)| u32::from(b - b'0') * if i % 2 == 0 { 3 } else { 1 })
        .sum();
    ((10 - sum % 10) % 10 == u32::from(check.as_bytes()[0] - b'0')).then(|| gtin.to_owned())
}
/// The GTIN-13 of an ISBN (ISBN-10 or ISBN-13, hyphens allowed): a book's barcode.
pub fn of_isbn(isbn: &str) -> Option<String> {
    let number = crate::resolver::cards::isbn(&format!("isbn:{}", isbn.trim()))?;
    gtin13(&crate::resolver::cards::isbn13(&number))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn barcodes_have_one_form() {
        // UPC-A, its EAN-13 and GTIN-14 spellings, and Deezer's 13-digit form are one value.
        for code in ["602557656527", "0602557656527", "00602557656527"] {
            assert_eq!(gtin13(code).as_deref(), Some("0602557656527"), "{code}");
        }
        assert_eq!(gtin13("196874054246").as_deref(), Some("0196874054246"));
        assert_eq!(gtin13("4006381333931").as_deref(), Some("4006381333931"));
        assert_eq!(gtin13("96385074").as_deref(), Some("0000096385074"));
        for bad in [
            "602557656528",
            "10602557656527",
            "60255765652",
            "60255765652a",
            "",
        ] {
            assert_eq!(gtin13(bad), None, "{bad}");
        }
        assert_eq!(of_isbn("0-14-032872-6").as_deref(), Some("9780140328721"));
        assert_eq!(of_isbn("9780140328721").as_deref(), Some("9780140328721"));
        assert_eq!(of_isbn("0140328727"), None);
    }
}
