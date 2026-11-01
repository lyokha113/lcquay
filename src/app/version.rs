use time::OffsetDateTime;

/// `MAJOR.MINOR.PATCH[-suffix]`; the suffix is everything after the first `-`, kept with its `-`.
fn parse(tag: &str) -> Option<((u64, u64, u64), &str)> {
    let (core, suffix) = tag.find('-').map_or((tag, ""), |i| tag.split_at(i));
    let mut parts = core.split('.').map(|p| {
        // u64::from_str accepts a leading `+`; require plain digits.
        (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())).then(|| p.parse().ok())?
    });
    let v = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some((v, suffix))
}

/// Next version for the push dialog (§7.5): the suffix group of the most recently pushed parseable
/// tag (highest version overall when none has `pushed_at`), its highest version with patch + 1.
pub fn suggest(tags: &[(&str, Option<OffsetDateTime>)]) -> Option<String> {
    let parsed: Vec<_> = tags
        .iter()
        .filter_map(|&(t, at)| parse(t).map(|(v, s)| (at, v, s)))
        .collect();
    let (_, _, suffix) = parsed
        .iter()
        .filter(|(at, ..)| at.is_some())
        .max_by_key(|(at, v, _)| (*at, *v))
        .or_else(|| parsed.iter().max_by_key(|(_, v, _)| *v))?;
    let (major, minor, patch) = parsed
        .iter()
        .filter(|e| e.2 == *suffix)
        .map(|e| e.1)
        .max()?;
    Some(format!("{major}.{minor}.{}{suffix}", patch.checked_add(1)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const T1: Option<OffsetDateTime> = Some(datetime!(2026-01-01 0:00 UTC));
    const T2: Option<OffsetDateTime> = Some(datetime!(2026-02-01 0:00 UTC));
    const T3: Option<OffsetDateTime> = Some(datetime!(2026-03-01 0:00 UTC));

    #[test]
    fn suggest_bumps_patch_same_suffix() {
        assert_eq!(
            suggest(&[("0.2.91-dev", T1)]).as_deref(),
            Some("0.2.92-dev")
        );
    }

    #[test]
    fn suggest_numeric_not_lexical() {
        let tags = [("0.1.9-dev", T1), ("0.1.81-dev", T2)];
        assert_eq!(suggest(&tags).as_deref(), Some("0.1.82-dev"));
    }

    #[test]
    fn suggest_group_of_most_recent_push() {
        let tags = [("1.0.0", T1), ("0.3.5-dev", T2)];
        assert_eq!(suggest(&tags).as_deref(), Some("0.3.6-dev"));
    }

    #[test]
    fn suggest_without_pushed_uses_highest_overall() {
        let tags = [("1.2.3", None), ("0.9.9-rc", None)];
        assert_eq!(suggest(&tags).as_deref(), Some("1.2.4"));
    }

    #[test]
    fn suggest_ignores_unparseable() {
        assert_eq!(
            suggest(&[("latest", T2), ("0.1.0", T1)]).as_deref(),
            Some("0.1.1")
        );
        for bad in [
            "latest", "v1.2.3", "1.2", "1.2.3.4", "1.02.x", "+1.2.3", "1..3", "",
        ] {
            assert_eq!(suggest(&[(bad, T1)]), None, "{bad}");
        }
        assert_eq!(suggest(&[]), None);
        assert_eq!(suggest(&[("1.02.3", None)]).as_deref(), Some("1.2.4"));
    }

    #[test]
    fn suggest_suffix_is_whole_rest() {
        assert_eq!(
            suggest(&[("1.2.3-rc.1", T1)]).as_deref(),
            Some("1.2.4-rc.1")
        );
    }

    #[test]
    fn suggest_two_stage_selection() {
        let tags = [("0.9.0-dev", T1), ("0.2.1-dev", T3), ("5.0.0", T2)];
        assert_eq!(suggest(&tags).as_deref(), Some("0.9.1-dev"));
    }

    #[test]
    fn suggest_patch_overflow() {
        assert_eq!(suggest(&[("1.2.18446744073709551615", T1)]), None);
    }
}
