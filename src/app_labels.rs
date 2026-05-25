const APP_ID_LABELS: &[(&str, &str)] =
    &[("org.telegram.desktop", "telegram"), ("vesktop", "discord")];

const TITLE_LABELS: &[(&str, &str)] = &[("microsoft teams", "teams")];

const TERMINAL_APP_ID_PARTS: &[&str] = &["ghostty", "terminal", "alacritty"];

const TITLE_APP_IDS: &[&str] = &["desktop"];
const TITLE_APP_ID_PARTS: &[&str] = &["electron"];

pub fn simplify_label(title: &str, app_id: &str) -> String {
    let app_id_lower = app_id.to_ascii_lowercase();
    let title_lower = title.to_ascii_lowercase();

    if let Some(label) = exact_label(&app_id_lower, APP_ID_LABELS) {
        return label.to_string();
    }
    if let Some(label) = contains_label(&title_lower, TITLE_LABELS) {
        return label.to_string();
    }
    if contains_any(&app_id_lower, TERMINAL_APP_ID_PARTS) {
        return terminal_label(title);
    }
    if exact_any(&app_id_lower, TITLE_APP_IDS) || contains_any(&app_id_lower, TITLE_APP_ID_PARTS) {
        return title_label_or_app_id(title, app_id);
    }

    app_id_fallback(app_id)
}

fn exact_label<'a>(value: &str, mappings: &'a [(&str, &str)]) -> Option<&'a str> {
    mappings
        .iter()
        .find_map(|(pattern, label)| (*pattern == value).then_some(*label))
}

fn contains_label<'a>(value: &str, mappings: &'a [(&str, &str)]) -> Option<&'a str> {
    mappings
        .iter()
        .find_map(|(pattern, label)| value.contains(pattern).then_some(*label))
}

fn exact_any(value: &str, patterns: &[&str]) -> bool {
    patterns.contains(&value)
}

fn contains_any(value: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|pattern| value.contains(pattern))
}

fn terminal_label(title: &str) -> String {
    let cleaned = title
        .trim_start_matches(|c: char| !c.is_alphanumeric() && c != '~' && c != '/')
        .trim();
    if cleaned.starts_with('~') {
        let last = cleaned.split('/').next_back().unwrap_or(cleaned);
        format!("~/{}", last)
    } else if cleaned.starts_with('/') {
        cleaned
            .split('/')
            .next_back()
            .unwrap_or(cleaned)
            .to_string()
    } else {
        cleaned.to_string()
    }
}

fn title_label_or_app_id(title: &str, app_id: &str) -> String {
    let title = title.trim();
    if title.is_empty() {
        app_id_fallback(app_id)
    } else {
        title.to_ascii_lowercase()
    }
}

fn app_id_fallback(app_id: &str) -> String {
    app_id.split('.').next_back().unwrap_or(app_id).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn electron_labels_are_taken_from_window_title() {
        assert_eq!(simplify_label("Linear", "electron"), "linear");
        assert_eq!(
            simplify_label("  Slack — workspace  ", "com.github.Electron"),
            "slack — workspace",
        );
    }

    #[test]
    fn desktop_labels_are_taken_from_window_title() {
        assert_eq!(simplify_label("Telegram", "desktop"), "telegram");
    }

    #[test]
    fn known_app_ids_use_simple_labels() {
        assert_eq!(
            simplify_label("Some chat", "org.telegram.desktop"),
            "telegram"
        );
        assert_eq!(simplify_label("General", "vesktop"), "discord");
    }

    #[test]
    fn known_window_titles_use_simple_labels() {
        assert_eq!(
            simplify_label(
                "(8) Calendar | Townhall (External) | Microsoft Teams",
                "electron"
            ),
            "teams"
        );
    }

    #[test]
    fn terminal_labels_are_simplified_from_title() {
        assert_eq!(
            simplify_label("/home/thrawny/code/agent-switch", "ghostty"),
            "agent-switch"
        );
        assert_eq!(
            simplify_label("~/code/agent-switch", "Alacritty"),
            "~/agent-switch"
        );
    }
}
