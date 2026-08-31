use std::{
    fs,
    path::{Path, PathBuf},
};

use rustedoutclient::{
    fallback::{FallbackError, FallbackPreferences, FallbackSession, TigerVncFallback},
    runtime::RuntimeDir,
    ssh::TrustedSshProxy,
};

const APPROVED_RELAY: &str = "src/fallback/relay.rs";
const APPROVED_VIEWER_ENDPOINT: &str = "src/fallback/mod.rs";
const APPROVED_BIND: &str = "TcpListener::bind((Ipv4Addr::LOCALHOST, 0))";

// Compiling this helper pins the only production fallback constructor to the
// verified SSH transport, private runtime, configured path, and two display
// preferences. It is deliberately never called by this synthetic test suite.
#[allow(dead_code)]
async fn production_open_surface(
    proxy: TrustedSshProxy,
    runtime: &RuntimeDir,
    viewer: &Path,
    preferences: FallbackPreferences,
) -> Result<FallbackSession, FallbackError> {
    TigerVncFallback::open(proxy, runtime, viewer, preferences).await
}

fn source(path: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap()
}

fn rust_sources(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_owned()];
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    files.sort();
    files
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RustTokenKind<'a> {
    Ident(&'a str),
    Punct(char),
    Literal,
}

#[derive(Clone, Copy, Debug)]
struct RustToken<'a> {
    kind: RustTokenKind<'a>,
    start: usize,
    end: usize,
}

impl RustToken<'_> {
    fn is_ident(&self, expected: &str) -> bool {
        matches!(self.kind, RustTokenKind::Ident(actual) if actual == expected)
    }

    fn is_punct(&self, expected: char) -> bool {
        self.kind == RustTokenKind::Punct(expected)
    }
}

fn is_ident_start(character: char) -> bool {
    character == '_' || character.is_alphabetic()
}

fn is_ident_continue(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

fn raw_literal_end(bytes: &[u8], start: usize) -> Result<Option<usize>, String> {
    let mut cursor = if bytes.get(start) == Some(&b'r') {
        start + 1
    } else if matches!(bytes.get(start), Some(b'b' | b'c')) && bytes.get(start + 1) == Some(&b'r') {
        start + 2
    } else {
        return Ok(None);
    };
    let hash_start = cursor;
    while bytes.get(cursor) == Some(&b'#') {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'"') {
        return Ok(None);
    }
    let hash_count = cursor - hash_start;
    cursor += 1;
    while cursor < bytes.len() {
        if bytes[cursor] == b'"'
            && bytes
                .get(cursor + 1..cursor + 1 + hash_count)
                .is_some_and(|hashes| hashes.iter().all(|byte| *byte == b'#'))
        {
            return Ok(Some(cursor + 1 + hash_count));
        }
        cursor += 1;
    }
    Err("unterminated raw string literal".to_owned())
}

fn quoted_literal_end(bytes: &[u8], quote: usize) -> Result<usize, String> {
    let mut cursor = quote + 1;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => {
                cursor += 1;
                if cursor == bytes.len() {
                    return Err("unterminated escape in string literal".to_owned());
                }
                cursor += 1;
            }
            b'"' => return Ok(cursor + 1),
            _ => cursor += 1,
        }
    }
    Err("unterminated string literal".to_owned())
}

fn char_literal_end(text: &str, quote: usize) -> Result<Option<usize>, String> {
    let bytes = text.as_bytes();
    let mut cursor = quote + 1;
    let Some(first) = bytes.get(cursor).copied() else {
        return Ok(None);
    };

    if first == b'\\' {
        cursor += 1;
        let Some(escape) = bytes.get(cursor).copied() else {
            return Err("unterminated character escape".to_owned());
        };
        match escape {
            b'u' => {
                cursor += 1;
                if bytes.get(cursor) != Some(&b'{') {
                    return Err("unsupported unicode character escape".to_owned());
                }
                cursor += 1;
                while bytes.get(cursor).is_some_and(|byte| *byte != b'}') {
                    cursor += 1;
                }
                if bytes.get(cursor) != Some(&b'}') {
                    return Err("unterminated unicode character escape".to_owned());
                }
                cursor += 1;
            }
            b'x' => {
                cursor += 3;
                if cursor > bytes.len() {
                    return Err("unterminated hexadecimal character escape".to_owned());
                }
            }
            _ => cursor += 1,
        }
    } else {
        let character = text[cursor..]
            .chars()
            .next()
            .expect("cursor is below the text length");
        if matches!(character, '\'' | '\r' | '\n') {
            return Ok(None);
        }
        cursor += character.len_utf8();
    }

    Ok((bytes.get(cursor) == Some(&b'\'')).then_some(cursor + 1))
}

fn rust_tokens(text: &str) -> Result<Vec<RustToken<'_>>, String> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut cursor = 0;

    while cursor < bytes.len() {
        let character = text[cursor..]
            .chars()
            .next()
            .expect("cursor is below the text length");
        if character.is_whitespace() {
            cursor += character.len_utf8();
            continue;
        }

        if bytes.get(cursor..cursor + 2) == Some(b"//") {
            cursor += 2;
            while bytes.get(cursor).is_some_and(|byte| *byte != b'\n') {
                cursor += 1;
            }
            continue;
        }
        if bytes.get(cursor..cursor + 2) == Some(b"/*") {
            let mut depth = 1_usize;
            cursor += 2;
            while cursor < bytes.len() && depth > 0 {
                if bytes.get(cursor..cursor + 2) == Some(b"/*") {
                    depth += 1;
                    cursor += 2;
                } else if bytes.get(cursor..cursor + 2) == Some(b"*/") {
                    depth -= 1;
                    cursor += 2;
                } else {
                    cursor += 1;
                }
            }
            if depth != 0 {
                return Err("unterminated block comment".to_owned());
            }
            continue;
        }

        if let Some(end) = raw_literal_end(bytes, cursor)? {
            tokens.push(RustToken {
                kind: RustTokenKind::Literal,
                start: cursor,
                end,
            });
            cursor = end;
            continue;
        }

        let quoted_prefix = if bytes.get(cursor) == Some(&b'"') {
            Some(0)
        } else if matches!(bytes.get(cursor), Some(b'b' | b'c'))
            && bytes.get(cursor + 1) == Some(&b'"')
        {
            Some(1)
        } else {
            None
        };
        if let Some(prefix_length) = quoted_prefix {
            let end = quoted_literal_end(bytes, cursor + prefix_length)?;
            tokens.push(RustToken {
                kind: RustTokenKind::Literal,
                start: cursor,
                end,
            });
            cursor = end;
            continue;
        }

        if bytes.get(cursor) == Some(&b'b') && bytes.get(cursor + 1) == Some(&b'\'') {
            let end = char_literal_end(text, cursor + 1)?
                .ok_or_else(|| "unterminated byte-character literal".to_owned())?;
            tokens.push(RustToken {
                kind: RustTokenKind::Literal,
                start: cursor,
                end,
            });
            cursor = end;
            continue;
        }
        if bytes.get(cursor) == Some(&b'\'') {
            if let Some(end) = char_literal_end(text, cursor)? {
                tokens.push(RustToken {
                    kind: RustTokenKind::Literal,
                    start: cursor,
                    end,
                });
                cursor = end;
                continue;
            }
            let lifetime_start = cursor + 1;
            let lifetime = text
                .get(lifetime_start..)
                .and_then(|suffix| suffix.chars().next());
            if !lifetime.is_some_and(is_ident_start) {
                return Err("unterminated or unsupported character literal".to_owned());
            }
        }

        if bytes.get(cursor..cursor + 2) == Some(b"r#") {
            let ident_start = cursor + 2;
            if text[ident_start..]
                .chars()
                .next()
                .is_some_and(is_ident_start)
            {
                let mut end = ident_start;
                while let Some(next) = text[end..].chars().next() {
                    if !is_ident_continue(next) {
                        break;
                    }
                    end += next.len_utf8();
                }
                tokens.push(RustToken {
                    kind: RustTokenKind::Ident(&text[ident_start..end]),
                    start: cursor,
                    end,
                });
                cursor = end;
                continue;
            }
        }

        if is_ident_start(character) {
            let start = cursor;
            cursor += character.len_utf8();
            while let Some(next) = text[cursor..].chars().next() {
                if !is_ident_continue(next) {
                    break;
                }
                cursor += next.len_utf8();
            }
            tokens.push(RustToken {
                kind: RustTokenKind::Ident(&text[start..cursor]),
                start,
                end: cursor,
            });
            continue;
        }

        tokens.push(RustToken {
            kind: RustTokenKind::Punct(character),
            start: cursor,
            end: cursor + character.len_utf8(),
        });
        cursor += character.len_utf8();
    }

    Ok(tokens)
}

fn production_portion<'a>(relative_path: &Path, text: &'a str) -> &'a str {
    const TEST_MODULE_MARKER: &str = "\n#[cfg(test)]\nmod tests {";

    let tokens = rust_tokens(text).unwrap_or_else(|error| {
        panic!(
            "{} could not be inspected lexically: {error}",
            relative_path.display()
        )
    });
    let test_module_openings = tokens
        .windows(3)
        .enumerate()
        .filter_map(|(index, window)| {
            (window[0].is_ident("mod") && window[1].is_ident("tests") && window[2].is_punct('{'))
                .then_some(index + 2)
        })
        .collect::<Vec<_>>();
    let exact_markers = text.matches(TEST_MODULE_MARKER).count();
    if test_module_openings.is_empty() {
        assert_eq!(
            exact_markers,
            0,
            "{} has an ambiguous test-module marker",
            relative_path.display()
        );
        return text;
    }

    assert_eq!(
        test_module_openings.len(),
        1,
        "{} has multiple conventional test modules",
        relative_path.display()
    );
    assert_eq!(
        exact_markers,
        1,
        "{} has an unsupported cfg(test) module layout",
        relative_path.display()
    );
    let marker_start = text
        .find(TEST_MODULE_MARKER)
        .expect("the exact marker count was checked");
    let opening_index = test_module_openings[0];
    assert_eq!(
        tokens[opening_index].start,
        marker_start + TEST_MODULE_MARKER.len() - 1,
        "{} has an ambiguous test-module marker",
        relative_path.display()
    );

    let mut brace_depth = 0_usize;
    let closing = tokens[opening_index..]
        .iter()
        .find(|token| {
            if token.is_punct('{') {
                brace_depth += 1;
            } else if token.is_punct('}') {
                brace_depth = brace_depth.checked_sub(1).unwrap_or_else(|| {
                    panic!(
                        "{} has an ambiguous test-module boundary",
                        relative_path.display()
                    )
                });
                if brace_depth == 0 {
                    return true;
                }
            }
            false
        })
        .unwrap_or_else(|| {
            panic!(
                "{} has an unterminated conventional test module",
                relative_path.display()
            )
        });
    let suffix_tokens = rust_tokens(&text[closing.end..]).unwrap_or_else(|error| {
        panic!(
            "{} has invalid trivia after its test module: {error}",
            relative_path.display()
        )
    });
    assert!(
        suffix_tokens.is_empty(),
        "{} has source after its conventional test module",
        relative_path.display()
    );
    &text[..marker_start]
}

fn bind_invocation_count(production: &str) -> usize {
    let tokens = rust_tokens(production)
        .unwrap_or_else(|error| panic!("production bind policy could not be inspected: {error}"));
    let mut count = 0;
    for (index, token) in tokens.iter().enumerate() {
        if !token.is_ident("bind")
            || index
                .checked_sub(1)
                .is_some_and(|previous| tokens[previous].is_ident("fn"))
        {
            continue;
        }

        let mut next = index + 1;
        if tokens.get(next).is_some_and(|token| token.is_punct(':'))
            && tokens
                .get(next + 1)
                .is_some_and(|token| token.is_punct(':'))
            && tokens
                .get(next + 2)
                .is_some_and(|token| token.is_punct('<'))
        {
            next += 3;
            let mut angle_depth = 1_usize;
            while let Some(token) = tokens.get(next) {
                if token.is_punct('<') {
                    angle_depth += 1;
                } else if token.is_punct('>') {
                    angle_depth -= 1;
                    if angle_depth == 0 {
                        next += 1;
                        break;
                    }
                }
                next += 1;
            }
            if angle_depth != 0 {
                continue;
            }
        }
        while tokens.get(next).is_some_and(|token| token.is_punct(')')) {
            next += 1;
        }
        if tokens.get(next).is_some_and(|token| token.is_punct('(')) {
            count += 1;
        }
    }
    count
}

fn contains_identifier(production: &str, identifier: &str) -> bool {
    rust_tokens(production)
        .unwrap_or_else(|error| {
            panic!("production identifier policy could not be inspected: {error}")
        })
        .iter()
        .any(|token| token.is_ident(identifier))
}

fn lexical_occurrence_count(production: &str, needle: &str) -> usize {
    rust_tokens(production)
        .unwrap_or_else(|error| {
            panic!("production occurrence policy could not be inspected: {error}")
        })
        .iter()
        .map(|token| production[token.start..token.end].matches(needle).count())
        .sum()
}

fn approved_bind_expression_count(production: &str) -> usize {
    let tokens = rust_tokens(production)
        .unwrap_or_else(|error| panic!("approved bind expression could not be inspected: {error}"));
    production
        .match_indices(APPROVED_BIND)
        .filter(|(start, matched)| {
            let end = start + matched.len();
            let mut cursor = *start;
            for token in tokens
                .iter()
                .filter(|token| token.end > *start && token.start < end)
            {
                if token.start < *start
                    || token.end > end
                    || token.kind == RustTokenKind::Literal
                    || !production[cursor..token.start]
                        .chars()
                        .all(char::is_whitespace)
                {
                    return false;
                }
                cursor = token.end;
            }
            production[cursor..end].chars().all(char::is_whitespace)
        })
        .count()
}

fn assert_viewer_endpoint_policy(relative_path: &Path, production: &str) {
    let expected = usize::from(relative_path == Path::new(APPROVED_VIEWER_ENDPOINT));
    assert_eq!(
        lexical_occurrence_count(production, "127.0.0.1"),
        expected,
        "{} has the wrong production loopback endpoint count",
        relative_path.display()
    );
}

#[test]
fn production_portion_rejects_commented_test_close_followed_by_production() {
    let text = r#"fn approved_production() {}
#[cfg(test)]
mod tests {
    #[test]
    fn synthetic_case() {}
} // tests
fn hidden_production() {
    TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0));
}
"#;

    let result =
        std::panic::catch_unwind(|| production_portion(Path::new("src/synthetic.rs"), text));

    assert!(
        result.is_err(),
        "production source after a commented test-module close was hidden"
    );
}

#[test]
fn production_portion_accepts_terminal_modules_with_nested_and_lexical_braces() {
    let cases = [
        (
            "ordinary terminal module",
            "fn approved_production() {}\n#[cfg(test)]\nmod tests {\n}\n",
        ),
        (
            "nested code braces",
            "fn approved_production() {}\n#[cfg(test)]\nmod tests {\n    fn nested() { if true { let _ = (); } }\n}\n",
        ),
        (
            "commented terminal close",
            "fn approved_production() {}\n#[cfg(test)]\nmod tests {\n    fn nested() {}\n} // tests\n",
        ),
        (
            "comments and literals containing braces",
            r####"fn approved_production() {}
#[cfg(test)]
mod tests {
    // } line-comment close
    /* } outer /* { nested } */ { */
    let _cooked = "escaped quote: \" }";
    let _bytes = b"escaped quote: \" }";
    let _character = '}';
    let _byte_character = b'}';
    let _raw = r###"} raw {"###;
    let _raw_bytes = br##"} raw byte {"##;
}
"####,
        ),
    ];

    for (name, text) in cases {
        assert_eq!(
            production_portion(Path::new("src/synthetic.rs"), text),
            "fn approved_production() {}",
            "{name}"
        );
    }
}

#[test]
fn production_portion_rejects_unterminated_nonterminal_repeated_and_unsupported_modules() {
    let cases = [
        (
            "unterminated",
            "fn approved_production() {}\n#[cfg(test)]\nmod tests {\n    fn nested() {}\n",
        ),
        (
            "nonterminal",
            "fn approved_production() {}\n#[cfg(test)]\nmod tests {\n}\nfn forbidden_production() {}\n",
        ),
        (
            "repeated",
            "fn approved_production() {}\n#[cfg(test)]\nmod tests {\n}\n#[cfg(test)]\nmod tests {\n}\n",
        ),
        (
            "unsupported layout",
            "fn approved_production() {}\n#[cfg(test)] mod tests {\n}\n",
        ),
    ];

    for (name, text) in cases {
        let result =
            std::panic::catch_unwind(|| production_portion(Path::new("src/synthetic.rs"), text));
        assert!(result.is_err(), "{name} test module was accepted");
    }
}

#[test]
fn bind_counter_detects_comment_separated_second_relay_bind() {
    let production = r#"
TcpListener::bind((Ipv4Addr::LOCALHOST, 0));
TcpListener::bind /* second listener */ ((Ipv4Addr::UNSPECIFIED, 0));
"#;

    assert_eq!(
        bind_invocation_count(production),
        2,
        "comment trivia must not hide a second bind invocation"
    );
}

#[test]
fn bind_counter_handles_whitespace_comments_turbofish_and_parenthesization() {
    let cases = [
        ("whitespace", "TcpListener :: bind ((endpoint));"),
        (
            "line comment",
            "TcpListener::bind // trivia\n ((endpoint));",
        ),
        (
            "block comment",
            "TcpListener::bind /* trivia */ ((endpoint));",
        ),
        (
            "nested block comment",
            "TcpListener::bind /* outer /* nested */ trivia */ ((endpoint));",
        ),
        (
            "turbofish",
            "TcpListener::bind::<(Ipv4Addr, u16)>((endpoint));",
        ),
        (
            "parenthesized callee",
            "(TcpListener::bind) (((endpoint)));",
        ),
        (
            "parenthesized arguments",
            "TcpListener::bind((((endpoint))));",
        ),
    ];

    for (name, production) in cases {
        assert_eq!(
            bind_invocation_count(production),
            1,
            "{name} bind invocation was not detected"
        );
    }
}

#[test]
fn bind_counter_ignores_comments_literals_definitions_and_split_identifiers() {
    let production = r####"
// TcpListener::bind((comment));
/* bind((block_comment)); /* TcpListener::bind((nested)); */ */
let _cooked = "TcpListener::bind((string))";
let _bytes = b"bind((byte_string))";
let _raw = r###"TcpListener::bind((raw_string))"###;
let _raw_bytes = br##"bind((raw_byte_string))"##;
let _character = 'b';
let _byte_character = b'b';
fn bind(_endpoint: Endpoint) {}
let binding = 1;
bi/* token boundary */nd((not_a_bind_call));
"####;

    assert_eq!(
        bind_invocation_count(production),
        0,
        "non-call text must not create bind invocations"
    );
}

#[test]
fn approved_bind_pin_ignores_exact_decoys_in_comments_and_literals() {
    let production = r#"
TcpListener::bind((Ipv4Addr::LOCALHOST, 0));
// TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
/* TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) */
let _decoy = "TcpListener::bind((Ipv4Addr::LOCALHOST, 0))";
"#;

    assert_eq!(
        approved_bind_expression_count(production),
        1,
        "only the literal approved production expression may satisfy the pin"
    );
}

#[test]
fn viewer_endpoint_policy_rejects_a_second_approved_file_occurrence() {
    let production = r#"
let viewer_endpoint = "127.0.0.1";
let second_endpoint = "127.0.0.1";
"#;

    let result = std::panic::catch_unwind(|| {
        assert_viewer_endpoint_policy(Path::new(APPROVED_VIEWER_ENDPOINT), production)
    });

    assert!(
        result.is_err(),
        "the approved viewer file must contain exactly one endpoint occurrence"
    );
}

#[test]
fn public_surface_is_verified_transport_only_and_contains_no_dangerous_constructor() {
    let fallback = source("src/fallback/mod.rs");
    let public_prefix = fallback
        .split("\n#[cfg(test)]\nmod tests")
        .next()
        .expect("fallback source has a production section");

    assert!(public_prefix.contains("pub async fn open("));
    assert!(public_prefix.contains("TrustedSshProxy"));
    assert!(public_prefix.contains("&RuntimeDir"));
    assert!(public_prefix.contains("&Path"));
    assert!(public_prefix.contains("FallbackPreferences"));
    for forbidden in [
        "bind_addr",
        "bind_address",
        "clear_password",
        "password: String",
        "host: String",
        "endpoint: String",
        "VncCommand",
        "TcpStream",
        "dyn AsyncRead",
        "auto_fallback",
    ] {
        assert!(
            !public_prefix.contains(forbidden),
            "dangerous public fallback surface contains {forbidden}"
        );
    }
}

#[test]
fn listener_process_and_password_artifacts_stay_inside_the_approved_boundary() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let relay = source("src/fallback/relay.rs");
    let password = source("src/fallback/password_file.rs");
    let fallback = source("src/fallback/mod.rs");
    let viewer = source("src/fallback/viewer.rs");
    let manager = source("src/session/manager.rs");
    assert!(relay.contains("TcpListener::bind((Ipv4Addr::LOCALHOST, 0))"));
    assert!(relay.contains("copy_bidirectional"));
    assert!(viewer.contains("File::open(configured_path)"));
    assert!(viewer.contains(".metadata()"));
    assert!(viewer.contains("create_new(true)"));
    assert!(fallback.contains("tokio::process::Command::new(snapshot.path())"));
    assert!(!fallback.contains("tokio::process::Command::new(viewer_path)"));
    assert!(password.contains("0xE8, 0x4A, 0xD6, 0x60, 0xC4, 0x72, 0x1A, 0xE0"));

    const SOCKET_SURFACES: [&str; 7] = [
        "TcpListener",
        "TcpStream",
        "TcpSocket",
        "UdpSocket",
        "UnixListener",
        "UnixStream",
        "UnixDatagram",
    ];
    let mut approved_relay_seen = false;
    let mut approved_viewer_endpoint_seen = false;
    for file in rust_sources(&manifest.join("src")) {
        let relative_path = file
            .strip_prefix(manifest)
            .expect("enumerated source remains below the manifest root");
        let text = fs::read_to_string(&file).unwrap();
        let production = production_portion(relative_path, &text);
        let bind_calls = bind_invocation_count(production);

        if relative_path == Path::new(APPROVED_RELAY) {
            approved_relay_seen = true;
            assert_eq!(
                approved_bind_expression_count(production),
                1,
                "approved relay must contain one exact loopback bind"
            );
            assert_eq!(
                bind_calls, 1,
                "approved relay gained a second or non-approved bind"
            );
            for forbidden in &SOCKET_SURFACES[2..] {
                assert!(
                    !contains_identifier(production, forbidden),
                    "approved relay gained another socket family: {forbidden}"
                );
            }
        } else {
            for forbidden in SOCKET_SURFACES {
                assert!(
                    !contains_identifier(production, forbidden),
                    "{} gained a listener/socket surface: {forbidden}",
                    relative_path.display()
                );
            }
            assert_eq!(
                bind_calls,
                0,
                "{} gained a production bind",
                relative_path.display()
            );
        }
        if relative_path == Path::new(APPROVED_VIEWER_ENDPOINT) {
            approved_viewer_endpoint_seen = true;
        }
        assert_viewer_endpoint_policy(relative_path, production);
    }
    assert!(
        approved_relay_seen,
        "approved relay source was not enumerated"
    );
    assert!(
        approved_viewer_endpoint_seen,
        "approved viewer endpoint source was not enumerated"
    );

    let cleanup_cycle = manager
        .split("async fn ten_connect_disconnect_cycles_leave_zero_exact_owned_residue()")
        .nth(1)
        .expect("Task 13 cleanup cycle exists")
        .split("\n    #[cfg(unix)]")
        .next()
        .expect("Task 13 cleanup cycle has a bounded source section");
    for unrelated in ["TcpListener", "listener_task", "rebound", "drop(runtime)"] {
        assert!(
            !cleanup_cycle.contains(unrelated),
            "cleanup proof retained unrelated listener evidence: {unrelated}"
        );
    }
}

#[test]
fn launch_and_lifecycle_constants_are_fixed_and_secret_free() {
    let fallback_source = source("src/fallback/mod.rs");
    let fallback = fallback_source
        .split("\n#[cfg(test)]\nmod tests")
        .next()
        .unwrap();
    let relay = source("src/fallback/relay.rs");
    for argument in [
        "-Shared=1",
        "-RemoteResize=1",
        "-SecurityTypes=VncAuth",
        "-PasswordFile",
        "-FullScreen=1",
        "-ViewOnly=1",
    ] {
        assert!(
            fallback.contains(argument),
            "missing fixed argument {argument}"
        );
    }
    assert!(fallback.contains(".env_clear()"));
    assert!(fallback.contains(".env(\"PATH\", \"/usr/bin:/bin\")"));
    for allowed in ["HOME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE"] {
        assert!(fallback.contains(allowed));
    }
    assert!(relay.contains("Duration::from_secs(20)"));
    assert!(fallback.contains("Duration::from_secs(3)"));
    assert!(!fallback.contains("Stdio::piped"));
    assert!(!fallback.contains("/bin/sh"));
    assert!(!fallback.contains("sh -c"));
}

#[test]
fn semantic_command_contains_only_target_and_display_preferences() {
    let events = source("src/session/events.rs");
    let command = events
        .split("OpenInTigerVnc")
        .nth(1)
        .expect("semantic fallback command exists")
        .split('}')
        .next()
        .unwrap();
    assert!(command.contains("vmid: VmId"));
    assert!(command.contains("preferences: FallbackPreferences"));
    for forbidden in [
        "PathBuf",
        "String",
        "password",
        "endpoint",
        "host",
        "VncCommand",
        "TrustedSshProxy",
    ] {
        assert!(!command.contains(forbidden));
    }
}

#[test]
fn production_manager_rechecks_inventory_before_one_pinned_viewer_open() {
    let manager = source("src/session/manager.rs");
    let production = manager
        .split("impl SessionBackend for ProductionBackend")
        .nth(1)
        .unwrap();
    let fallback = production
        .split("fn open_fallback(")
        .nth(1)
        .unwrap()
        .split("fn close_master")
        .next()
        .unwrap();
    assert!(!fallback.contains("validate_viewer_path"));
    let verify = fallback.find("master.verify()").unwrap();
    let connect = fallback.find("TrustedSshProxy::connect").unwrap();
    let open = fallback.find("TigerVncFallback::open").unwrap();
    assert!(verify < connect && connect < open);

    let native_error_mapping = manager
        .split("fn public_rfb_error")
        .nth(1)
        .unwrap()
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert!(!native_error_mapping.contains("OpenInTigerVnc"));
}

#[test]
fn pure_state_retains_only_configuration_presence_and_shutdown_closes_fallbacks_first() {
    let state = source("src/app/state.rs");
    let fields = state
        .split("pub struct AppState")
        .nth(1)
        .unwrap()
        .split("impl AppState")
        .next()
        .unwrap();
    assert!(fields.contains("fallback_configured: bool"));
    assert!(!fields.contains("fallback_viewer"));
    assert!(!fields.contains("PathBuf"));

    let manager = source("src/session/manager.rs");
    let shutdown = manager
        .split("async fn shutdown(&mut self)")
        .nth(1)
        .unwrap()
        .split("fn active_session_count")
        .next()
        .unwrap();
    assert!(
        shutdown.find("record.session.close()").unwrap()
            < shutdown.find("self.backend.close_master()").unwrap()
    );
}
