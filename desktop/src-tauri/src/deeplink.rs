//! What the operating system handed us: a `deeptutor://` link or a file.
//!
//! Both arrive through the same channel — on macOS the deep-link plugin turns
//! `RunEvent::Opened` into an event carrying every URL the OS wanted us to
//! handle, and on Windows/Linux the second launch's command line carries either
//! form. One classifier keeps the two paths from drifting, and it is pure, so
//! the grammar is unit-tested instead of hand-checked on three platforms.
//!
//! Grammar for links (everything else is ignored, not "best effort"):
//!
//! ```text
//! deeptutor://chat/1f0c            -> /chat/1f0c
//! deeptutor://settings             -> /settings
//! deeptutor://                     -> /
//! deeptutor://learning/books?x=1   -> /learning/books?x=1
//! ```
//!
//! A link may only address a route *inside* the app. Each segment is checked
//! against that allow-list and refused if it is empty, encoded (`%`), contains a
//! slash or a backslash, or is `..`. Encoding is refused rather than decoded
//! because every route the app has is plain ASCII: `deeptutor://..%2F..%2Fetc`
//! has no legitimate caller, so it does not get a decode step to hide behind.
//! That is the whole security story for deep links — a hand-off must not be able
//! to aim the webview at anywhere but its own routes.

use std::path::PathBuf;

use tauri::Url;

/// Longest link this will consider; anything past it is dropped as junk.
const MAX_INPUT_LEN: usize = 4096;
const MAX_ROUTE_LEN: usize = 512;

/// One thing the UI should do when it is next in the foreground.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenRequest {
    /// Navigate to an in-app route.
    Route { route: String },
    /// Ingest a local file (file association, Dock drop, "Open with").
    File { path: PathBuf },
}

impl OpenRequest {
    /// Short label for logs.
    pub fn kind(&self) -> &'static str {
        match self {
            OpenRequest::Route { .. } => "route",
            OpenRequest::File { .. } => "file",
        }
    }
}

/// Classify a whole command line, pairing each request with the argument that
/// produced it so the log can quote what the OS actually sent.
pub fn classify_args(args: &[String]) -> Vec<(OpenRequest, String)> {
    args.iter()
        .skip(1)
        .filter(|argument| !argument.starts_with('-'))
        .filter_map(|argument| {
            classify_argument(argument).map(|request| (request, argument.clone()))
        })
        .collect()
}

/// Classify one URL the OS handed us.
pub fn classify_url(url: &Url) -> Option<OpenRequest> {
    match url.scheme() {
        "deeptutor" => route_from_url(url).map(|route| OpenRequest::Route { route }),
        "file" => url
            .to_file_path()
            .ok()
            .map(|path| OpenRequest::File { path }),
        _ => None,
    }
}

/// Classify one bare command-line argument (Windows/Linux second launch).
///
/// A file association passes a plain path there, while a URL handler passes the
/// link itself, so both shapes have to be understood.
pub fn classify_argument(argument: &str) -> Option<OpenRequest> {
    let argument = argument.trim();
    if argument.is_empty() || argument.len() > MAX_INPUT_LEN {
        return None;
    }
    // Windows and Linux both deliver deep links as an argument, and the
    // argument may be wrapped in quotes by the shell.
    let unquoted = argument.trim_matches('"');
    if unquoted.starts_with("deeptutor:") {
        return Url::parse(unquoted).ok().and_then(|url| classify_url(&url));
    }
    if looks_like_path(unquoted) {
        let path = PathBuf::from(unquoted);
        return path.is_absolute().then_some(OpenRequest::File { path });
    }
    None
}

fn looks_like_path(value: &str) -> bool {
    if value.starts_with('/') {
        return true;
    }
    // `C:\…` on a single Windows volume; `\\server\share` for a UNC path.
    let mut chars = value.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(letter), Some(':'), Some(separator)) => {
            letter.is_ascii_alphabetic() && (separator == '\\' || separator == '/')
        }
        (Some('\\'), Some('\\'), _) => true,
        _ => false,
    }
}

fn route_from_url(url: &Url) -> Option<String> {
    let mut segments: Vec<String> = Vec::new();
    // The authority doubles as the first segment: `deeptutor://chat/1f0c`.
    if let Some(host) = url.host_str() {
        if !host.is_empty() {
            push_segment(&mut segments, host)?;
        }
    }
    // An empty path yields no segments at all — and for a non-special scheme
    // like `deeptutor`, the url crate reports that as `None`, not as `[""]`.
    if let Some(path) = url.path_segments() {
        for segment in path {
            if segment.is_empty() {
                continue;
            }
            push_segment(&mut segments, segment)?;
        }
    }
    let mut route = String::from("/");
    route.push_str(&segments.join("/"));
    if let Some(query) = url.query() {
        // Queries only address in-app state (which panel, which tab), so the
        // same character policy applies.
        if !query.is_empty() {
            if query.len() > MAX_ROUTE_LEN || !query.chars().all(is_route_char) {
                return None;
            }
            route.push('?');
            route.push_str(query);
        }
    }
    if route.len() > MAX_ROUTE_LEN {
        return None;
    }
    Some(route)
}

/// Reject anything that is not a single, harmless path segment.
fn push_segment(segments: &mut Vec<String>, segment: &str) -> Option<()> {
    let clean = segment.trim();
    if clean.is_empty() || clean == "." || clean == ".." {
        return None;
    }
    if clean.contains('/') || clean.contains('\\') || clean.contains('\0') {
        return None;
    }
    if !clean.chars().all(is_route_char) {
        return None;
    }
    segments.push(clean.to_string());
    Some(())
}

/// Characters allowed in a deep link's path or query.
fn is_route_char(character: char) -> bool {
    character.is_ascii_alphanumeric()
        || matches!(
            character,
            '-' | '_'
                | '.'
                | '~'
                | '!'
                | '$'
                | '&'
                | '\''
                | '('
                | ')'
                | '*'
                | '+'
                | ','
                | ';'
                | '='
                | ':'
                | '@'
                | '['
                | ']'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).expect("test url")
    }

    fn route(raw: &str) -> Option<String> {
        match classify_url(&url(raw)) {
            Some(OpenRequest::Route { route }) => Some(route),
            _ => None,
        }
    }

    #[test]
    fn deep_links_map_onto_app_routes() {
        assert_eq!(
            route("deeptutor://chat/1f0c").as_deref(),
            Some("/chat/1f0c")
        );
        assert_eq!(route("deeptutor://settings").as_deref(), Some("/settings"));
        assert_eq!(
            route("deeptutor://learning/mastery/abc/sessions/xyz").as_deref(),
            Some("/learning/mastery/abc/sessions/xyz")
        );
        assert_eq!(
            route("deeptutor://co-writer?doc=42").as_deref(),
            Some("/co-writer?doc=42")
        );
    }

    #[test]
    fn trailing_and_duplicate_slashes_collapse() {
        assert_eq!(
            route("deeptutor://chat//1f0c/").as_deref(),
            Some("/chat/1f0c")
        );
        assert_eq!(route("deeptutor://").as_deref(), Some("/"));
    }

    #[test]
    fn traversal_and_foreign_schemes_are_refused() {
        // The url crate collapses dot segments (plain or percent-encoded)
        // before this code sees them, so a traversal cannot survive into a
        // route; the worst it can do is shorten one.
        assert_eq!(
            route("deeptutor://chat/%2e%2e/%2e%2e").as_deref(),
            Some("/chat")
        );
        // Percent-encoding never appears in a DeepTutor route, so a segment
        // that still carries one was not written for this app.
        assert_eq!(route("deeptutor://chat/%20"), None);
        assert_eq!(route("deeptutor://%2e%2e/etc/passwd"), None);
        assert_eq!(route("deeptutor://..%2F..%2Fetc/passwd"), None);
        assert_eq!(classify_url(&url("https://example.com/chat/1")), None);
        assert_eq!(classify_url(&url("tauri://localhost/index.html")), None);
    }

    #[test]
    fn file_urls_become_file_requests() {
        let request = classify_url(&url("file:///Users/someone/paper.pdf"));
        assert_eq!(
            request,
            Some(OpenRequest::File {
                path: PathBuf::from("/Users/someone/paper.pdf")
            })
        );
        assert_eq!(request.as_ref().map(OpenRequest::kind), Some("file"));
    }

    #[test]
    fn command_line_arguments_cover_links_and_paths() {
        assert_eq!(
            classify_argument("deeptutor://chat/9"),
            Some(OpenRequest::Route {
                route: "/chat/9".to_string()
            })
        );
        // Quoted by the Windows shell when the path contains spaces.
        assert_eq!(
            classify_argument("\"/Users/someone/My Paper.pdf\""),
            Some(OpenRequest::File {
                path: PathBuf::from("/Users/someone/My Paper.pdf")
            })
        );
        #[cfg(windows)]
        assert_eq!(
            classify_argument(r"C:\Users\someone\paper.pdf"),
            Some(OpenRequest::File {
                path: PathBuf::from(r"C:\Users\someone\paper.pdf")
            })
        );
        assert_eq!(classify_argument("--self-check"), None);
        assert_eq!(classify_argument("chat"), None);
    }

    #[test]
    fn classify_args_skips_the_binary_and_flags() {
        let args: Vec<String> = ["deeptutor-desktop", "--flag", "deeptutor://settings"]
            .iter()
            .map(|value| value.to_string())
            .collect();
        assert_eq!(
            classify_args(&args),
            vec![(
                OpenRequest::Route {
                    route: "/settings".to_string()
                },
                "deeptutor://settings".to_string()
            )]
        );
    }
}
