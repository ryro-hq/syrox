use super::*;

#[test]
fn watched_modules_rebuild_the_graph_and_reapply_unsaved_buffers() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir(root.join("recipes")).unwrap();
    std::fs::write(
        root.join("recipes/one.srx"),
        "pub value I(int); pub fn make() -> I { I(1) }",
    )
    .unwrap();
    let text =
        "inputs { lib = \"modules:recipes\"; } fn use_it() -> lib::one::I { lib::one::make() }";
    std::fs::write(root.join("main.srx"), text).unwrap();
    let document = uri(&root.join("main.srx"));
    let mut client = Client::with_watching(root, "utf-16", true);
    let registration = client.wait(|message| message["method"] == "client/registerCapability");
    assert_eq!(
        registration["params"]["registrations"][0]["method"],
        "workspace/didChangeWatchedFiles"
    );
    client.send(&json!({"jsonrpc":"2.0","id":registration["id"],"result":null}));
    client.open(&document, text);
    client.diagnostics(&document, 1);
    client.diagnostics(&document, 1);
    let changed = text.replace("one::make", "two::make");
    client.notify(
        "textDocument/didChange",
        json!({"textDocument":{"uri":document,"version":2},"contentChanges":[{"text":changed}]}),
    );
    client.diagnostics(&document, 2);
    assert!(
        !client.diagnostics(&document, 2)["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let added = root.join("recipes/two.srx");
    std::fs::write(&added, "pub fn make() -> one::I { one::I(2) }").unwrap();
    client.notify(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&added),"type":1}]}),
    );
    client.diagnostics(&document, 2);
    assert_eq!(
        client.diagnostics(&document, 2)["params"]["diagnostics"],
        json!([])
    );
    let definition = client.request(
        "textDocument/definition",
        json!({"textDocument":{"uri":document},"position":cursor(&changed,"lib::two::make")}),
    );
    assert_eq!(definition["result"]["uri"], uri(&added));
    assert_eq!(
        std::fs::read_to_string(root.join("main.srx")).unwrap(),
        text
    );
    client.stop();
}
