//! Human-readable sizes, counts and percentages in Ukrainian and English.
//!
//! * [`size`]: binary units (1 KB = 1024 B) with three significant digits:
//!   `1.23 GB`, `12.3 GB`, `123 GB`. Below 1000 bytes the exact number of
//!   bytes is shown; a value that would round to 1000 or more in its unit is
//!   shown in the next unit instead (`1000 B` -> `0.98 KB`), so at most three
//!   digits are ever printed. Units go up to EB (u64 max = 16 EB).
//!   Ukrainian uses a decimal comma and `Б, КБ, МБ, ГБ, ТБ, ПБ, ЕБ`; English
//!   a decimal point and `B, KB, MB, GB, TB, PB, EB`. The number and the unit
//!   are separated by a plain space.
//! * [`count`]: thousands grouped from four digits on, with a no-break
//!   space U+00A0 in Ukrainian (`1 234 567`, as Windows groups for uk-UA)
//!   and a comma in English (`1,234,567`). Not the narrow no-break space
//!   U+202F: in Segoe UI Variable it is no wider than the side bearings of
//!   a "1" or a "7", so `1 137 193` read as `1137193`.
//! * [`percent`]: takes a *ratio* (0.123 = 12.3 %), one decimal; Ukrainian
//!   `12,3 %` (plain space before the sign), English `12.3%`. Positive values
//!   under 0.05 % print as `<0.1%` / `<0,1 %`; NaN and infinities as 0.

use crate::Lang;

const UNITS_EN: [&str; 7] = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
const UNITS_UK: [&str; 7] = ["Б", "КБ", "МБ", "ГБ", "ТБ", "ПБ", "ЕБ"];

/// Formats a byte count, e.g. `1.23 GB` / `1,23 ГБ`.
pub fn size(bytes: u64, lang: Lang) -> String {
    let units = match lang {
        Lang::En => &UNITS_EN,
        Lang::Uk => &UNITS_UK,
    };
    if bytes < 1000 {
        return format!("{bytes} {}", units[0]);
    }
    let mut v = bytes as f64;
    let mut unit = 0;
    // 999.5 and above would print as "1000" or more.
    while v >= 999.5 && unit < units.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    let decimals = if v < 9.995 {
        2
    } else if v < 99.95 {
        1
    } else {
        0
    };
    let number = format!("{v:.decimals$}");
    format!("{} {}", localize_decimal(number, lang), units[unit])
}

/// Formats an integer with grouped thousands, e.g. `1,234` / `1 234`.
pub fn count(n: u64, lang: Lang) -> String {
    let sep = match lang {
        Lang::En => ",",
        Lang::Uk => "\u{A0}",
    };
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 * sep.len());
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push_str(sep);
        }
        out.push(ch);
    }
    out
}

/// Formats a ratio as a percentage, e.g. `0.123` -> `12.3%` / `12,3 %`.
pub fn percent(ratio: f64, lang: Lang) -> String {
    let p = if ratio.is_finite() {
        ratio * 100.0
    } else {
        0.0
    };
    let number = if p > 0.0 && p < 0.05 {
        "<0.1".to_string()
    } else {
        format!("{p:.1}")
    };
    match lang {
        Lang::En => format!("{number}%"),
        Lang::Uk => format!("{} %", localize_decimal(number, lang)),
    }
}

fn localize_decimal(number: String, lang: Lang) -> String {
    match lang {
        Lang::En => number,
        Lang::Uk => number.replace('.', ","),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KB: u64 = 1024;
    const MB: u64 = KB * KB;
    const GB: u64 = MB * KB;
    const TB: u64 = GB * KB;

    #[test]
    fn sizes_en() {
        let cases = [
            (0, "0 B"),
            (1, "1 B"),
            (999, "999 B"),
            (1000, "0.98 KB"),
            (1023, "1.00 KB"),
            (KB, "1.00 KB"),
            (1536, "1.50 KB"),
            (10 * KB, "10.0 KB"),
            (10 * KB - 1, "10.0 KB"),
            (99 * KB, "99.0 KB"),
            (100 * KB - 1, "100 KB"),
            (123 * KB + 400, "123 KB"),
            (999 * KB, "999 KB"),
            (1000 * KB, "0.98 MB"),
            (MB - 1, "1.00 MB"),
            (GB + GB / 4, "1.25 GB"),
            (1_320_702_444, "1.23 GB"),
            (500 * GB, "500 GB"),
            (2 * TB, "2.00 TB"),
            (5 * 1024 * TB, "5.00 PB"),
            (u64::MAX, "16.0 EB"),
        ];
        for (b, want) in cases {
            assert_eq!(size(b, Lang::En), want, "{b}");
        }
    }

    #[test]
    fn sizes_uk() {
        assert_eq!(size(0, Lang::Uk), "0 Б");
        assert_eq!(size(512, Lang::Uk), "512 Б");
        assert_eq!(size(1536, Lang::Uk), "1,50 КБ");
        assert_eq!(size(1_320_702_444, Lang::Uk), "1,23 ГБ");
        assert_eq!(size(12 * MB + MB / 3, Lang::Uk), "12,3 МБ");
        assert_eq!(size(300 * GB, Lang::Uk), "300 ГБ");
        assert_eq!(size(3 * TB, Lang::Uk), "3,00 ТБ");
    }

    #[test]
    fn never_more_than_three_digits() {
        let mut b = 1u64;
        while b < u64::MAX / 3 {
            for v in [b, b + b / 7, b * 2 - 1] {
                let s = size(v, Lang::En);
                let digits = s.chars().filter(char::is_ascii_digit).count();
                assert!(digits <= 3 || v < 1000, "{v} -> {s}");
            }
            b = b * 3 / 2 + 1;
        }
    }

    #[test]
    fn counts() {
        assert_eq!(count(0, Lang::En), "0");
        assert_eq!(count(999, Lang::En), "999");
        assert_eq!(count(1000, Lang::En), "1,000");
        assert_eq!(count(1_234_567, Lang::En), "1,234,567");
        assert_eq!(count(1_234_567, Lang::Uk), "1\u{A0}234\u{A0}567");
        assert_eq!(count(12_345, Lang::Uk), "12\u{A0}345");
        assert_eq!(count(100_000, Lang::En), "100,000");
        assert_eq!(count(u64::MAX, Lang::En), "18,446,744,073,709,551,615");
    }

    #[test]
    fn percents() {
        assert_eq!(percent(0.123, Lang::En), "12.3%");
        assert_eq!(percent(0.123, Lang::Uk), "12,3 %");
        assert_eq!(percent(0.0, Lang::En), "0.0%");
        assert_eq!(percent(1.0, Lang::Uk), "100,0 %");
        assert_eq!(percent(0.0004, Lang::En), "<0.1%");
        assert_eq!(percent(0.0004, Lang::Uk), "<0,1 %");
        assert_eq!(percent(0.0006, Lang::En), "0.1%");
        assert_eq!(percent(f64::NAN, Lang::En), "0.0%");
        assert_eq!(percent(f64::INFINITY, Lang::Uk), "0,0 %");
        assert_eq!(percent(0.99999, Lang::En), "100.0%");
    }
}
