use super::*;

#[test]
fn resolve_relative_and_absolute_part_uris() {
    assert_eq!(
        resolve_target("books/main.xml", "../tabs/sheet%201.xml").unwrap(),
        "tabs/sheet%201.xml"
    );
    assert_eq!(
        resolve_target("books/main.xml", "/xl/worksheets/sheet1.xml").unwrap(),
        "xl/worksheets/sheet1.xml"
    );
    for (target, resolved) in [
        ("../tabs/sheet%20%3f.xml", "tabs/sheet%20%3F.xml"),
        ("../tabs/sheet%25.xml", "tabs/sheet%25.xml"),
        ("../tabs/雪.xml", "tabs/%E9%9B%AA.xml"),
        ("../tabs/sheet:1.xml", "tabs/sheet:1.xml"),
        ("./sheet:1.xml", "books/sheet:1.xml"),
        ("/tabs/sheet:1.xml", "tabs/sheet:1.xml"),
        ("workshee%74s/sheet1.xml", "books/worksheets/sheet1.xml"),
    ] {
        assert_eq!(resolve_target("books/main.xml", target).unwrap(), resolved);
    }
    for target in [
        "../../escape",
        "http://example.com/x",
        "a%2fb",
        "a%5cb",
        "a%00b",
        "%2e%2e/escape",
        "//example.com/a",
        "sheet:1.xml",
        "a%",
        "a\\b",
        "",
    ] {
        assert!(resolve_target("books/main.xml", target).is_err());
    }
}

// The resolver before targets were normalized against a reusable base.
fn reference_resolve_target(source: &str, target: &str) -> Result<String> {
    if target.is_empty()
        || target.starts_with("//")
        || target.contains(['\\', '#', '?', '\0'])
        || target
            .split('/')
            .next()
            .is_some_and(|segment| segment.contains(':'))
    {
        return Err(Error::InvalidWorkbook(format!(
            "invalid internal relationship target {target}"
        )));
    }
    let target = escape_path(target)?;
    let source = escape_path(source)?;
    let mut parts: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        source
            .rsplit_once('/')
            .map(|(p, _)| p.split('/').collect())
            .unwrap_or_default()
    };
    let mut segments = target
        .strip_prefix('/')
        .unwrap_or(&target)
        .split('/')
        .peekable();
    while let Some(part) = segments.next() {
        match part {
            "." => {
                if segments.peek().is_none() {
                    parts.push("");
                }
            }
            ".." => {
                if parts.pop().is_none() {
                    return Err(Error::InvalidWorkbook(
                        "relationship target escapes package root".into(),
                    ));
                }
                if segments.peek().is_none() {
                    parts.push("");
                }
            }
            value => parts.push(value),
        }
    }
    if parts.is_empty() {
        return Err(Error::InvalidWorkbook(
            "relationship target names no part".into(),
        ));
    }
    if parts.iter().any(|part| part.is_empty()) {
        return Err(Error::InvalidWorkbook(
            "relationship target contains an empty part-name segment".into(),
        ));
    }
    Ok(parts.join("/"))
}

#[test]
fn resolution_against_a_target_base_matches_the_reference_resolver() {
    let sources = [
        "",
        "a.xml",
        "xl/workbook.xml",
        "xl/worksheets/sheet1.xml",
        "a/b/c/d.xml",
        "x%20y/%C3%A9/s.xml",
        "a/b/",
        "a//b/c.xml",
    ];
    let atoms = [
        "a", "B", ".", "..", "", "%41", "%62", "c%20d", "t.xml", "%C3%A9", "x:y", "%2F", "%", "~",
        "A",
    ];
    let mut state: u64 = 0x1234_5678;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as usize
    };
    for _ in 0..100_000 {
        let source = sources[next() % sources.len()];
        let mut target = String::new();
        if next() % 5 == 0 {
            target.push('/');
        }
        for index in 0..1 + next() % 6 {
            if index > 0 {
                target.push('/');
            }
            target.push_str(atoms[next() % atoms.len()]);
        }
        assert_eq!(
            resolve_target(source, &target).map_err(|error| error.to_string()),
            reference_resolve_target(source, &target).map_err(|error| error.to_string()),
            "source {source:?}, target {target:?}"
        );
    }
}

#[test]
fn relationship_resolution_preserves_empty_segments_and_directory_targets() {
    for target in [
        "tabs//sheet.xml",
        "tabs/sheet.xml/",
        "tabs/sheet.xml/.",
        "tabs/sheet.xml/child/..",
        ".",
        "..",
    ] {
        assert!(
            resolve_target("books/main.xml", target).is_err(),
            "{target}"
        );
    }
    // An empty segment removed by a following '..' does not remove its
    // preceding named segment. RFC URI resolution differs from collapsing
    // every consecutive slash before resolving traversal.
    assert_eq!(
        resolve_target("books/main.xml", "tabs//../sheet.xml").unwrap(),
        "books/tabs/sheet.xml"
    );
}
