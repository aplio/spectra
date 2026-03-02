use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachTarget {
    pub session_token: String,
    pub window: Option<usize>,
    pub pane: Option<usize>,
    #[serde(default)]
    pub pane_is_index: bool,
}

impl AttachTarget {
    pub fn parse(raw: &str) -> Result<Self, String> {
        raw.parse()
    }
}

impl FromStr for AttachTarget {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let target = raw.trim();
        if target.is_empty() {
            return Err("target is empty".to_string());
        }

        let mut session_and_rest = target.splitn(2, ':');
        let session_token = session_and_rest
            .next()
            .expect("splitn always yields the first segment")
            .trim();
        if session_token.is_empty() {
            return Err("missing session segment".to_string());
        }

        let Some(rest_raw) = session_and_rest.next() else {
            return Ok(Self {
                session_token: session_token.to_string(),
                window: None,
                pane: None,
                pane_is_index: false,
            });
        };

        if rest_raw.contains(':') {
            return Err("target has too many ':' separators".to_string());
        }

        let rest = rest_raw.trim();
        if rest.is_empty() {
            return Err("missing window segment".to_string());
        }

        let mut window_and_pane = rest.split('.');
        let window_token = window_and_pane
            .next()
            .expect("split always yields the first segment")
            .trim();
        let pane_token = window_and_pane.next().map(str::trim);
        if window_and_pane.next().is_some() {
            return Err("target has too many '.' separators".to_string());
        }

        let window = parse_numeric_component(window_token, ComponentKind::Window)?;
        let pane_selector = pane_token.map(parse_pane_component).transpose()?;
        let (pane, pane_is_index) = pane_selector
            .map(|(value, is_index)| (Some(value), is_index))
            .unwrap_or((None, false));

        Ok(Self {
            session_token: session_token.to_string(),
            window: Some(window),
            pane,
            pane_is_index,
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum ComponentKind {
    Window,
    Pane,
}

impl ComponentKind {
    fn label(self) -> &'static str {
        match self {
            Self::Window => "window",
            Self::Pane => "pane",
        }
    }

    fn prefix(self) -> char {
        match self {
            Self::Window => 'w',
            Self::Pane => 'p',
        }
    }
}

fn parse_numeric_component(token: &str, kind: ComponentKind) -> Result<usize, String> {
    if token.is_empty() {
        return Err(format!("missing {} segment", kind.label()));
    }

    let prefixed_lower = kind.prefix();
    let prefixed_upper = prefixed_lower.to_ascii_uppercase();
    let numeric = if let Some(stripped) = token.strip_prefix(prefixed_lower) {
        stripped
    } else if let Some(stripped) = token.strip_prefix(prefixed_upper) {
        stripped
    } else {
        token
    };

    if numeric.is_empty() {
        return Err(format!("missing {} number", kind.label()));
    }
    if !numeric.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(format!("invalid {} segment `{token}`", kind.label()));
    }
    let value = numeric
        .parse::<usize>()
        .map_err(|_| format!("invalid {} number `{token}`", kind.label()))?;
    if value == 0 {
        return Err(format!("{} must be >= 1", kind.label()));
    }
    Ok(value)
}

fn parse_pane_component(token: &str) -> Result<(usize, bool), String> {
    if let Some(raw) = token.strip_prefix('i').or_else(|| token.strip_prefix('I')) {
        let value = parse_required_positive_number(raw, token, "pane index")?;
        return Ok((value, true));
    }
    parse_numeric_component(token, ComponentKind::Pane).map(|value| (value, false))
}

fn parse_required_positive_number(raw: &str, token: &str, label: &str) -> Result<usize, String> {
    if raw.is_empty() {
        return Err(format!("missing {label} number"));
    }
    if !raw.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(format!("invalid {label} segment `{token}`"));
    }
    let value = raw
        .parse::<usize>()
        .map_err(|_| format!("invalid {label} number `{token}`"))?;
    if value == 0 {
        return Err(format!("{label} must be >= 1"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::AttachTarget;

    #[test]
    fn parses_valid_targets() {
        let cases = [
            ("main", "main", None, None, false),
            ("main-2:w1", "main-2", Some(1), None, false),
            ("s2:3.4", "s2", Some(3), Some(4), false),
            ("dev:w10.p2", "dev", Some(10), Some(2), false),
            ("DEV:W7.P8", "DEV", Some(7), Some(8), false),
            ("dev:w2.i1", "dev", Some(2), Some(1), true),
            ("dev:2.I3", "dev", Some(2), Some(3), true),
        ];

        for (raw, session_token, window, pane, pane_is_index) in cases {
            let parsed = AttachTarget::parse(raw).expect("parse target");
            assert_eq!(parsed.session_token, session_token);
            assert_eq!(parsed.window, window);
            assert_eq!(parsed.pane, pane);
            assert_eq!(parsed.pane_is_index, pane_is_index);
        }
    }

    #[test]
    fn rejects_malformed_targets() {
        let cases = [
            "",
            "   ",
            ":1",
            "main:",
            "main:.1",
            "main:1.",
            "main:1.2.3",
            "main::1",
            "main:0",
            "main:1.0",
            "main:w",
            "main:p1",
            "main:1.p",
            "main:1.w2",
            "main:one",
            "main:1.two",
            "main:1.i0",
            "main:1.i",
            "main:1.ix",
        ];

        for raw in cases {
            assert!(
                AttachTarget::parse(raw).is_err(),
                "expected parse failure for {raw:?}"
            );
        }
    }
}
