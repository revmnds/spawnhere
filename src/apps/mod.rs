use std::sync::mpsc;
use std::thread::{self, JoinHandle};

mod icon;
pub use icon::IconCache;

#[derive(Clone, Debug)]
pub struct App {
    pub name: String,
    pub exec: String,
    pub icon: Option<String>,
}

/// Kicks off a background scan of `.desktop` files. The receiver yields the
/// full list once when scanning finishes (~50-200ms typical).
pub fn discover_async() -> (mpsc::Receiver<Vec<App>>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let apps = discover_sync();
        let _ = tx.send(apps);
    });
    (rx, handle)
}

fn discover_sync() -> Vec<App> {
    use freedesktop_desktop_entry::{default_paths, DesktopEntry, Iter};

    let locales = freedesktop_desktop_entry::get_languages_from_env();
    let mut out: Vec<App> = Vec::new();

    for path in Iter::new(default_paths()) {
        let bytes = match std::fs::read_to_string(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let entry = match DesktopEntry::from_str(&path, &bytes, Some(&locales)) {
            Ok(e) => e,
            Err(_) => continue,
        };

        if entry.no_display() {
            continue;
        }
        if entry.desktop_entry("Hidden").map(|v| v == "true").unwrap_or(false) {
            continue;
        }
        if entry.type_().map(|t| t != "Application").unwrap_or(false) {
            continue;
        }

        let raw_exec = match entry.exec() {
            Some(e) => e,
            None => continue,
        };
        let exec = sanitize_exec(raw_exec);
        if exec.trim().is_empty() {
            continue;
        }

        let name = entry
            .name(&locales)
            .map(|s| s.to_string())
            .unwrap_or_else(|| path.file_stem().and_then(|s| s.to_str()).unwrap_or("?").to_string());

        let icon = entry.icon().map(|s| s.to_string());

        out.push(App { name, exec, icon });
    }

    // De-duplicate by exec (multiple .desktop files for same binary).
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out.dedup_by(|a, b| a.exec == b.exec);
    out
}

/// Strip Exec field codes per freedesktop spec: %f %F %u %U %d %D %n %N %i %c %k %v %m.
fn sanitize_exec(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            if let Some(&next) = chars.peek() {
                match next {
                    'f' | 'F' | 'u' | 'U' | 'd' | 'D' | 'n' | 'N' | 'i' | 'c' | 'k' | 'v' | 'm' => {
                        chars.next();
                        continue;
                    }
                    '%' => {
                        out.push('%');
                        chars.next();
                        continue;
                    }
                    _ => {}
                }
            }
        }
        out.push(c);
    }
    // Collapse multiple spaces left behind by stripped codes.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_field_codes() {
        assert_eq!(sanitize_exec("firefox %U"), "firefox");
        assert_eq!(sanitize_exec("foo --bar %f"), "foo --bar");
        assert_eq!(sanitize_exec("env VAR=1 cmd %F %i"), "env VAR=1 cmd");
    }

    #[test]
    fn sanitize_preserves_literal_percent() {
        assert_eq!(sanitize_exec("foo %% bar"), "foo % bar");
    }

    #[test]
    fn sanitize_handles_no_codes() {
        assert_eq!(sanitize_exec("kitty"), "kitty");
    }
}
