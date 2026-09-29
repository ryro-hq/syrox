use super::*;

#[test]
fn completion_requested_immediately_after_typing_waits_for_semantic_classification() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("main.srx");
    let text = "fn example() {}";
    std::fs::write(&path, text).unwrap();
    let document = uri(&path);
    let mut client = Client::with_options(temp.path(), "utf-16", false, "bundled");
    client.open(&document, text);
    let changed = "fn example() { std::map_from_en; }";
    client.notify(
        "textDocument/didChange",
        json!({"textDocument":{"uri":document,"version":2},"contentChanges":[{"text":changed}]}),
    );
    let result = client.request(
        "textDocument/completion",
        at(
            &document,
            changed,
            "std::map_from_en",
            "std::map_from_en".len(),
        ),
    );
    let item = result["result"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["label"] == "map_from_entries")
        .expect("semantic function candidate after edit");
    assert_eq!(item["kind"], 3, "Function, not a plain text completion");
    assert!(item["detail"].as_str().unwrap().contains("fn("));
    client.stop();
}

fn at(document: &str, text: &str, needle: &str, delta: usize) -> Value {
    let offset = text.find(needle).unwrap() + delta;
    json!({"textDocument":{"uri":document},"position":{"line":0,"character":text[..offset].encode_utf16().count()}})
}

#[test]
fn completion_respects_interfaces_aliases_and_lexical_locals() {
    let temp = tempfile::tempdir().unwrap();
    let text = "value I(int); mod lib { outputs {} fn hidden() {} pub fn make() {} } use lib::make; fn example(arg: I) { let prior = I(1); prior; let later = I(2); lib::make(); }";
    let path = temp.path().join("main.srx");
    std::fs::write(&path, text).unwrap();
    let document = uri(&path);
    let mut client = Client::new(temp.path(), "utf-16");
    client.open(&document, text);
    client.diagnostics(&document, 1);
    client.diagnostics(&document, 1);
    let qualified = client.request(
        "textDocument/completion",
        at(&document, text, "lib::make();", 5),
    );
    let labels: Vec<_> = qualified["result"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["make"]);
    let local = client.request("textDocument/completion", at(&document, text, "prior;", 0));
    let labels: Vec<_> = local["result"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["label"].as_str().unwrap())
        .collect();
    assert!(labels.contains(&"arg"), "{local}");
    assert!(labels.contains(&"prior"));
    assert!(!labels.contains(&"later"));
    assert!(labels.contains(&"make"));
    client.stop();
}

#[test]
fn signature_help_survives_incomplete_calls_and_counts_nested_arguments() {
    let temp = tempfile::tempdir().unwrap();
    let text = "value I(int); fn choose(first: I, second: I) -> I { second } fn run() -> I { choose(I(1), I(2)) }";
    let path = temp.path().join("main.srx");
    std::fs::write(&path, text).unwrap();
    let document = uri(&path);
    let mut client = Client::new(temp.path(), "utf-16");
    client.open(&document, text);
    client.diagnostics(&document, 1);
    client.diagnostics(&document, 1);
    let help = client.request("textDocument/signatureHelp", at(&document, text, "I(2)", 0));
    assert_eq!(help["result"]["activeParameter"], 1, "{help}");
    assert_eq!(
        help["result"]["signatures"][0]["parameters"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        help["result"]["signatures"][0]["parameters"][1]["label"],
        "second: I"
    );
    let changed = text.replace("choose(I(1), I(2))", "choose(I(1), ");
    client.notify(
        "textDocument/didChange",
        json!({"textDocument":{"uri":document,"version":2},"contentChanges":[{"text":changed}]}),
    );
    client.diagnostics(&document, 2);
    client.diagnostics(&document, 2);
    let help = client.request(
        "textDocument/signatureHelp",
        at(&document, &changed, "choose(I(1), ", "choose(I(1), ".len()),
    );
    assert_eq!(help["result"]["activeParameter"], 1, "{help}");
    client.stop();
}

#[test]
fn completion_handles_an_incomplete_namespace_after_unicode_text() {
    let temp = tempfile::tempdir().unwrap();
    let text = "value S(str); mod lib { pub fn make() {} } fn run() { S(\"😀\"); lib::make(); }";
    let path = temp.path().join("main.srx");
    std::fs::write(&path, text).unwrap();
    let document = uri(&path);
    let mut client = Client::new(temp.path(), "utf-16");
    client.open(&document, text);
    client.diagnostics(&document, 1);
    client.diagnostics(&document, 1);
    let changed = text.replace("lib::make();", "lib::");
    client.notify(
        "textDocument/didChange",
        json!({"textDocument":{"uri":document,"version":2},"contentChanges":[{"text":changed}]}),
    );
    client.diagnostics(&document, 2);
    client.diagnostics(&document, 2);
    let params = at(&document, &changed, "lib::", 5);
    let response = client.request("textDocument/completion", params.clone());
    let item = &response["result"]["items"][0];
    assert_eq!(item["label"], "make");
    assert_eq!(item["textEdit"]["range"]["start"], params["position"]);
    assert_eq!(item["textEdit"]["range"]["end"], params["position"]);
    client.stop();
}

#[test]
fn std_definitions_are_read_only_revisioned_virtual_sources() {
    let temp = tempfile::tempdir().unwrap();
    let text = "fn example() -> std::PackageId { std::PackageId(\"hello\") }";
    let path = temp.path().join("main.srx");
    std::fs::write(&path, text).unwrap();
    let document = uri(&path);
    let mut client = Client::with_options(temp.path(), "utf-16", false, "bundled");
    client.open(&document, text);
    client.diagnostics(&document, 1);
    client.diagnostics(&document, 1);
    let definition = client.request(
        "textDocument/definition",
        at(&document, text, "std::PackageId", 0),
    );
    let virtual_uri = definition["result"]["uri"]
        .as_str()
        .expect("std definition URI");
    assert!(virtual_uri.starts_with("syrox-source:"));
    let content = client.request("syrox/readSource", json!({"uri":virtual_uri}));
    assert!(
        content["result"]["text"]
            .as_str()
            .unwrap()
            .contains("PackageId")
    );
    assert_eq!(
        client.request("syrox/readSource", json!({"uri":"file:///etc/passwd"}))["error"]["code"],
        -32602
    );
    client.notify(
        "textDocument/didChange",
        json!({"textDocument":{"uri":document,"version":2},"contentChanges":[{"text":text}]}),
    );
    client.diagnostics(&document, 2);
    client.diagnostics(&document, 2);
    assert_eq!(
        client.request("syrox/readSource", json!({"uri":virtual_uri}))["error"]["code"],
        -32602
    );
    client.stop();
}
