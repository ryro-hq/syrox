use super::*;

fn complete(client: &mut Client, uri: &str, text: &str, needle: &str) -> Value {
    let offset = text.rfind(needle).unwrap() + needle.len();
    client.request("textDocument/completion",json!({"textDocument":{"uri":uri},"position":{"line":0,"character":text[..offset].encode_utf16().count()}}))["result"].clone()
}

#[test]
fn constructors_complete_missing_specialized_fields_and_defaults_without_duplicates() {
    let temp = tempfile::tempdir().unwrap();
    let declarations = "value S(str); struct Box<T> { first: T; second: T; optional: S = S(\"default\"); } fn inspect() { ";
    let path = temp.path().join("main.srx");
    std::fs::write(&path, format!("{declarations}}}")).unwrap();
    let document = uri(&path);
    let mut client = Client::new(temp.path(), "utf-16");
    client.open(&document, &format!("{declarations}}}"));
    for (index, body, needle, labels) in [
        (
            0,
            "let b = Box<S> {  }; }",
            "Box<S> { ",
            vec!["first", "second", "optional"],
        ),
        (
            1,
            "let b = Box<S> { first = S(\"é😀\"); se }; }",
            " se",
            vec!["second"],
        ),
        (
            2,
            "let b = Box<S> {  first = S(\"ok\"); }; }",
            "Box<S> { ",
            vec!["second", "optional"],
        ),
        (
            3,
            "let b = Box<S> { first = S(\"ok\"); second = S(\"ok\");  }; }",
            "second = S(\"ok\"); ",
            vec!["optional"],
        ),
        (
            4,
            "let b = Box<S> { ",
            "Box<S> { ",
            vec!["first", "second", "optional"],
        ),
        (
            5,
            "let b = Box<S> { first = ; se }; }",
            " se",
            vec!["second"],
        ),
    ] {
        let text = format!("{declarations}{body}");
        client.notify("textDocument/didChange",json!({"textDocument":{"uri":document,"version":index+2},"contentChanges":[{"text":text}]}));
        let response = complete(&mut client, &document, &text, needle);
        let items = response["items"].as_array().unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| item["label"].as_str().unwrap())
                .collect::<Vec<_>>(),
            labels,
            "case {index}: {response}"
        );
        for item in items {
            assert_eq!(item["kind"], 5);
            let default = item["label"] == "optional";
            assert_eq!(
                item["detail"],
                if default {
                    "S (default)"
                } else {
                    "S (required)"
                }
            );
            assert!(item["sortText"].as_str().unwrap().starts_with(if default {
                "1:"
            } else {
                "0:"
            }));
        }
        if index == 1 {
            let start = text.find(" se }").unwrap() + 1;
            assert_eq!(
                items[0]["textEdit"]["range"]["start"]["character"],
                text[..start].encode_utf16().count()
            );
            assert_eq!(
                items[0]["textEdit"]["range"]["end"]["character"],
                text[..start + 2].encode_utf16().count()
            );
        }
    }
    client.stop();
}

#[test]
fn constructors_use_innermost_literal_and_lexical_or_delegated_authority() {
    let temp = tempfile::tempdir().unwrap();
    let declarations = "value S(str); struct Inner { nested: S; } struct Outer { inner: Inner; outer: S; } mod vault { pub opaque struct Secret { hidden: S; } pub opaque struct Owned<owner T> { delegated: S; } } struct Key {} ";
    let path = temp.path().join("main.srx");
    std::fs::write(&path, declarations).unwrap();
    let document = uri(&path);
    let mut client = Client::new(temp.path(), "utf-16");
    client.open(&document, declarations);
    for (index, body, needle, expected) in [
        (
            0,
            "fn f() { Outer { inner = Inner { ne }; outer = S(\"x\"); }; }",
            "Inner { ne",
            Some("nested"),
        ),
        (
            1,
            "fn f() { Outer { inner = Inner { nested = S(\"x\"); }; ou }; }",
            " ou",
            Some("outer"),
        ),
        (2, "fn f() { vault::Secret { hi }; }", " hi", None),
        (
            3,
            "mod vault { fn f() { Secret { hi }; } }",
            " hi",
            Some("hidden"),
        ),
        (
            4,
            "fn f() { vault::Owned<Key> { de }; }",
            " de",
            Some("delegated"),
        ),
    ] {
        let text = format!("{declarations}{body}");
        client.notify("textDocument/didChange",json!({"textDocument":{"uri":document,"version":index+2},"contentChanges":[{"text":text}]}));
        let response = complete(&mut client, &document, &text, needle);
        let items = response["items"].as_array().unwrap();
        if let Some(label) = expected {
            assert!(
                items.iter().any(|item| item["label"] == label),
                "case {index}: {response}"
            );
        } else {
            assert!(items.is_empty(), "{response}");
        }
    }
    client.stop();
}
