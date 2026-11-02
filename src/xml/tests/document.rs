use super::*;

#[test]
fn qualified_names_accept_the_xml_unicode_ranges() {
    for name in [
        "row",
        "xmlns",
        "xmlns:雪",
        "雪:属性",
        "_one",
        "a-1.b",
        "a\u{300}",
        "\u{10000}:\u{effff}",
    ] {
        assert!(valid_qname(name), "{name}");
    }
    for name in [
        "",
        "0bad",
        ":name",
        "name:",
        "a:b:c",
        "a b",
        "a/b",
        "\u{d7}",
        "\u{300}a",
        "\u{f0000}",
    ] {
        assert!(!valid_qname(name), "{name}");
    }
    assert!(valid_name(":target:one"));
    assert!(!valid_name("1target"));
}

#[test]
fn xml_space_is_limited_to_the_four_defined_characters() {
    assert!(whitespace(" \t\r\n"));
    assert_eq!(trim_whitespace(" \t\r\n17\n\t "), "17");
    let spaces: Vec<u8> = (0..=u8::MAX).filter(|&byte| is_space(byte)).collect();
    assert_eq!(spaces, b"\t\n\r ");
    for text in ["\u{a0}", "\u{2000}", "\u{85}", "text"] {
        assert!(!whitespace(text));
        assert_eq!(trim_whitespace(text), text);
    }
}
