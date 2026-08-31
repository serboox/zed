use crate::{
    lsp_command::make_lsp_text_document_position, lsp_store::LspStore,
    project_settings::ProjectSettings,
};
use anyhow::{Context as _, Result};
use gpui::{App, AsyncApp, Entity, SharedString};
use language::{
    Anchor, Bias, Buffer, File as _, LocalFile, Location, PointUtf16, SymbolKind, lsp_to_symbol_kind,
    point_from_lsp,
};
use lsp::{LanguageServer, LanguageServerId};
use serde_json::Value;
use settings::Settings as _;
use std::{ops::Range, path::PathBuf, sync::Arc, time::Duration};

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
        let to = resolve_call_hierarchy_item(lsp_store, item.language_server_id, call.to, cx).await?;
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
        resolved
            .push(resolve_type_hierarchy_item(lsp_store, item.language_server_id, raw_item, cx).await?);
    }
    Ok(HierarchyOutcome::Found(resolved))
}

pub async fn subtypes(
    lsp_store: &Entity<LspStore>,
    item: &TypeHierarchyItem,
    cx: &mut AsyncApp,
) -> Result<HierarchyOutcome<TypeHierarchyItem>> {
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
        resolved
            .push(resolve_type_hierarchy_item(lsp_store, item.language_server_id, raw_item, cx).await?);
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
