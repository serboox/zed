use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use fs::FakeFs;
use futures::StreamExt as _;
use gpui::TestAppContext;
use language::{FakeLspAdapter, OffsetRangeExt, PointUtf16, SymbolKind, rust_lang};
use project::{
    Project,
    hierarchies::{
        HierarchyOutcome, call_hierarchy_item_from_proto, call_hierarchy_item_to_proto,
        incoming_calls, prepare_call_hierarchy, prepare_type_hierarchy,
    },
};
use serde_json::json;
use util::path;

use crate::init_test;

/// Same wire request as `lsp::request::Initialize`, but with a response we build by hand as
/// raw JSON. Used to give a fake server a `typeHierarchyProvider` capability, which
/// `lsp::ServerCapabilities` has no field for at all.
enum InitializeWithTypeHierarchy {}

impl lsp::request::Request for InitializeWithTypeHierarchy {
    type Params = lsp::InitializeParams;
    type Result = serde_json::Value;
    const METHOD: &'static str = "initialize";
}

fn type_hierarchy_capable_adapter() -> FakeLspAdapter {
    FakeLspAdapter {
        capabilities: lsp::ServerCapabilities::default(),
        initializer: Some(Box::new(|fake_server| {
            fake_server.set_request_handler::<InitializeWithTypeHierarchy, _, _>(|_, _| async {
                Ok(json!({
                    "capabilities": {
                        "typeHierarchyProvider": true
                    }
                }))
            });
        })),
        ..Default::default()
    }
}

fn call_item(
    name: &str,
    kind: lsp::SymbolKind,
    uri: lsp::Uri,
    line: u32,
) -> lsp::CallHierarchyItem {
    let range = lsp::Range::new(lsp::Position::new(line, 0), lsp::Position::new(line, 10));
    lsp::CallHierarchyItem {
        name: name.to_string(),
        kind,
        tags: None,
        detail: None,
        uri,
        range,
        selection_range: range,
        data: None,
    }
}

fn type_item(
    name: &str,
    kind: lsp::SymbolKind,
    uri: lsp::Uri,
    line: u32,
) -> lsp::TypeHierarchyItem {
    let range = lsp::Range::new(lsp::Position::new(line, 0), lsp::Position::new(line, 10));
    lsp::TypeHierarchyItem {
        name: name.to_string(),
        kind,
        tags: None,
        detail: None,
        uri,
        range,
        selection_range: range,
        data: None,
    }
}

#[gpui::test]
async fn test_incoming_calls_found(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/dir"),
        json!({
            "a.rs": "fn callee() {}\nfn caller() { callee(); }\n",
        }),
    )
    .await;

    let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_language_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            capabilities: lsp::ServerCapabilities {
                call_hierarchy_provider: Some(lsp::CallHierarchyServerCapability::Simple(true)),
                ..Default::default()
            },
            ..Default::default()
        },
    );

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/a.rs"), cx)
        })
        .await
        .unwrap();
    let fake_server = fake_language_servers.next().await.unwrap();
    cx.run_until_parked();

    let callee_uri = lsp::Uri::from_file_path(path!("/dir/a.rs")).unwrap();
    let prepare_item = call_item("callee", lsp::SymbolKind::FUNCTION, callee_uri, 0);
    fake_server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>({
        let prepare_item = prepare_item.clone();
        move |_, _| {
            let prepare_item = prepare_item.clone();
            async move { Ok(Some(vec![prepare_item])) }
        }
    });

    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
    let outcome = prepare_call_hierarchy(
        &lsp_store,
        &buffer,
        PointUtf16::new(0, 3),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    let items = match outcome {
        HierarchyOutcome::Found(items) => items,
        other => panic!("expected a resolved call hierarchy item, got {other:?}"),
    };
    assert_eq!(items.len(), 1);
    let callee = items.into_iter().next().unwrap();
    assert_eq!(callee.name.as_ref(), "callee");

    let caller_uri = lsp::Uri::from_file_path(path!("/dir/a.rs")).unwrap();
    let caller_item = call_item("caller", lsp::SymbolKind::FUNCTION, caller_uri, 1);
    let from_range = lsp::Range::new(lsp::Position::new(1, 14), lsp::Position::new(1, 20));
    fake_server.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>({
        let caller_item = caller_item.clone();
        move |_, _| {
            let caller_item = caller_item.clone();
            async move {
                Ok(Some(vec![lsp::CallHierarchyIncomingCall {
                    from: caller_item,
                    from_ranges: vec![from_range],
                }]))
            }
        }
    });

    let outcome = incoming_calls(&lsp_store, &callee, &mut cx.to_async())
        .await
        .unwrap();
    let calls = match outcome {
        HierarchyOutcome::Found(calls) => calls,
        other => panic!("expected incoming calls, got {other:?}"),
    };
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.from.name.as_ref(), "caller");
    assert_eq!(call.from.kind, SymbolKind::Function);
    assert_eq!(call.from_ranges.len(), 1);
    let snapshot = call
        .from
        .location
        .buffer
        .read_with(cx, |buffer, _| buffer.snapshot());
    let range = call.from_ranges[0].to_point_utf16(&snapshot);
    assert_eq!(range, PointUtf16::new(1, 14)..PointUtf16::new(1, 20));
}

#[gpui::test]
async fn test_incoming_calls_empty(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/dir"),
        json!({
            "a.rs": "fn callee() {}\n",
        }),
    )
    .await;

    let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_language_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            capabilities: lsp::ServerCapabilities {
                call_hierarchy_provider: Some(lsp::CallHierarchyServerCapability::Simple(true)),
                ..Default::default()
            },
            ..Default::default()
        },
    );

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/a.rs"), cx)
        })
        .await
        .unwrap();
    let fake_server = fake_language_servers.next().await.unwrap();
    cx.run_until_parked();

    fake_server
        .set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>(|_, _| async { Ok(None) });

    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
    let outcome = prepare_call_hierarchy(
        &lsp_store,
        &buffer,
        PointUtf16::new(0, 3),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, HierarchyOutcome::NoResults));
}

#[gpui::test]
async fn test_call_hierarchy_unsupported_sends_no_request(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/dir"),
        json!({
            "a.rs": "fn callee() {}\n",
        }),
    )
    .await;

    let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_language_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            capabilities: lsp::ServerCapabilities {
                call_hierarchy_provider: None,
                ..Default::default()
            },
            ..Default::default()
        },
    );

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/a.rs"), cx)
        })
        .await
        .unwrap();
    let fake_server = fake_language_servers.next().await.unwrap();
    cx.run_until_parked();

    let request_received = Arc::new(AtomicBool::new(false));
    fake_server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>({
        let request_received = request_received.clone();
        move |_, _| {
            request_received.store(true, Ordering::SeqCst);
            async { Ok(None) }
        }
    });

    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
    let outcome = prepare_call_hierarchy(
        &lsp_store,
        &buffer,
        PointUtf16::new(0, 3),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    cx.run_until_parked();

    assert!(matches!(outcome, HierarchyOutcome::Unsupported));
    assert!(
        !request_received.load(Ordering::SeqCst),
        "prepareCallHierarchy must not be sent when the server does not advertise the capability"
    );
}

#[gpui::test]
async fn test_type_hierarchy_unsupported_sends_no_request(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/dir"),
        json!({
            "a.rs": "struct Base;\n",
        }),
    )
    .await;

    let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    // Plain default adapter: no `initializer` override, so the raw `initialize` response
    // this fake server sends has no `typeHierarchyProvider` key at all, matching a server
    // that genuinely does not support type hierarchy.
    let mut fake_language_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            capabilities: lsp::ServerCapabilities::default(),
            ..Default::default()
        },
    );

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/a.rs"), cx)
        })
        .await
        .unwrap();
    let fake_server = fake_language_servers.next().await.unwrap();
    cx.run_until_parked();

    let request_received = Arc::new(AtomicBool::new(false));
    fake_server.set_request_handler::<lsp::request::TypeHierarchyPrepare, _, _>({
        let request_received = request_received.clone();
        move |_, _| {
            request_received.store(true, Ordering::SeqCst);
            async { Ok(None) }
        }
    });

    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
    let outcome = prepare_type_hierarchy(
        &lsp_store,
        &buffer,
        PointUtf16::new(0, 7),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    cx.run_until_parked();

    assert!(matches!(outcome, HierarchyOutcome::Unsupported));
    assert!(
        !request_received.load(Ordering::SeqCst),
        "prepareTypeHierarchy must not be sent when the server does not advertise the capability"
    );
}

#[gpui::test]
async fn test_type_hierarchy_found(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/dir"),
        json!({
            "a.rs": "struct Base;\nstruct Derived;\n",
        }),
    )
    .await;

    let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_language_servers =
        language_registry.register_fake_lsp("Rust", type_hierarchy_capable_adapter());

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/a.rs"), cx)
        })
        .await
        .unwrap();
    let fake_server = fake_language_servers.next().await.unwrap();
    cx.run_until_parked();

    let uri = lsp::Uri::from_file_path(path!("/dir/a.rs")).unwrap();
    let base_item = type_item("Base", lsp::SymbolKind::STRUCT, uri, 0);
    fake_server.set_request_handler::<lsp::request::TypeHierarchyPrepare, _, _>({
        let base_item = base_item.clone();
        move |_, _| {
            let base_item = base_item.clone();
            async move { Ok(Some(vec![base_item])) }
        }
    });

    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
    let outcome = prepare_type_hierarchy(
        &lsp_store,
        &buffer,
        PointUtf16::new(0, 7),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    let items = match outcome {
        HierarchyOutcome::Found(items) => items,
        other => panic!("expected a resolved type hierarchy item, got {other:?}"),
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name.as_ref(), "Base");
    assert_eq!(items[0].kind, SymbolKind::Struct);
}

#[gpui::test]
async fn test_type_hierarchy_empty(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/dir"),
        json!({
            "a.rs": "struct Base;\n",
        }),
    )
    .await;

    let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_language_servers =
        language_registry.register_fake_lsp("Rust", type_hierarchy_capable_adapter());

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/a.rs"), cx)
        })
        .await
        .unwrap();
    let fake_server = fake_language_servers.next().await.unwrap();
    cx.run_until_parked();

    fake_server
        .set_request_handler::<lsp::request::TypeHierarchyPrepare, _, _>(|_, _| async { Ok(None) });

    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
    let outcome = prepare_type_hierarchy(
        &lsp_store,
        &buffer,
        PointUtf16::new(0, 7),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, HierarchyOutcome::NoResults));
}

/// The three outcomes must survive a round trip through what a response can
/// carry, because the view says something different for each: "this language
/// cannot do it", "nothing found", and a tree.
#[test]
fn the_three_outcomes_survive_being_taken_apart_and_put_back() {
    for outcome in [
        HierarchyOutcome::Unsupported,
        HierarchyOutcome::NoResults,
        HierarchyOutcome::Found(vec![1, 2, 3]),
    ] {
        let expected = format!("{outcome:?}");
        let (supported, items) = outcome.into_parts();
        let back = HierarchyOutcome::from_parts(supported, items);
        assert_eq!(format!("{back:?}"), expected);
    }
}

/// An item a guest holds is one the host sent over the wire. Everything the
/// view draws from it, and everything the follow-up question needs, has to
/// come back out of that wire form -- including the server's own item, which
/// the index of what a name means is not enough to rebuild.
#[gpui::test]
async fn a_call_hierarchy_item_survives_the_wire(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/dir"),
        json!({
            "a.rs": "fn callee() {}\nfn caller() { callee(); }\n",
        }),
    )
    .await;

    let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_language_servers = language_registry.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            capabilities: lsp::ServerCapabilities {
                call_hierarchy_provider: Some(lsp::CallHierarchyServerCapability::Simple(true)),
                ..Default::default()
            },
            ..Default::default()
        },
    );

    let (buffer, _handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/dir/a.rs"), cx)
        })
        .await
        .unwrap();
    let fake_server = fake_language_servers.next().await.unwrap();
    cx.run_until_parked();

    let uri = lsp::Uri::from_file_path(path!("/dir/a.rs")).unwrap();
    let mut prepared = call_item("callee", lsp::SymbolKind::FUNCTION, uri, 0);
    prepared.detail = Some("fn callee()".to_string());
    // Something only the server understands, which nothing on the guest side
    // could reconstruct from the name and the place alone.
    prepared.data = Some(json!({ "server_private": "opaque" }));
    fake_server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>({
        let prepared = prepared.clone();
        move |_, _| {
            let prepared = prepared.clone();
            async move { Ok(Some(vec![prepared])) }
        }
    });

    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
    let outcome = prepare_call_hierarchy(
        &lsp_store,
        &buffer,
        PointUtf16::new(0, 3),
        &mut cx.to_async(),
    )
    .await
    .unwrap();
    let original = match outcome {
        HierarchyOutcome::Found(items) => items.into_iter().next().unwrap(),
        other => panic!("expected a resolved call hierarchy item, got {other:?}"),
    };

    let on_the_wire = cx
        .update(|cx| call_hierarchy_item_to_proto(&original, cx))
        .unwrap();
    let recovered =
        call_hierarchy_item_from_proto(&lsp_store, on_the_wire.clone(), &mut cx.to_async())
            .await
            .unwrap();

    assert_eq!(recovered.name, original.name);
    assert_eq!(recovered.kind, original.kind);
    assert_eq!(recovered.detail, original.detail);
    assert_eq!(recovered.language_server_id, original.language_server_id);
    assert_eq!(recovered.location.buffer, original.location.buffer);
    assert_eq!(recovered.location.range, original.location.range);
    assert_eq!(recovered.selection_range, original.selection_range);
    assert_eq!(
        cx.update(|cx| call_hierarchy_item_to_proto(&recovered, cx))
            .unwrap(),
        on_the_wire,
        "an item put back on the wire is the same item"
    );

    // The strongest check: the follow-up question, asked with the recovered
    // item, reaches the server as the very item the server first sent.
    let asked_with: Arc<Mutex<Option<lsp::CallHierarchyItem>>> = Arc::new(Mutex::new(None));
    fake_server.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>({
        let asked_with = asked_with.clone();
        move |params, _| {
            *asked_with.lock().unwrap() = Some(params.item);
            async move { Ok(Some(Vec::new())) }
        }
    });
    let outcome = incoming_calls(&lsp_store, &recovered, &mut cx.to_async())
        .await
        .unwrap();
    assert!(matches!(outcome, HierarchyOutcome::NoResults));
    assert_eq!(
        asked_with.lock().unwrap().as_ref(),
        Some(&prepared),
        "the server's own item has to reach it unchanged, private data and all"
    );
}
