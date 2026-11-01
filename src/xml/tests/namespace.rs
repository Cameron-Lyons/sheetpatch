use super::*;

#[test]
fn namespace_cache_reuses_inherited_bindings_and_restores_shadowed_parents() {
    let mut xml = String::from("<root xmlns:v='urn:parent'>");
    for _ in 0..32 {
        xml.push_str(
            "<v:item xmlns:q='urn:unrelated'/>\
             <v:child xmlns:v='urn:child'><v:item/></v:child>\
             <v:item/>\
             <v:other xmlns:v='urn:other'/>\
             <v:item/>",
        );
    }
    xml.push_str("</root>");
    let mut reader = NamespaceReader::from_str(&xml);
    let mut cache = NamespaceCache::default();
    let mut resolutions = HashMap::new();
    let mut values = Vec::new();
    loop {
        match reader.read_event().unwrap() {
            Event::Start(tag) | Event::Empty(tag) if tag.name().prefix().is_some() => {
                let name = tag.name();
                let value = cache
                    .get(&reader, name, NameKind::Element)
                    .unwrap_or_else(|| {
                        let uri = namespace(reader.resolver().resolve_element(name).0).unwrap();
                        *resolutions.entry(uri.clone()).or_insert(0) += 1;
                        cache.insert(&reader, name, NameKind::Element, uri.clone());
                        uri
                    });
                assert_eq!(
                    cache.get(&reader, QName("v:attribute"), NameKind::Attribute),
                    Some(value.clone())
                );
                assert!(
                    cache
                        .get(&reader, QName("attribute"), NameKind::Attribute)
                        .is_none()
                );
                values.push(value);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    assert_eq!(
        values,
        [
            "urn:parent",
            "urn:child",
            "urn:child",
            "urn:parent",
            "urn:other",
            "urn:parent"
        ]
        .repeat(32)
    );
    assert_eq!(resolutions["urn:parent"], 1);
    assert_eq!(resolutions["urn:child"], 32);
    assert_eq!(resolutions["urn:other"], 32);
}

#[test]
fn cleared_namespace_pools_forget_every_uri_id_and_binding() {
    let mut reader = NamespaceReader::from_str("<p:a xmlns:p='urn:p' xmlns:q='urn&#58;q'/>");
    let Event::Empty(tag) = reader.read_event().unwrap() else {
        panic!("expected the empty root");
    };
    let mut pool = NamespacePool::new();
    let p = pool
        .resolve(&reader, tag.name(), NameKind::Element)
        .unwrap();
    let q = pool
        .resolve(&reader, QName("q:x"), NameKind::Attribute)
        .unwrap();
    assert_eq!((pool.spellings(), &**pool.uri(q)), (2, "urn:q"));
    pool.clear();
    assert_eq!(
        (pool.spellings(), pool.id("urn:p"), pool.id("urn:q")),
        (0, None, None)
    );
    // IDs start over: neither a cached binding nor the URI table keeps an
    // ID from before.
    assert_eq!(
        pool.resolve(&reader, QName("q:x"), NameKind::Attribute)
            .unwrap(),
        p
    );
    assert_eq!(
        pool.resolve(&reader, tag.name(), NameKind::Element)
            .unwrap(),
        q
    );
    assert_eq!((&**pool.uri(p), &**pool.uri(q)), ("urn:q", "urn:p"));
}

#[test]
fn escaped_xml_binding_retry_keeps_the_namespace_binding_limit() {
    let mut reader = NamespaceReader::from_str(
        "<root xmlns:a='urn:a' xmlns:xml='http://www.w3.org/XML/1998/namespac&#101;' xmlns:b='urn:b' xmlns:c='urn:c'/>",
    );
    reader.namespaces.set_max_namespace_bindings(2);
    assert!(matches!(
        reader.read_event(),
        Err(quick_xml::Error::Namespace(
            NamespaceError::TooManyBindings(2)
        ))
    ));
}
