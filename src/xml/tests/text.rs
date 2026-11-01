use super::*;
use std::iter;

#[test]
fn text_scan_matches_the_character_predicate() {
    let expected = |text: &str| text.chars().all(valid_char);
    // Every scalar value alone, which the scan reads as a remainder, and
    // across a boundary between full chunks. One prefix leaves its chunk
    // to be flagged by the character alone; the other flags it anyway.
    let mut text = String::new();
    let characters = (0..=u32::from(char::MAX)).filter_map(char::from_u32);
    for character in characters {
        for prefix in ["", "abc", "\u{fffd}"] {
            text.clear();
            text.push_str(prefix);
            text.push(character);
            text.push_str(&"abcdefg"[prefix.len()..]);
            assert_eq!(
                valid_text_in_chunks::<4>(&text),
                expected(&text),
                "{text:?}"
            );
            if prefix.is_empty() {
                let alone = &text[..character.len_utf8()];
                assert_eq!(valid_text(alone), expected(alone), "{alone:?}");
            }
        }
    }
    // The scan's special cases and their near misses, at every offset of
    // texts that end in and around each of the first chunk boundaries.
    let specials = [
        "\0",
        "\u{1}",
        "\u{1f}",
        "\t",
        "\n",
        "\r",
        " ",
        "\u{7f}",
        "\u{ef}",
        "\u{efbf}",
        "\u{f000}",
        "\u{ffbe}",
        "\u{fffd}",
        "\u{fffe}",
        "\u{ffff}",
        "\u{10ffff}",
    ];
    let lengths = (0..8).chain(124..132).chain(252..260);
    for length in lengths {
        for position in 0..=length {
            for special in specials {
                text.clear();
                text.extend(iter::repeat_n('a', position));
                text.push_str(special);
                text.extend(iter::repeat_n('a', length - position));
                assert_eq!(valid_text(&text), expected(&text), "{text:?}");
            }
        }
    }
    let valid = [
        "a",
        " ",
        "\t",
        "\n",
        "\r",
        "\u{7f}",
        "\u{80}",
        "\u{ef}",
        "雪",
        "\u{d7ff}",
        "\u{e000}",
        "\u{efbf}",
        "\u{f8ff}",
        "\u{feff}",
        "\u{ffbe}",
        "\u{fffd}",
        "\u{10000}",
        "\u{10ffff}",
    ];
    let invalid = ["\0", "\u{8}", "\u{1f}", "\u{fffe}", "\u{ffff}"];
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..20_000 {
        text.clear();
        // Invalid pieces are rare, so that one often falls in a late chunk.
        for _ in 0..next() % 160 {
            let random = next();
            text.push_str(if random % 64 == 0 {
                invalid[(random >> 8) as usize % invalid.len()]
            } else {
                valid[(random >> 8) as usize % valid.len()]
            });
        }
        assert_eq!(valid_text(&text), expected(&text), "{text:?}");
        assert_eq!(
            valid_text_in_chunks::<4>(&text),
            expected(&text),
            "{text:?}"
        );
    }
}

#[test]
fn references_resolve_only_xml_10_entities_and_legal_characters() {
    use std::fmt::Write as _;

    let resolve = |name: &str| match reference(&BytesRef::new(name)) {
        Ok(character) => Ok(character),
        Err(Error::Xml(message)) => Err(message),
        Err(error) => panic!("{name:?}: {error:?}"),
    };
    let invalid = || Err("invalid XML character reference".to_owned());
    let mut name = String::new();
    let codepoints = (0..=0x11_0000).chain([u32::MAX]);
    for codepoint in codepoints {
        let expected = char::from_u32(codepoint)
            .filter(|&c| c != '\0' && valid_char(c))
            .map_or_else(invalid, Ok);
        for spelling in [
            format_args!("#{codepoint}"),
            format_args!("#x{codepoint:x}"),
            format_args!("#x{codepoint:X}"),
        ] {
            name.clear();
            write!(name, "{spelling}").unwrap();
            assert_eq!(resolve(&name), expected, "{name}");
        }
    }
    for (name, character) in [
        ("amp", '&'),
        ("lt", '<'),
        ("gt", '>'),
        ("apos", '\''),
        ("quot", '"'),
        ("#00000065", 'A'),
        ("#x00000041", 'A'),
    ] {
        assert_eq!(resolve(name), Ok(character), "{name}");
    }
    // HTML and differently spelled names are undefined in XML, whatever
    // quick-xml's escape-html feature adds.
    for name in [
        "nbsp",
        "NotEqualTilde",
        "eacute",
        "AMP",
        "Amp",
        "unknown",
        "雪",
        "",
    ] {
        assert_eq!(
            resolve(name),
            Err(format!("undefined XML entity &{name};")),
            "{name}"
        );
    }
    for name in [
        "#",
        "#x",
        "#X41",
        "#-65",
        "#+65",
        "#x+41",
        "#x-41",
        "# 65",
        "#65 ",
        "#999999999999999999999999999999",
        "#xFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
        "#xD800",
        "#xDFFF",
        "#0",
        "#x0",
        "#x1",
        "#xFFFE",
    ] {
        assert_eq!(resolve(name), invalid(), "{name}");
    }
}

#[test]
fn excel_escape_units_match_utf16_decoding() {
    for unit in 0..=u16::MAX {
        let escaped = format!("before雪_x{unit:04X}_😀after");
        let expected = String::from_utf16(&[unit])
            .ok()
            .map(|text| format!("before雪{text}😀after"));
        assert_eq!(excel_text(&escaped).ok(), expected, "{escaped}");
    }
    // Both ends of the surrogate ranges, valid pairs, and every way a
    // non-surrogate, high surrogate or low surrogate can meet another.
    let units = [
        0, 0x41, 0xd7ff, 0xd800, 0xdbff, 0xdc00, 0xdfff, 0xe000, 0xffff,
    ];
    for first in units {
        for second in units {
            let escaped = format!("_x{first:04X}__x{second:04X}_");
            assert_eq!(
                excel_text(&escaped).ok(),
                String::from_utf16(&[first, second]).ok(),
                "{escaped}"
            );
        }
    }
    for text in [
        "_xD800_",
        "_xD800_雪",
        "_xD800_😀",
        "😀_xDC00_",
        "_xD800__XDC00_",
    ] {
        assert!(excel_text(text).is_err(), "{text}");
    }
}

#[test]
fn excel_escaping_is_the_exact_inverse_of_decoding() {
    assert!(matches!(excel_escaped("Plain 雪 x0041"), Cow::Borrowed(_)));
    for (text, escaped) in [
        (
            "\0\u{1}\u{8}\u{b}\u{c}\u{e}\u{1f}",
            "_x0000__x0001__x0008__x000B__x000C__x000E__x001F_",
        ),
        ("\u{fffe}\u{ffff}\t\n\r", "_xFFFE__xFFFF_\t\n\r"),
        ("_x0041_ _Xbeef_", "_x005F_x0041_ _Xbeef_"),
        // A literal prefix before an escaped character would otherwise
        // read back as one token.
        ("_x0001\u{1}", "_x005F_x0001_x0001_"),
        ("_X0001\u{1}", "_X0001_x0001_"),
        ("_x0041 _x004G_ _x041_ _", "_x0041 _x004G_ _x041_ _"),
    ] {
        assert_eq!(excel_escaped(text), escaped, "{text:?}");
        assert_eq!(excel_text(escaped).unwrap(), text);
    }
    let pieces = [
        "_", "x", "X", "0", "1", "F", "d", "a", "\u{1}", "\u{fffe}", "\0", "雪", "_x0041_",
        "_x0001", "_xD83D_", "_X005F", "x005F_",
    ];
    let mut state = 0x2545_f491_4f6c_dd1du64;
    for _ in 0..20_000 {
        let mut text = String::new();
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        for step in 0..(state % 12) {
            text.push_str(pieces[(state >> (step * 5 + 4)) as usize % pieces.len()]);
        }
        let escaped = excel_escaped(&text);
        assert!(escaped.chars().all(valid_char), "{text:?}");
        assert_eq!(
            excel_text(&escaped).unwrap(),
            text,
            "{text:?} -> {escaped:?}"
        );
    }
}

#[test]
fn excel_escape_decoding_is_single_pass_and_utf16_aware() {
    assert_eq!(excel_text("Plain 雪😀").unwrap(), "Plain 雪😀");
    assert_eq!(excel_text("_x005F_x0041_").unwrap(), "_x0041_");
    assert_eq!(excel_text("_x005F_X0041_").unwrap(), "_X0041_");
    assert_eq!(excel_text("_xD83D__xDE00_").unwrap(), "😀");
    assert_eq!(excel_text("_xd83d__xde00_").unwrap(), "😀");
    assert_eq!(excel_text("_X0041_雪").unwrap(), "_X0041_雪");
    assert_eq!(excel_text("_XD83D__XDE00_").unwrap(), "_XD83D__XDE00_");
    assert_eq!(excel_text("_XD800_").unwrap(), "_XD800_");
    // A lone surrogate is valid ST_Xstring, which a Rust string cannot hold.
    for text in ["_xD800_", "_xDBFF_x", "_xDC00_", "_xD83D__xD83D__xDE00_"] {
        assert!(
            matches!(
                excel_text(text),
                Err(Error::Unsupported(message))
                    if message == "unpaired UTF-16 surrogate in Excel text escape"
            ),
            "{text}"
        );
    }
}
