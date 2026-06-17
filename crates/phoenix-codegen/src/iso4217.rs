//! The active ISO-4217 currency codes, the single source of truth for the
//! `Money` type's `currency` validation across all four targets (Go emits a
//! `map`, TypeScript a `Set`, Python a `set`, OpenAPI an `enum`).
//!
//! The codes are factual data (an ISO standard's alphabetic codes — not
//! copyrightable expression), so this list is **hand-authored from the standard**
//! rather than vendored from a licensed source, per the repo's MIT-only policy
//! ("compute constants from definitions"). [`iso_4217_codes_are_well_formed`]
//! guards the obvious authoring mistakes (non-`^[A-Z]{3}$`, duplicates, wrong
//! sort); a differential test against an MIT-licensed reference set is a possible
//! future hardening.
//!
//! Scope: the active national/supranational *transaction* currencies. The
//! precious-metal (`XAU`/`XAG`/`XPT`/`XPD`), fund (`XBA`–`XBD`), test (`XTS`),
//! and no-currency (`XXX`) codes are intentionally excluded — a `Money.currency`
//! names a spendable currency. The widely-used supranational codes (`EUR`, the
//! CFA francs `XOF`/`XAF`/`XPF`, `XCD`, the IMF's `XDR`) are included, as is the
//! Caribbean guilder `XCG` (which replaced the now-decommissioned Netherlands
//! Antillean guilder `ANG` in 2025 — `ANG` is therefore excluded). Where a country
//! has one spendable code we list exactly that: Venezuela is `VES` only (the
//! transitional digital-bolívar `VED` is excluded).

/// Active ISO-4217 alphabetic currency codes, **ascending** (the emitters rely on
/// the order for stable output). See the module docs for scope.
pub(crate) const ISO_4217_CODES: &[&str] = &[
    "AED", "AFN", "ALL", "AMD", "AOA", "ARS", "AUD", "AWG", "AZN", "BAM", "BBD", "BDT", "BGN",
    "BHD", "BIF", "BMD", "BND", "BOB", "BRL", "BSD", "BTN", "BWP", "BYN", "BZD", "CAD", "CDF",
    "CHF", "CLP", "CNY", "COP", "CRC", "CUP", "CVE", "CZK", "DJF", "DKK", "DOP", "DZD", "EGP",
    "ERN", "ETB", "EUR", "FJD", "FKP", "GBP", "GEL", "GHS", "GIP", "GMD", "GNF", "GTQ", "GYD",
    "HKD", "HNL", "HTG", "HUF", "IDR", "ILS", "INR", "IQD", "IRR", "ISK", "JMD", "JOD", "JPY",
    "KES", "KGS", "KHR", "KMF", "KPW", "KRW", "KWD", "KYD", "KZT", "LAK", "LBP", "LKR", "LRD",
    "LSL", "LYD", "MAD", "MDL", "MGA", "MKD", "MMK", "MNT", "MOP", "MRU", "MUR", "MVR", "MWK",
    "MXN", "MYR", "MZN", "NAD", "NGN", "NIO", "NOK", "NPR", "NZD", "OMR", "PAB", "PEN", "PGK",
    "PHP", "PKR", "PLN", "PYG", "QAR", "RON", "RSD", "RUB", "RWF", "SAR", "SBD", "SCR", "SDG",
    "SEK", "SGD", "SHP", "SLE", "SOS", "SRD", "SSP", "STN", "SVC", "SYP", "SZL", "THB", "TJS",
    "TMT", "TND", "TOP", "TRY", "TTD", "TWD", "TZS", "UAH", "UGX", "USD", "UYU", "UZS", "VES",
    "VND", "VUV", "WST", "XAF", "XCD", "XCG", "XDR", "XOF", "XPF", "YER", "ZAR", "ZMW", "ZWG",
];

#[cfg(test)]
mod tests {
    use super::ISO_4217_CODES;

    /// Guards the hand-authored list against the authoring mistakes a reviewer
    /// can't easily eyeball: every code is exactly three ASCII uppercase letters,
    /// there are no duplicates, the list is ascending (emitters depend on it), and
    /// the count is in the plausible band for active ISO-4217 codes.
    #[test]
    fn iso_4217_codes_are_well_formed() {
        for code in ISO_4217_CODES {
            assert!(
                code.len() == 3 && code.bytes().all(|b| b.is_ascii_uppercase()),
                "not a 3-uppercase-letter code: {code:?}"
            );
        }
        for pair in ISO_4217_CODES.windows(2) {
            assert!(
                pair[0] < pair[1],
                "ISO_4217_CODES must be ascending + unique; {:?} !< {:?}",
                pair[0],
                pair[1]
            );
        }
        // ~150–180 active codes; a count far outside that band means a bulk
        // edit went wrong.
        assert!(
            (150..=185).contains(&ISO_4217_CODES.len()),
            "unexpected ISO_4217_CODES count: {}",
            ISO_4217_CODES.len()
        );
    }

    /// Shape checks (above) can't catch a *well-formed* error — a typo like `USB`
    /// for `USD`, a missing major currency, or an excluded code creeping back in.
    /// Lacking a vendored reference list (MIT-only policy), this spot-checks the
    /// content against curated anchors: the high-traffic codes most users will
    /// reach for MUST be present, and the codes the module scope deliberately
    /// EXCLUDES (precious metals, funds, test/no-currency `X` codes, and the
    /// decommissioned `ANG`/transitional `VED`) MUST stay out. It does not aim for
    /// full coverage — a differential test against an MIT reference set remains the
    /// future hardening — but it locks the documented scope decisions and the
    /// currencies whose silent breakage would be most damaging.
    #[test]
    fn iso_4217_codes_match_curated_anchors() {
        // Major transaction currencies a `Money` user is most likely to pass; a
        // typo or omission in any of these is the highest-impact failure.
        const MUST_INCLUDE: &[&str] = &[
            "USD", "EUR", "GBP", "JPY", "CNY", "CHF", "CAD", "AUD", "NZD", "HKD", "SGD", "INR",
            "BRL", "ZAR", "MXN", "SEK", "NOK", "DKK", "PLN", "TRY", "AED", "SAR", "KRW", "THB",
            // Supranational codes the module docs explicitly include.
            "XOF", "XAF", "XPF", "XCD", "XDR", "XCG",
        ];
        for code in MUST_INCLUDE {
            assert!(
                ISO_4217_CODES.contains(code),
                "expected active currency {code:?} is missing from ISO_4217_CODES"
            );
        }

        // Codes the module scope deliberately excludes: precious metals, funds,
        // test/no-currency `X` codes, and the retired `ANG` / transitional `VED`.
        const MUST_EXCLUDE: &[&str] = &[
            "XAU", "XAG", "XPT", "XPD", "XBA", "XBB", "XBC", "XBD", "XTS", "XXX", "ANG", "VED",
        ];
        for code in MUST_EXCLUDE {
            assert!(
                !ISO_4217_CODES.contains(code),
                "out-of-scope code {code:?} must not appear in ISO_4217_CODES"
            );
        }
    }
}
