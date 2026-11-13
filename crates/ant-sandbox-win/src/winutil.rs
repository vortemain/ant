//! Safe helpers for building Win32-wide-string inputs.
//!
//! Audit contract: these functions only allocate and transform owned Rust
//! strings into UTF-16 buffers; no raw pointer outlives the call they feed.

/// Encode a string as UTF-16 with a trailing null terminator, as Win32
/// functions expect.
pub(crate) fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Quote one argument following the standard Windows command-line rules
/// (backslashes before a quote are doubled, embedded quotes are escaped).
pub(crate) fn quote_argument(arg: &str) -> String {
    if !arg.is_empty()
        && !arg
            .chars()
            .any(|c| c == ' ' || c == '\t' || c == '"' || c == '\0' || c == '\n')
    {
        return arg.to_owned();
    }
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // Backslashes immediately before a quote must be doubled,
                // plus one escape for the quote itself.
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                backslashes = 0;
                quoted.push('"');
            }
            _ => {
                quoted.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                quoted.push(c);
            }
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

/// Build a full command line from argv (argv[0] is quoted like any other).
pub(crate) fn argv_to_command_line(argv: &[String]) -> String {
    argv.iter().map(|arg| quote_argument(arg)).collect::<Vec<_>>().join(" ")
}

/// Build a null-separated UTF-16 environment block sorted case-insensitively
/// by key then exactly by key (deterministic; matches Win32 expectations).
pub(crate) fn make_env_block(environment: &[(String, String)]) -> Vec<u16> {
    let mut items: Vec<&(String, String)> = environment.iter().collect();
    items.sort_by(|a, b| {
        a.0.to_uppercase()
            .cmp(&b.0.to_uppercase())
            .then_with(|| a.0.cmp(&b.0))
    });
    let mut block: Vec<u16> = Vec::new();
    for (key, value) in items {
        let mut entry = to_wide(&format!("{key}={value}"));
        entry.pop();
        block.extend_from_slice(&entry);
        block.push(0);
    }
    block.push(0);
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_arguments_are_not_quoted() {
        assert_eq!(quote_argument("cmd"), "cmd");
        assert_eq!(quote_argument("/C"), "/C");
    }

    #[test]
    fn complex_arguments_follow_windows_quoting() {
        assert_eq!(quote_argument("a b"), "\"a b\"");
        assert_eq!(quote_argument("say \"hi\""), "\"say \\\"hi\\\"\"");
        // No special characters (only backslashes): the argument is left
        // unquoted. Backslashes only need doubling when they precede a quote
        // inside a quoted argument.
        assert_eq!(quote_argument("trail\\"), "trail\\");
        assert_eq!(quote_argument("pre\\\"mid"), "\"pre\\\\\\\"mid\"");
    }

    #[test]
    fn env_block_is_sorted_and_terminated() {
        let block = make_env_block(&[
            ("PATH".to_owned(), "C:\\x".to_owned()),
            ("TMP".to_owned(), "C:\\t".to_owned()),
            ("path".to_owned(), "lower".to_owned()),
        ]);
        let text: String = block
            .split(|&u| u == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf16(part).expect("utf16"))
            .collect::<Vec<_>>()
            .join("|");
        // Case-insensitive sort (keys "PATH" and "path" compare equal); the
        // stable sort keeps their original relative order.
        assert_eq!(text, "PATH=C:\\x|path=lower|TMP=C:\\t");
        assert_eq!(block.last(), Some(&0), "double null termination");
    }

    #[test]
    fn command_line_joins_quoted_args() {
        assert_eq!(
            argv_to_command_line(&["cmd".to_owned(), "/C".to_owned(), "echo hi".to_owned()]),
            "cmd /C \"echo hi\""
        );
    }
}
