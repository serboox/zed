use crate::{
    lsp_command::make_lsp_text_document_position, lsp_store::LspStore,
    project_settings::ProjectSettings,
};
use anyhow::{Context as _, Result};
use client::{TypedEnvelope, proto};
use gpui::{App, AsyncApp, Entity, SharedString, Task, TaskExt as _};
use language::{
    Anchor, Bias, Buffer, File as _, Location, PointUtf16, SymbolKind, lsp_to_symbol_kind,
    point_from_lsp,
    proto::{
        deserialize_anchor, deserialize_anchor_range, deserialize_version, serialize_anchor,
        serialize_anchor_range, serialize_version,
    },
};
use lsp::{LanguageServer, LanguageServerId};
use rpc::AnyProtoClient;
use serde_json::Value;
use settings::Settings as _;
use std::{ops::Range, path::PathBuf, sync::Arc, time::Duration};
use text::{BufferId, ToPointUtf16 as _};

/// Outcome of a call or type hierarchy request, keeping "the server cannot do
/// this" distinguishable from "the server did it and found nothing".
#[derive(Debug, Clone)]
pub enum HierarchyOutcome<T> {
    /// No language server attached to the buffer advertises this capability.
    Unsupported,
    /// A capable server answered the request with an empty result.
    NoResults,
    /// A capable server answered the request with these results.
    Found(Vec<T>),
}

impl<T> HierarchyOutcome<T> {
    /// The outcome a response carries: a host that found no capable server says
    /// so outright, and a capable server that answered with nothing is the
    /// empty list. Keeping the two apart is what lets the view say "this
    /// language cannot do it" rather than "nothing found".
    pub fn from_parts(supported: bool, items: Vec<T>) -> Self {
        if !supported {
            Self::Unsupported
        } else if items.is_empty() {
            Self::NoResults
        } else {
            Self::Found(items)
        }
    }

    pub fn into_parts(self) -> (bool, Vec<T>) {
        match self {
            Self::Unsupported => (false, Vec::new()),
            Self::NoResults => (true, Vec::new()),
            Self::Found(items) => (true, items),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CallHierarchyItem {
    pub name: SharedString,
    pub kind: SymbolKind,
    pub detail: Option<SharedString>,
    pub location: Location,
    pub selection_range: Range<Anchor>,
    pub language_server_id: LanguageServerId,
    source: lsp::CallHierarchyItem,
}

#[derive(Debug, Clone)]
pub struct CallHierarchyIncomingCall {
    pub from: CallHierarchyItem,
    pub from_ranges: Vec<Range<Anchor>>,
}

#[derive(Debug, Clone)]
pub struct CallHierarchyOutgoingCall {
    pub to: CallHierarchyItem,
    pub from_ranges: Vec<Range<Anchor>>,
}

#[derive(Debug, Clone)]
pub struct TypeHierarchyItem {
    pub name: SharedString,
    pub kind: SymbolKind,
    pub detail: Option<SharedString>,
    pub location: Location,
    pub selection_range: Range<Anchor>,
    pub language_server_id: LanguageServerId,
    source: lsp::TypeHierarchyItem,
}

pub async fn prepare_call_hierarchy(
    lsp_store: &Entity<LspStore>,
    buffer: &Entity<Buffer>,
    position: PointUtf16,
    cx: &mut AsyncApp,
) -> Result<HierarchyOutcome<CallHierarchyItem>> {
    if let Some((client, project_id)) = upstream_of(lsp_store, cx) {
        let (buffer_id, position, version) = position_request(buffer, position, cx);
        let response = client
            .request(proto::PrepareCallHierarchy {
                project_id,
                buffer_id,
                position,
                version,
            })
            .await?;
        let items = call_items_from_proto(lsp_store, response.items, cx).await?;
        return Ok(HierarchyOutcome::from_parts(response.supported, items));
    }

    let Some(language_server) =
        find_capable_language_server(lsp_store, buffer, call_hierarchy_capability, cx)
    else {
        return Ok(HierarchyOutcome::Unsupported);
    };

    let path = buffer.read_with(cx, |buffer, cx| buffer_abs_path(buffer, cx))?;
    let params = lsp::CallHierarchyPrepareParams {
        text_document_position_params: make_lsp_text_document_position(&path, position)?,
        work_done_progress_params: Default::default(),
    };

    let timeout = request_timeout(lsp_store, cx);
    let response = language_server
        .request::<lsp::request::CallHierarchyPrepare>(params, timeout)
        .await
        .into_response()
        .context("prepare call hierarchy request failed")?;

    let Some(items) = response.filter(|items| !items.is_empty()) else {
        return Ok(HierarchyOutcome::NoResults);
    };

    let server_id = language_server.server_id();
    let mut resolved = Vec::with_capacity(items.len());
    for raw_item in items {
        resolved.push(resolve_call_hierarchy_item(lsp_store, server_id, raw_item, cx).await?);
    }
    Ok(HierarchyOutcome::Found(resolved))
}

pub async fn incoming_calls(
    lsp_store: &Entity<LspStore>,
    item: &CallHierarchyItem,
    cx: &mut AsyncApp,
) -> Result<HierarchyOutcome<CallHierarchyIncomingCall>> {
    if let Some((client, project_id)) = upstream_of(lsp_store, cx) {
        let outcome = remote_calls(lsp_store, item, false, client, project_id, cx).await?;
        return Ok(map_outcome(outcome, |(from, from_ranges)| {
            CallHierarchyIncomingCall { from, from_ranges }
        }));
    }

    let language_server = lsp_store
        .read_with(cx, |lsp_store, _| {
            lsp_store.language_server_for_id(item.language_server_id)
        })
        .context("language server for this call hierarchy item is no longer running")?;

    let params = lsp::CallHierarchyIncomingCallsParams {
        item: item.source.clone(),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    let timeout = request_timeout(lsp_store, cx);
    let response = language_server
        .request::<lsp::request::CallHierarchyIncomingCalls>(params, timeout)
        .await
        .into_response()
        .context("incoming calls request failed")?;

    let Some(calls) = response.filter(|calls| !calls.is_empty()) else {
        return Ok(HierarchyOutcome::NoResults);
    };

    let mut resolved = Vec::with_capacity(calls.len());
    for call in calls {
        let from =
            resolve_call_hierarchy_item(lsp_store, item.language_server_id, call.from, cx).await?;
        let from_ranges = anchor_ranges_for(&from.location.buffer, call.from_ranges, cx);
        resolved.push(CallHierarchyIncomingCall { from, from_ranges });
    }
    Ok(HierarchyOutcome::Found(resolved))
}

pub async fn outgoing_calls(
    lsp_store: &Entity<LspStore>,
    item: &CallHierarchyItem,
    cx: &mut AsyncApp,
) -> Result<HierarchyOutcome<CallHierarchyOutgoingCall>> {
    if let Some((client, project_id)) = upstream_of(lsp_store, cx) {
        let outcome = remote_calls(lsp_store, item, true, client, project_id, cx).await?;
        return Ok(map_outcome(outcome, |(to, from_ranges)| {
            CallHierarchyOutgoingCall { to, from_ranges }
        }));
    }

    let language_server = lsp_store
        .read_with(cx, |lsp_store, _| {
            lsp_store.language_server_for_id(item.language_server_id)
        })
        .context("language server for this call hierarchy item is no longer running")?;

    let params = lsp::CallHierarchyOutgoingCallsParams {
        item: item.source.clone(),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    let timeout = request_timeout(lsp_store, cx);
    let response = language_server
        .request::<lsp::request::CallHierarchyOutgoingCalls>(params, timeout)
        .await
        .into_response()
        .context("outgoing calls request failed")?;

    let Some(calls) = response.filter(|calls| !calls.is_empty()) else {
        return Ok(HierarchyOutcome::NoResults);
    };

    // `from_ranges` here is relative to the queried item itself, not to `to`.
    let queried_buffer = item.location.buffer.clone();
    let mut resolved = Vec::with_capacity(calls.len());
    for call in calls {
        let to =
            resolve_call_hierarchy_item(lsp_store, item.language_server_id, call.to, cx).await?;
        let from_ranges = anchor_ranges_for(&queried_buffer, call.from_ranges, cx);
        resolved.push(CallHierarchyOutgoingCall { to, from_ranges });
    }
    Ok(HierarchyOutcome::Found(resolved))
}

pub async fn prepare_type_hierarchy(
    lsp_store: &Entity<LspStore>,
    buffer: &Entity<Buffer>,
    position: PointUtf16,
    cx: &mut AsyncApp,
) -> Result<HierarchyOutcome<TypeHierarchyItem>> {
    if let Some((client, project_id)) = upstream_of(lsp_store, cx) {
        let (buffer_id, position, version) = position_request(buffer, position, cx);
        let response = client
            .request(proto::PrepareTypeHierarchy {
                project_id,
                buffer_id,
                position,
                version,
            })
            .await?;
        let items = type_items_from_proto(lsp_store, response.items, cx).await?;
        return Ok(HierarchyOutcome::from_parts(response.supported, items));
    }

    let Some(language_server) =
        find_capable_language_server(lsp_store, buffer, type_hierarchy_capability, cx)
    else {
        return Ok(HierarchyOutcome::Unsupported);
    };

    let path = buffer.read_with(cx, |buffer, cx| buffer_abs_path(buffer, cx))?;
    let params = lsp::TypeHierarchyPrepareParams {
        text_document_position_params: make_lsp_text_document_position(&path, position)?,
        work_done_progress_params: Default::default(),
    };

    let timeout = request_timeout(lsp_store, cx);
    let response = language_server
        .request::<lsp::request::TypeHierarchyPrepare>(params, timeout)
        .await
        .into_response()
        .context("prepare type hierarchy request failed")?;

    let Some(items) = response.filter(|items| !items.is_empty()) else {
        return Ok(HierarchyOutcome::NoResults);
    };

    let server_id = language_server.server_id();
    let mut resolved = Vec::with_capacity(items.len());
    for raw_item in items {
        resolved.push(resolve_type_hierarchy_item(lsp_store, server_id, raw_item, cx).await?);
    }
    Ok(HierarchyOutcome::Found(resolved))
}

pub async fn supertypes(
    lsp_store: &Entity<LspStore>,
    item: &TypeHierarchyItem,
    cx: &mut AsyncApp,
) -> Result<HierarchyOutcome<TypeHierarchyItem>> {
    if let Some((client, project_id)) = upstream_of(lsp_store, cx) {
        return remote_relatives(lsp_store, item, false, client, project_id, cx).await;
    }

    let language_server = lsp_store
        .read_with(cx, |lsp_store, _| {
            lsp_store.language_server_for_id(item.language_server_id)
        })
        .context("language server for this type hierarchy item is no longer running")?;

    let params = lsp::TypeHierarchySupertypesParams {
        item: item.source.clone(),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    let timeout = request_timeout(lsp_store, cx);
    let response = language_server
        .request::<lsp::request::TypeHierarchySupertypes>(params, timeout)
        .await
        .into_response()
        .context("supertypes request failed")?;

    let Some(items) = response.filter(|items| !items.is_empty()) else {
        return Ok(HierarchyOutcome::NoResults);
    };

    let mut resolved = Vec::with_capacity(items.len());
    for raw_item in items {
        resolved.push(
            resolve_type_hierarchy_item(lsp_store, item.language_server_id, raw_item, cx).await?,
        );
    }
    Ok(HierarchyOutcome::Found(resolved))
}

pub async fn subtypes(
    lsp_store: &Entity<LspStore>,
    item: &TypeHierarchyItem,
    cx: &mut AsyncApp,
) -> Result<HierarchyOutcome<TypeHierarchyItem>> {
    if let Some((client, project_id)) = upstream_of(lsp_store, cx) {
        return remote_relatives(lsp_store, item, true, client, project_id, cx).await;
    }

    let language_server = lsp_store
        .read_with(cx, |lsp_store, _| {
            lsp_store.language_server_for_id(item.language_server_id)
        })
        .context("language server for this type hierarchy item is no longer running")?;

    let params = lsp::TypeHierarchySubtypesParams {
        item: item.source.clone(),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    let timeout = request_timeout(lsp_store, cx);
    let response = language_server
        .request::<lsp::request::TypeHierarchySubtypes>(params, timeout)
        .await
        .into_response()
        .context("subtypes request failed")?;

    let Some(items) = response.filter(|items| !items.is_empty()) else {
        return Ok(HierarchyOutcome::NoResults);
    };

    let mut resolved = Vec::with_capacity(items.len());
    for raw_item in items {
        resolved.push(
            resolve_type_hierarchy_item(lsp_store, item.language_server_id, raw_item, cx).await?,
        );
    }
    Ok(HierarchyOutcome::Found(resolved))
}

fn call_hierarchy_capability(server: &LanguageServer) -> bool {
    match server.capabilities().call_hierarchy_provider {
        None => false,
        Some(lsp::CallHierarchyServerCapability::Simple(supported)) => supported,
        Some(lsp::CallHierarchyServerCapability::Options(_)) => true,
    }
}

// `lsp-types` does not model every capability the LSP spec defines: there is no
// `type_hierarchy_provider` field on `ServerCapabilities` at all, even though the crate
// separately defines `TypeHierarchyOptions`/`TypeHierarchyRegistrationOptions`. A server's
// advertised `typeHierarchyProvider` would be silently dropped by serde while deserializing
// into the typed `ServerCapabilities`, so this reads the server's raw `initialize` response
// instead, via `LanguageServer::raw_capabilities`, which keeps exactly what the server sent.
fn type_hierarchy_capability(server: &LanguageServer) -> bool {
    let Some(raw_capabilities) = server.raw_capabilities() else {
        return false;
    };
    match raw_capabilities.get("typeHierarchyProvider") {
        Some(Value::Bool(supported)) => *supported,
        Some(Value::Object(_)) => true,
        _ => false,
    }
}

fn find_capable_language_server(
    lsp_store: &Entity<LspStore>,
    buffer: &Entity<Buffer>,
    supports: fn(&LanguageServer) -> bool,
    cx: &mut AsyncApp,
) -> Option<Arc<LanguageServer>> {
    lsp_store.update(cx, |lsp_store, cx| {
        buffer.update(cx, |buffer, cx| {
            lsp_store
                .language_servers_for_local_buffer(buffer, cx)
                .into_iter()
                .find_map(|server_id| {
                    let (_, server) =
                        lsp_store.language_server_for_local_buffer(buffer, server_id, cx)?;
                    supports(server).then(|| server.clone())
                })
        })
    })
}

fn request_timeout(lsp_store: &Entity<LspStore>, cx: &mut AsyncApp) -> Duration {
    lsp_store.read_with(cx, |_, cx| {
        ProjectSettings::get_global(cx)
            .global_lsp_settings
            .get_request_timeout()
    })
}

fn buffer_abs_path(buffer: &Buffer, cx: &App) -> Result<PathBuf> {
    worktree::File::from_dyn(buffer.file())
        .and_then(worktree::File::as_local)
        .map(|file| file.abs_path(cx))
        .context("buffer is not backed by a local file")
}

fn anchor_range_from_lsp(buffer: &Buffer, range: lsp::Range) -> Range<Anchor> {
    let start = buffer.clip_point_utf16(point_from_lsp(range.start), Bias::Left);
    let end = buffer.clip_point_utf16(point_from_lsp(range.end), Bias::Left);
    buffer.anchor_after(start)..buffer.anchor_before(end)
}

fn anchor_ranges_for(
    buffer: &Entity<Buffer>,
    ranges: Vec<lsp::Range>,
    cx: &mut AsyncApp,
) -> Vec<Range<Anchor>> {
    buffer.read_with(cx, |buffer, _| {
        ranges
            .into_iter()
            .map(|range| anchor_range_from_lsp(buffer, range))
            .collect()
    })
}

async fn resolve_call_hierarchy_item(
    lsp_store: &Entity<LspStore>,
    language_server_id: LanguageServerId,
    item: lsp::CallHierarchyItem,
    cx: &mut AsyncApp,
) -> Result<CallHierarchyItem> {
    let target_buffer = lsp_store
        .update(cx, |lsp_store, cx| {
            lsp_store.open_local_buffer_via_lsp(item.uri.clone(), language_server_id, cx)
        })
        .await?;

    let (range, selection_range) = target_buffer.read_with(cx, |buffer, _| {
        (
            anchor_range_from_lsp(buffer, item.range),
            anchor_range_from_lsp(buffer, item.selection_range),
        )
    });

    Ok(CallHierarchyItem {
        name: item.name.clone().into(),
        kind: lsp_to_symbol_kind(item.kind),
        detail: item.detail.clone().map(SharedString::from),
        location: Location {
            buffer: target_buffer,
            range,
        },
        selection_range,
        language_server_id,
        source: item,
    })
}

async fn resolve_type_hierarchy_item(
    lsp_store: &Entity<LspStore>,
    language_server_id: LanguageServerId,
    item: lsp::TypeHierarchyItem,
    cx: &mut AsyncApp,
) -> Result<TypeHierarchyItem> {
    let target_buffer = lsp_store
        .update(cx, |lsp_store, cx| {
            lsp_store.open_local_buffer_via_lsp(item.uri.clone(), language_server_id, cx)
        })
        .await?;

    let (range, selection_range) = target_buffer.read_with(cx, |buffer, _| {
        (
            anchor_range_from_lsp(buffer, item.range),
            anchor_range_from_lsp(buffer, item.selection_range),
        )
    });

    Ok(TypeHierarchyItem {
        name: item.name.clone().into(),
        kind: lsp_to_symbol_kind(item.kind),
        detail: item.detail.clone().map(SharedString::from),
        location: Location {
            buffer: target_buffer,
            range,
        },
        selection_range,
        language_server_id,
        source: item,
    })
}

// A guest in a collaborative session has no language server of its own, so
// every request below travels to the host, which owns the servers. What comes
// back is expressed in things a guest can hold: buffers the host has already
// shared with it, anchors into those buffers, and the server's own item kept
// verbatim, which is what the follow-up question has to be asked with.

fn upstream_of(lsp_store: &Entity<LspStore>, cx: &mut AsyncApp) -> Option<(AnyProtoClient, u64)> {
    lsp_store.read_with(cx, |lsp_store, _| lsp_store.upstream_client())
}

fn hierarchy_item_to_proto<S: serde::Serialize>(
    location: &Location,
    selection_range: &Range<Anchor>,
    language_server_id: LanguageServerId,
    source: &S,
    cx: &App,
) -> Result<proto::HierarchyItem> {
    Ok(proto::HierarchyItem {
        buffer_id: location.buffer.read(cx).remote_id().into(),
        range: Some(serialize_anchor_range(location.range.clone())),
        selection_range: Some(serialize_anchor_range(selection_range.clone())),
        language_server_id: language_server_id.0 as u64,
        lsp_item: serde_json::to_vec(source).context("serializing a hierarchy item")?,
    })
}

/// Makes an item's buffer reachable for the guest the answer is going to.
/// Without this the guest waits forever for a buffer that was never sent.
fn share_buffer_of(
    location: &Location,
    lsp_store: &mut LspStore,
    peer_id: proto::PeerId,
    cx: &mut App,
) {
    lsp_store
        .buffer_store()
        .update(cx, |buffer_store, cx| {
            buffer_store.create_buffer_for_peer(&location.buffer, peer_id, cx)
        })
        .detach_and_log_err(cx);
}

/// The buffer an item names. A guest waits for the host to send it; a host
/// already has it open, and waiting there would be waiting on itself.
fn buffer_named(
    lsp_store: &Entity<LspStore>,
    buffer_id: BufferId,
    cx: &mut AsyncApp,
) -> Task<Result<Entity<Buffer>>> {
    lsp_store.update(cx, |lsp_store, cx| {
        if lsp_store.upstream_client().is_some() {
            lsp_store.buffer_store().update(cx, |buffer_store, cx| {
                buffer_store.wait_for_remote_buffer(buffer_id, cx)
            })
        } else {
            Task::ready(lsp_store.buffer_store().read(cx).get_existing(buffer_id))
        }
    })
}

async fn hierarchy_item_from_proto<S: serde::de::DeserializeOwned>(
    lsp_store: &Entity<LspStore>,
    item: proto::HierarchyItem,
    cx: &mut AsyncApp,
) -> Result<(Location, Range<Anchor>, LanguageServerId, S)> {
    let buffer_id = BufferId::new(item.buffer_id)?;
    let buffer = buffer_named(lsp_store, buffer_id, cx).await?;
    let range = deserialize_anchor_range(item.range.context("a hierarchy item without a range")?)?;
    let selection_range = deserialize_anchor_range(
        item.selection_range
            .context("a hierarchy item without a selection range")?,
    )?;
    let source = serde_json::from_slice(&item.lsp_item)
        .context("deserializing a hierarchy item the server sent")?;
    Ok((
        Location { buffer, range },
        selection_range,
        LanguageServerId::from_proto(item.language_server_id),
        source,
    ))
}

/// One call hierarchy item as it crosses the wire. What travels is a place in a
/// buffer plus the server's own item verbatim, because that item is what the
/// follow-up question has to be asked with.
pub fn call_hierarchy_item_to_proto(
    item: &CallHierarchyItem,
    cx: &App,
) -> Result<proto::HierarchyItem> {
    hierarchy_item_to_proto(
        &item.location,
        &item.selection_range,
        item.language_server_id,
        &item.source,
        cx,
    )
}

pub async fn call_hierarchy_item_from_proto(
    lsp_store: &Entity<LspStore>,
    item: proto::HierarchyItem,
    cx: &mut AsyncApp,
) -> Result<CallHierarchyItem> {
    let (location, selection_range, server_id, source) =
        hierarchy_item_from_proto(lsp_store, item, cx).await?;
    Ok(call_item_from_parts(
        location,
        selection_range,
        server_id,
        source,
    ))
}

pub fn type_hierarchy_item_to_proto(
    item: &TypeHierarchyItem,
    cx: &App,
) -> Result<proto::HierarchyItem> {
    hierarchy_item_to_proto(
        &item.location,
        &item.selection_range,
        item.language_server_id,
        &item.source,
        cx,
    )
}

pub async fn type_hierarchy_item_from_proto(
    lsp_store: &Entity<LspStore>,
    item: proto::HierarchyItem,
    cx: &mut AsyncApp,
) -> Result<TypeHierarchyItem> {
    let (location, selection_range, server_id, source) =
        hierarchy_item_from_proto(lsp_store, item, cx).await?;
    Ok(type_item_from_parts(
        location,
        selection_range,
        server_id,
        source,
    ))
}

fn call_item_from_parts(
    location: Location,
    selection_range: Range<Anchor>,
    language_server_id: LanguageServerId,
    source: lsp::CallHierarchyItem,
) -> CallHierarchyItem {
    CallHierarchyItem {
        name: source.name.clone().into(),
        kind: lsp_to_symbol_kind(source.kind),
        detail: source.detail.clone().map(SharedString::from),
        location,
        selection_range,
        language_server_id,
        source,
    }
}

fn type_item_from_parts(
    location: Location,
    selection_range: Range<Anchor>,
    language_server_id: LanguageServerId,
    source: lsp::TypeHierarchyItem,
) -> TypeHierarchyItem {
    TypeHierarchyItem {
        name: source.name.clone().into(),
        kind: lsp_to_symbol_kind(source.kind),
        detail: source.detail.clone().map(SharedString::from),
        location,
        selection_range,
        language_server_id,
        source,
    }
}

async fn call_items_from_proto(
    lsp_store: &Entity<LspStore>,
    items: Vec<proto::HierarchyItem>,
    cx: &mut AsyncApp,
) -> Result<Vec<CallHierarchyItem>> {
    let mut resolved = Vec::with_capacity(items.len());
    for item in items {
        resolved.push(call_hierarchy_item_from_proto(lsp_store, item, cx).await?);
    }
    Ok(resolved)
}

async fn type_items_from_proto(
    lsp_store: &Entity<LspStore>,
    items: Vec<proto::HierarchyItem>,
    cx: &mut AsyncApp,
) -> Result<Vec<TypeHierarchyItem>> {
    let mut resolved = Vec::with_capacity(items.len());
    for item in items {
        resolved.push(type_hierarchy_item_from_proto(lsp_store, item, cx).await?);
    }
    Ok(resolved)
}

fn position_request(
    buffer: &Entity<Buffer>,
    position: PointUtf16,
    cx: &mut AsyncApp,
) -> (u64, Option<proto::Anchor>, Vec<proto::VectorClockEntry>) {
    buffer.read_with(cx, |buffer, _| {
        (
            buffer.remote_id().into(),
            Some(serialize_anchor(&buffer.anchor_before(position))),
            serialize_version(&buffer.version()),
        )
    })
}

async fn remote_calls(
    lsp_store: &Entity<LspStore>,
    item: &CallHierarchyItem,
    outgoing: bool,
    client: AnyProtoClient,
    project_id: u64,
    cx: &mut AsyncApp,
) -> Result<HierarchyOutcome<(CallHierarchyItem, Vec<Range<Anchor>>)>> {
    let proto_item = lsp_store.read_with(cx, |_, cx| call_hierarchy_item_to_proto(item, cx))?;
    let response = client
        .request(proto::CallHierarchyCalls {
            project_id,
            item: Some(proto_item),
            outgoing,
        })
        .await?;

    let mut calls = Vec::with_capacity(response.calls.len());
    for call in response.calls {
        let item = call_hierarchy_item_from_proto(
            lsp_store,
            call.item.context("a hierarchy call without an item")?,
            cx,
        )
        .await?;
        let from_ranges = call
            .from_ranges
            .into_iter()
            .map(deserialize_anchor_range)
            .collect::<Result<Vec<_>>>()?;
        calls.push((item, from_ranges));
    }
    Ok(HierarchyOutcome::from_parts(response.supported, calls))
}

async fn remote_relatives(
    lsp_store: &Entity<LspStore>,
    item: &TypeHierarchyItem,
    subtypes: bool,
    client: AnyProtoClient,
    project_id: u64,
    cx: &mut AsyncApp,
) -> Result<HierarchyOutcome<TypeHierarchyItem>> {
    let proto_item = lsp_store.read_with(cx, |_, cx| type_hierarchy_item_to_proto(item, cx))?;
    let response = client
        .request(proto::TypeHierarchyRelatives {
            project_id,
            item: Some(proto_item),
            subtypes,
        })
        .await?;
    let items = type_items_from_proto(lsp_store, response.items, cx).await?;
    Ok(HierarchyOutcome::from_parts(response.supported, items))
}

/// The buffer and position a `prepare` request names, ready for the local path
/// to run over. The version wait is what keeps the host from answering about
/// text the guest has already changed underneath it.
async fn buffer_and_position(
    lsp_store: &Entity<LspStore>,
    buffer_id: u64,
    position: Option<proto::Anchor>,
    version: Vec<proto::VectorClockEntry>,
    cx: &mut AsyncApp,
) -> Result<(Entity<Buffer>, PointUtf16)> {
    let buffer_id = BufferId::new(buffer_id)?;
    let buffer = lsp_store.update(cx, |lsp_store, cx| {
        lsp_store.buffer_store().read(cx).get_existing(buffer_id)
    })?;
    let position = position
        .and_then(deserialize_anchor)
        .context("a hierarchy request without a position")?;
    buffer
        .update(cx, |buffer, _| {
            buffer.wait_for_version(deserialize_version(&version))
        })
        .await?;
    let position = buffer.read_with(cx, |buffer, _| position.to_point_utf16(buffer));
    Ok((buffer, position))
}

pub async fn handle_prepare_call_hierarchy(
    lsp_store: Entity<LspStore>,
    envelope: TypedEnvelope<proto::PrepareCallHierarchy>,
    mut cx: AsyncApp,
) -> Result<proto::PrepareCallHierarchyResponse> {
    let peer_id = envelope.original_sender_id.unwrap_or(envelope.sender_id);
    let payload = envelope.payload;
    let (buffer, position) = buffer_and_position(
        &lsp_store,
        payload.buffer_id,
        payload.position,
        payload.version,
        &mut cx,
    )
    .await?;
    let outcome = prepare_call_hierarchy(&lsp_store, &buffer, position, &mut cx).await?;
    let (supported, found) = outcome.into_parts();
    let items = lsp_store.update(&mut cx, |lsp_store, cx| {
        found
            .iter()
            .map(|item| {
                share_buffer_of(&item.location, lsp_store, peer_id, cx);
                call_hierarchy_item_to_proto(item, cx)
            })
            .collect::<Result<Vec<_>>>()
    })?;
    Ok(proto::PrepareCallHierarchyResponse { supported, items })
}

pub async fn handle_call_hierarchy_calls(
    lsp_store: Entity<LspStore>,
    envelope: TypedEnvelope<proto::CallHierarchyCalls>,
    mut cx: AsyncApp,
) -> Result<proto::CallHierarchyCallsResponse> {
    let peer_id = envelope.original_sender_id.unwrap_or(envelope.sender_id);
    let payload = envelope.payload;
    let outgoing = payload.outgoing;
    let item = call_hierarchy_item_from_proto(
        &lsp_store,
        payload.item.context("a calls request without an item")?,
        &mut cx,
    )
    .await?;

    let outcome = if outgoing {
        let found = outgoing_calls(&lsp_store, &item, &mut cx).await?;
        map_outcome(found, |call| (call.to, call.from_ranges))
    } else {
        let found = incoming_calls(&lsp_store, &item, &mut cx).await?;
        map_outcome(found, |call| (call.from, call.from_ranges))
    };
    let (supported, found) = outcome.into_parts();

    let calls = lsp_store.update(&mut cx, |lsp_store, cx| {
        found
            .iter()
            .map(|(item, from_ranges)| {
                share_buffer_of(&item.location, lsp_store, peer_id, cx);
                anyhow::Ok(proto::HierarchyCall {
                    item: Some(call_hierarchy_item_to_proto(item, cx)?),
                    from_ranges: from_ranges
                        .iter()
                        .cloned()
                        .map(serialize_anchor_range)
                        .collect(),
                })
            })
            .collect::<Result<Vec<_>>>()
    })?;
    Ok(proto::CallHierarchyCallsResponse { supported, calls })
}

pub async fn handle_prepare_type_hierarchy(
    lsp_store: Entity<LspStore>,
    envelope: TypedEnvelope<proto::PrepareTypeHierarchy>,
    mut cx: AsyncApp,
) -> Result<proto::PrepareTypeHierarchyResponse> {
    let peer_id = envelope.original_sender_id.unwrap_or(envelope.sender_id);
    let payload = envelope.payload;
    let (buffer, position) = buffer_and_position(
        &lsp_store,
        payload.buffer_id,
        payload.position,
        payload.version,
        &mut cx,
    )
    .await?;
    let outcome = prepare_type_hierarchy(&lsp_store, &buffer, position, &mut cx).await?;
    let (supported, items) = shared_type_items(&lsp_store, outcome, peer_id, &mut cx)?;
    Ok(proto::PrepareTypeHierarchyResponse { supported, items })
}

pub async fn handle_type_hierarchy_relatives(
    lsp_store: Entity<LspStore>,
    envelope: TypedEnvelope<proto::TypeHierarchyRelatives>,
    mut cx: AsyncApp,
) -> Result<proto::TypeHierarchyRelativesResponse> {
    let peer_id = envelope.original_sender_id.unwrap_or(envelope.sender_id);
    let payload = envelope.payload;
    let wants_subtypes = payload.subtypes;
    let item = type_hierarchy_item_from_proto(
        &lsp_store,
        payload
            .item
            .context("a relatives request without an item")?,
        &mut cx,
    )
    .await?;

    let outcome = if wants_subtypes {
        subtypes(&lsp_store, &item, &mut cx).await?
    } else {
        supertypes(&lsp_store, &item, &mut cx).await?
    };
    let (supported, items) = shared_type_items(&lsp_store, outcome, peer_id, &mut cx)?;
    Ok(proto::TypeHierarchyRelativesResponse { supported, items })
}

fn shared_type_items(
    lsp_store: &Entity<LspStore>,
    outcome: HierarchyOutcome<TypeHierarchyItem>,
    peer_id: proto::PeerId,
    cx: &mut AsyncApp,
) -> Result<(bool, Vec<proto::HierarchyItem>)> {
    let (supported, found) = outcome.into_parts();
    let items = lsp_store.update(cx, |lsp_store, cx| {
        found
            .iter()
            .map(|item| {
                share_buffer_of(&item.location, lsp_store, peer_id, cx);
                type_hierarchy_item_to_proto(item, cx)
            })
            .collect::<Result<Vec<_>>>()
    })?;
    Ok((supported, items))
}

fn map_outcome<T, U>(outcome: HierarchyOutcome<T>, of: impl Fn(T) -> U) -> HierarchyOutcome<U> {
    match outcome {
        HierarchyOutcome::Unsupported => HierarchyOutcome::Unsupported,
        HierarchyOutcome::NoResults => HierarchyOutcome::NoResults,
        HierarchyOutcome::Found(items) => {
            HierarchyOutcome::Found(items.into_iter().map(of).collect())
        }
    }
}
