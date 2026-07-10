use std::sync::Arc;

use anyhow::Result;
use api_client::{
    Collection, CollectionId, Environment, EnvironmentId, Folder, FolderId, HistoryEntry, Request,
    RequestId,
};
use credentials_provider::CredentialsProvider;
use gpui::{App, AsyncApp, Context, Entity, EventEmitter, Global};
use serde::{Deserialize, Serialize};
use util::ResultExt;

const COLLECTIONS_FILE: &str = "api_collections.json";
const ENVIRONMENTS_FILE: &str = "api_environments.json";
const HISTORY_FILE: &str = "api_history.json";

/// History is a flat log, not a database -- cap it so a heavily used client
/// doesn't grow the JSON file without bound. Oldest entries are dropped
/// first once the cap is hit.
const MAX_HISTORY_ENTRIES: usize = 500;

/// A tree is at most this many folder levels deep, mirroring
/// `db_client::MAX_FOLDER_DEPTH` — kept as an independent constant since
/// `api_client` has no reason to depend on `db_client`.
pub const MAX_FOLDER_DEPTH: usize = 5;

#[derive(Default, Serialize, Deserialize)]
struct StoredCollections {
    #[serde(default)]
    collections: Vec<Collection>,
    #[serde(default)]
    folders: Vec<Folder>,
    #[serde(default)]
    requests: Vec<Request>,
}

#[derive(Serialize, Deserialize)]
struct StoredEnvironments {
    #[serde(default)]
    environments: Vec<Environment>,
    global: Environment,
}

impl Default for StoredEnvironments {
    fn default() -> Self {
        Self {
            environments: Vec::new(),
            global: Environment::global(),
        }
    }
}

fn collections_file_path() -> std::path::PathBuf {
    paths::config_dir().join(COLLECTIONS_FILE)
}

fn environments_file_path() -> std::path::PathBuf {
    paths::config_dir().join(ENVIRONMENTS_FILE)
}

fn history_file_path() -> std::path::PathBuf {
    paths::config_dir().join(HISTORY_FILE)
}

fn load_collections_from_disk() -> StoredCollections {
    std::fs::read(collections_file_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn load_environments_from_disk() -> StoredEnvironments {
    std::fs::read(environments_file_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn load_history_from_disk() -> Vec<HistoryEntry> {
    std::fs::read(history_file_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_collections_to_disk(stored: &StoredCollections) -> Result<()> {
    let json = serde_json::to_vec_pretty(stored)?;
    std::fs::write(collections_file_path(), json)?;
    Ok(())
}

fn save_environments_to_disk(stored: &StoredEnvironments) -> Result<()> {
    let json = serde_json::to_vec_pretty(stored)?;
    std::fs::write(environments_file_path(), json)?;
    Ok(())
}

fn save_history_to_disk(history: &[HistoryEntry]) -> Result<()> {
    let json = serde_json::to_vec_pretty(history)?;
    std::fs::write(history_file_path(), json)?;
    Ok(())
}

/// Keychain key for a request's auth secret (Basic password, Bearer token, or
/// API key value — whichever `AuthConfig` variant it holds). One entry per
/// request, since a request has at most one auth config at a time.
fn request_credentials_url(id: RequestId) -> String {
    format!("api_client://request/{id}")
}

/// Returns a copy of `request` with any inline auth secret cleared. The
/// on-disk JSON must never hold a plaintext secret — it lives in the OS
/// keychain instead, mirroring `db_client_ui::store::redact_password`.
fn redact_auth_secret(request: &Request) -> Request {
    use api_client::AuthConfig;
    let mut redacted = request.clone();
    redacted.auth = match redacted.auth {
        AuthConfig::Basic { username, .. } => AuthConfig::Basic {
            username,
            password: String::new(),
        },
        AuthConfig::Bearer { .. } => AuthConfig::Bearer {
            token: String::new(),
        },
        AuthConfig::ApiKey { key, placement, .. } => AuthConfig::ApiKey {
            key,
            value: String::new(),
            placement,
        },
        AuthConfig::OAuth2(mut oauth2) => {
            oauth2.client_secret.clear();
            oauth2.access_token.clear();
            oauth2.refresh_token.clear();
            AuthConfig::OAuth2(oauth2)
        }
        AuthConfig::AwsSigV4(mut aws) => {
            aws.secret_key.clear();
            aws.session_token.clear();
            AuthConfig::AwsSigV4(aws)
        }
        other => other,
    };
    redacted
}

/// OAuth2 has three secret-shaped fields (client secret, access token,
/// refresh token) instead of the single secret every other `AuthConfig`
/// variant carries -- packed as a small JSON blob so it still fits through
/// `CredentialsProvider`'s one-secret-per-entry API.
#[derive(Serialize, Deserialize, Default)]
struct OAuth2Secrets {
    client_secret: String,
    access_token: String,
    refresh_token: String,
}

/// AWS SigV4's secret-shaped fields (the long-lived secret key and, for
/// temporary STS credentials, the session token) -- access key, region, and
/// service are not secret and stay in the plain persisted `AuthConfig`.
#[derive(Serialize, Deserialize, Default)]
struct AwsSigV4Secrets {
    secret_key: String,
    session_token: String,
}

async fn store_request_secret(
    provider: &Arc<dyn CredentialsProvider>,
    request: &Request,
    cx: &AsyncApp,
) -> Result<()> {
    use api_client::AuthConfig;
    let (username, secret): (String, Vec<u8>) = match &request.auth {
        AuthConfig::Basic { username, password } if !password.is_empty() => {
            (username.clone(), password.as_bytes().to_vec())
        }
        AuthConfig::Bearer { token } if !token.is_empty() => {
            (String::new(), token.as_bytes().to_vec())
        }
        AuthConfig::ApiKey { value, .. } if !value.is_empty() => {
            (String::new(), value.as_bytes().to_vec())
        }
        AuthConfig::OAuth2(oauth2)
            if !oauth2.client_secret.is_empty()
                || !oauth2.access_token.is_empty()
                || !oauth2.refresh_token.is_empty() =>
        {
            let secrets = OAuth2Secrets {
                client_secret: oauth2.client_secret.clone(),
                access_token: oauth2.access_token.clone(),
                refresh_token: oauth2.refresh_token.clone(),
            };
            (String::new(), serde_json::to_vec(&secrets)?)
        }
        AuthConfig::AwsSigV4(aws)
            if !aws.secret_key.is_empty() || !aws.session_token.is_empty() =>
        {
            let secrets = AwsSigV4Secrets {
                secret_key: aws.secret_key.clone(),
                session_token: aws.session_token.clone(),
            };
            (String::new(), serde_json::to_vec(&secrets)?)
        }
        _ => return Ok(()),
    };
    provider
        .write_credentials(&request_credentials_url(request.id), &username, &secret, cx)
        .await
}

/// Reads back a request's auth secret and fills it into whichever `AuthConfig`
/// variant is already set on `request` (the variant/username/key themselves
/// are plain JSON fields, only the secret payload lives in the keychain).
async fn read_request_secret(
    provider: &Arc<dyn CredentialsProvider>,
    request: &mut Request,
    cx: &AsyncApp,
) -> Result<()> {
    use api_client::AuthConfig;
    let Some((username, secret)) = provider
        .read_credentials(&request_credentials_url(request.id), cx)
        .await?
    else {
        return Ok(());
    };
    match &mut request.auth {
        AuthConfig::Basic {
            username: u,
            password,
        } if password.is_empty() => {
            *u = username;
            *password = String::from_utf8_lossy(&secret).into_owned();
        }
        AuthConfig::Bearer { token } if token.is_empty() => {
            *token = String::from_utf8_lossy(&secret).into_owned()
        }
        AuthConfig::ApiKey { value, .. } if value.is_empty() => {
            *value = String::from_utf8_lossy(&secret).into_owned()
        }
        AuthConfig::OAuth2(oauth2)
            if oauth2.access_token.is_empty() && oauth2.client_secret.is_empty() =>
        {
            if let Ok(secrets) = serde_json::from_slice::<OAuth2Secrets>(&secret) {
                oauth2.client_secret = secrets.client_secret;
                oauth2.access_token = secrets.access_token;
                oauth2.refresh_token = secrets.refresh_token;
            }
        }
        AuthConfig::AwsSigV4(aws) if aws.secret_key.is_empty() => {
            if let Ok(secrets) = serde_json::from_slice::<AwsSigV4Secrets>(&secret) {
                aws.secret_key = secrets.secret_key;
                aws.session_token = secrets.session_token;
            }
        }
        _ => {}
    }
    Ok(())
}

pub enum ApiClientStoreEvent {
    TreeChanged,
    EnvironmentsChanged,
    HistoryChanged,
}

/// One entry of the Collection/Folder/Request tree, for the drag-and-drop
/// reordering API — mirrors `db_client_ui::store::TreeItemRef`. Collections
/// themselves are not reorderable in Phase 1 (a flat top-level list is
/// sufficient for the MVP), only folders and requests within one collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeItemRef {
    Folder(FolderId),
    Request(RequestId),
}

/// Where a dragged item lands relative to the anchor sibling it was dropped
/// next to. Mirrors `db_client_ui::store::RelativePosition`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelativePosition {
    Before,
    After,
}

pub struct ApiClientStore {
    pub collections: Vec<Collection>,
    pub folders: Vec<Folder>,
    pub requests: Vec<Request>,
    pub environments: Vec<Environment>,
    pub global_environment: Environment,
    pub active_environment_id: Option<EnvironmentId>,
    pub history: Vec<HistoryEntry>,
    pub http_client: reqwest::Client,
}

pub struct GlobalApiClientStore(pub Entity<ApiClientStore>);

impl Global for GlobalApiClientStore {}

impl EventEmitter<ApiClientStoreEvent> for ApiClientStore {}

impl ApiClientStore {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Tests must stay hermetic: never touch the real config dir or OS
        // keychain global, mirroring `DatabaseStore::new`.
        if !cfg!(test) {
            cx.spawn(async move |this, cx| {
                let (collections, environments, history) = cx
                    .background_executor()
                    .spawn(async {
                        (
                            load_collections_from_disk(),
                            load_environments_from_disk(),
                            load_history_from_disk(),
                        )
                    })
                    .await;
                let StoredCollections {
                    collections,
                    folders,
                    mut requests,
                } = collections;
                let provider = cx.update(|cx| zed_credentials_provider::global(cx));
                for request in &mut requests {
                    read_request_secret(&provider, request, cx).await.log_err();
                }
                this.update(cx, |store, cx| {
                    store.collections = collections;
                    store.folders = folders;
                    store.requests = requests;
                    store.environments = environments.environments;
                    store.global_environment = environments.global;
                    store.history = history;
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }

        Self {
            collections: Vec::new(),
            folders: Vec::new(),
            requests: Vec::new(),
            environments: Vec::new(),
            global_environment: Environment::global(),
            active_environment_id: None,
            history: Vec::new(),
            http_client: reqwest::Client::new(),
        }
    }

    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalApiClientStore>()
            .map(|global| global.0.clone())
    }

    fn persist_collections(&self, cx: &mut Context<Self>) {
        if cfg!(test) {
            return;
        }
        let requests = self.requests.clone();
        let collections = self.collections.clone();
        let folders = self.folders.clone();
        cx.spawn(async move |_this, cx| {
            let provider = cx.update(|cx| zed_credentials_provider::global(cx));
            for request in &requests {
                store_request_secret(&provider, request, cx).await.log_err();
            }
            let redacted: Vec<Request> = requests.iter().map(redact_auth_secret).collect();
            cx.background_executor()
                .spawn(async move {
                    save_collections_to_disk(&StoredCollections {
                        collections,
                        folders,
                        requests: redacted,
                    })
                    .log_err();
                })
                .await;
        })
        .detach();
    }

    fn persist_environments(&self, cx: &mut Context<Self>) {
        if cfg!(test) {
            return;
        }
        let stored = StoredEnvironments {
            environments: self.environments.clone(),
            global: self.global_environment.clone(),
        };
        cx.background_executor()
            .spawn(async move {
                save_environments_to_disk(&stored).log_err();
            })
            .detach();
    }

    // ----- Collections -----

    pub fn create_collection(&mut self, name: String, cx: &mut Context<Self>) -> CollectionId {
        let collection = Collection::new(name);
        let id = collection.id;
        self.collections.push(collection);
        cx.emit(ApiClientStoreEvent::TreeChanged);
        cx.notify();
        self.persist_collections(cx);
        id
    }

    pub fn rename_collection(&mut self, id: CollectionId, name: String, cx: &mut Context<Self>) {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return;
        }
        let Some(collection) = self.collections.iter_mut().find(|c| c.id == id) else {
            return;
        };
        collection.name = trimmed.to_string();
        cx.emit(ApiClientStoreEvent::TreeChanged);
        cx.notify();
        self.persist_collections(cx);
    }

    /// Mutates a collection's variable list -- the counterpart of
    /// `update_environment` for `pm.collectionVariables.set()` write-backs
    /// from a pre-request/test script.
    pub fn update_collection(
        &mut self,
        id: CollectionId,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut Collection),
    ) {
        let Some(collection) = self.collections.iter_mut().find(|c| c.id == id) else {
            return;
        };
        update(collection);
        cx.emit(ApiClientStoreEvent::TreeChanged);
        cx.notify();
        self.persist_collections(cx);
    }

    /// Removes a collection only when it holds no folders and no requests, so
    /// a delete never silently destroys saved requests. Returns whether it ran.
    pub fn delete_collection(&mut self, id: CollectionId, cx: &mut Context<Self>) -> bool {
        let is_empty = !self.folders.iter().any(|f| f.collection_id == id)
            && !self.requests.iter().any(|r| r.collection_id == id);
        if !is_empty || !self.collections.iter().any(|c| c.id == id) {
            return false;
        }
        self.collections.retain(|c| c.id != id);
        cx.emit(ApiClientStoreEvent::TreeChanged);
        cx.notify();
        self.persist_collections(cx);
        true
    }

    // ----- Folder tree helpers (mirror db_client_ui::store's) -----

    pub fn folder_depth(&self, folder_id: FolderId) -> usize {
        let mut depth = 0;
        let mut current = Some(folder_id);
        let mut visited = std::collections::HashSet::new();
        while let Some(id) = current {
            if !visited.insert(id) {
                break;
            }
            let Some(folder) = self.folders.iter().find(|f| f.id == id) else {
                break;
            };
            depth += 1;
            current = folder.parent_id;
        }
        depth
    }

    fn subtree_height(&self, folder_id: FolderId) -> usize {
        let children: Vec<FolderId> = self
            .folders
            .iter()
            .filter(|f| f.parent_id == Some(folder_id))
            .map(|f| f.id)
            .collect();
        1 + children
            .into_iter()
            .map(|child| self.subtree_height(child))
            .max()
            .unwrap_or(0)
    }

    fn is_descendant_of(&self, folder_id: FolderId, ancestor: FolderId) -> bool {
        let mut current = Some(folder_id);
        let mut visited = std::collections::HashSet::new();
        while let Some(id) = current {
            if id == ancestor {
                return true;
            }
            if !visited.insert(id) {
                break;
            }
            current = self
                .folders
                .iter()
                .find(|f| f.id == id)
                .and_then(|f| f.parent_id);
        }
        false
    }

    fn next_order_in(&self, collection_id: CollectionId, parent_id: Option<FolderId>) -> i64 {
        let max_folder = self
            .folders
            .iter()
            .filter(|f| f.collection_id == collection_id && f.parent_id == parent_id)
            .map(|f| f.order)
            .max();
        let max_request = self
            .requests
            .iter()
            .filter(|r| r.collection_id == collection_id && r.folder_id == parent_id)
            .map(|r| r.order)
            .max();
        max_folder.max(max_request).map_or(0, |order| order + 1)
    }

    pub fn create_folder(
        &mut self,
        collection_id: CollectionId,
        name: String,
        parent_id: Option<FolderId>,
        cx: &mut Context<Self>,
    ) -> Option<FolderId> {
        if let Some(parent) = parent_id {
            if self.folder_depth(parent) >= MAX_FOLDER_DEPTH {
                return None;
            }
        }
        let order = self.next_order_in(collection_id, parent_id);
        let folder = Folder::new(collection_id, name, parent_id, order);
        let id = folder.id;
        self.folders.push(folder);
        cx.emit(ApiClientStoreEvent::TreeChanged);
        cx.notify();
        self.persist_collections(cx);
        Some(id)
    }

    pub fn rename_folder(&mut self, id: FolderId, name: String, cx: &mut Context<Self>) {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return;
        }
        let Some(folder) = self.folders.iter_mut().find(|f| f.id == id) else {
            return;
        };
        folder.name = trimmed.to_string();
        cx.emit(ApiClientStoreEvent::TreeChanged);
        cx.notify();
        self.persist_collections(cx);
    }

    pub fn folder_is_empty(&self, id: FolderId) -> bool {
        !self.folders.iter().any(|f| f.parent_id == Some(id))
            && !self.requests.iter().any(|r| r.folder_id == Some(id))
    }

    /// Removes a folder only when it is empty. Returns false and changes
    /// nothing for a missing or non-empty folder.
    pub fn delete_folder(&mut self, id: FolderId, cx: &mut Context<Self>) -> bool {
        if !self.folders.iter().any(|f| f.id == id) || !self.folder_is_empty(id) {
            return false;
        }
        self.folders.retain(|f| f.id != id);
        cx.emit(ApiClientStoreEvent::TreeChanged);
        cx.notify();
        self.persist_collections(cx);
        true
    }

    pub fn create_request(
        &mut self,
        collection_id: CollectionId,
        name: String,
        folder_id: Option<FolderId>,
        cx: &mut Context<Self>,
    ) -> RequestId {
        let order = self.next_order_in(collection_id, folder_id);
        let mut request = Request::new(collection_id, name);
        request.folder_id = folder_id;
        request.order = order;
        let id = request.id;
        self.requests.push(request);
        cx.emit(ApiClientStoreEvent::TreeChanged);
        cx.notify();
        self.persist_collections(cx);
        id
    }

    pub fn delete_request(&mut self, id: RequestId, cx: &mut Context<Self>) {
        self.requests.retain(|r| r.id != id);
        cx.emit(ApiClientStoreEvent::TreeChanged);
        cx.notify();
        self.persist_collections(cx);
    }

    pub fn update_request(
        &mut self,
        id: RequestId,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut Request),
    ) {
        let Some(request) = self.requests.iter_mut().find(|r| r.id == id) else {
            return;
        };
        update(request);
        cx.emit(ApiClientStoreEvent::TreeChanged);
        cx.notify();
        self.persist_collections(cx);
    }

    /// Reorders `folder_id` among its sibling folders by swapping order with
    /// the neighbor in `direction` (-1 up, +1 down). No-op at the boundary.
    pub fn reorder_folder(&mut self, folder_id: FolderId, direction: i64, cx: &mut Context<Self>) {
        let Some((collection_id, parent, order)) = self
            .folders
            .iter()
            .find(|f| f.id == folder_id)
            .map(|f| (f.collection_id, f.parent_id, f.order))
        else {
            return;
        };
        let mut siblings: Vec<(FolderId, i64)> = self
            .folders
            .iter()
            .filter(|f| f.collection_id == collection_id && f.parent_id == parent)
            .map(|f| (f.id, f.order))
            .collect();
        siblings.sort_by_key(|(_, order)| *order);
        let Some(position) = siblings.iter().position(|(id, _)| *id == folder_id) else {
            return;
        };
        let target = position as i64 + direction;
        if target < 0 || target as usize >= siblings.len() {
            return;
        }
        let (neighbor_id, neighbor_order) = siblings[target as usize];
        for folder in self.folders.iter_mut() {
            if folder.id == folder_id {
                folder.order = neighbor_order;
            } else if folder.id == neighbor_id {
                folder.order = order;
            }
        }
        cx.emit(ApiClientStoreEvent::TreeChanged);
        cx.notify();
        self.persist_collections(cx);
    }

    /// Reorders `request_id` among its sibling requests by swapping order with
    /// the neighbor in `direction` (-1 up, +1 down). No-op at the boundary.
    pub fn reorder_request(
        &mut self,
        request_id: RequestId,
        direction: i64,
        cx: &mut Context<Self>,
    ) {
        let Some((collection_id, parent, order)) = self
            .requests
            .iter()
            .find(|r| r.id == request_id)
            .map(|r| (r.collection_id, r.folder_id, r.order))
        else {
            return;
        };
        let mut siblings: Vec<(RequestId, i64)> = self
            .requests
            .iter()
            .filter(|r| r.collection_id == collection_id && r.folder_id == parent)
            .map(|r| (r.id, r.order))
            .collect();
        siblings.sort_by_key(|(_, order)| *order);
        let Some(position) = siblings.iter().position(|(id, _)| *id == request_id) else {
            return;
        };
        let target = position as i64 + direction;
        if target < 0 || target as usize >= siblings.len() {
            return;
        }
        let (neighbor_id, neighbor_order) = siblings[target as usize];
        for request in self.requests.iter_mut() {
            if request.id == request_id {
                request.order = neighbor_order;
            } else if request.id == neighbor_id {
                request.order = order;
            }
        }
        cx.emit(ApiClientStoreEvent::TreeChanged);
        cx.notify();
        self.persist_collections(cx);
    }

    fn tree_item_parent(&self, item: TreeItemRef) -> Option<(CollectionId, Option<FolderId>)> {
        match item {
            TreeItemRef::Folder(id) => self
                .folders
                .iter()
                .find(|f| f.id == id)
                .map(|f| (f.collection_id, f.parent_id)),
            TreeItemRef::Request(id) => self
                .requests
                .iter()
                .find(|r| r.id == id)
                .map(|r| (r.collection_id, r.folder_id)),
        }
    }

    fn combined_siblings(
        &self,
        collection_id: CollectionId,
        parent_id: Option<FolderId>,
    ) -> Vec<TreeItemRef> {
        let mut siblings: Vec<(TreeItemRef, i64)> = self
            .folders
            .iter()
            .filter(|f| f.collection_id == collection_id && f.parent_id == parent_id)
            .map(|f| (TreeItemRef::Folder(f.id), f.order))
            .chain(
                self.requests
                    .iter()
                    .filter(|r| r.collection_id == collection_id && r.folder_id == parent_id)
                    .map(|r| (TreeItemRef::Request(r.id), r.order)),
            )
            .collect();
        siblings.sort_by_key(|(_, order)| *order);
        siblings.into_iter().map(|(item, _)| item).collect()
    }

    fn set_tree_item_order(&mut self, item: TreeItemRef, parent_id: Option<FolderId>, order: i64) {
        match item {
            TreeItemRef::Folder(id) => {
                if let Some(folder) = self.folders.iter_mut().find(|f| f.id == id) {
                    folder.parent_id = parent_id;
                    folder.order = order;
                }
            }
            TreeItemRef::Request(id) => {
                if let Some(request) = self.requests.iter_mut().find(|r| r.id == id) {
                    request.folder_id = parent_id;
                    request.order = order;
                }
            }
        }
    }

    /// Moves `item` to sit immediately `position` (before/after) `anchor`
    /// among `anchor`'s siblings, reparenting `item` under `anchor`'s parent
    /// if it wasn't already there. Rejects moves that would create a cycle,
    /// push a folder subtree past `MAX_FOLDER_DEPTH`, or cross collections
    /// (a request/folder always stays inside its own collection in Phase 1).
    /// Mirrors `db_client_ui::store::DatabaseStore::reposition_item`.
    pub fn reposition_item(
        &mut self,
        item: TreeItemRef,
        anchor: TreeItemRef,
        position: RelativePosition,
        cx: &mut Context<Self>,
    ) -> bool {
        if item == anchor {
            return false;
        }
        let Some((item_collection, _)) = self.tree_item_parent(item) else {
            return false;
        };
        let Some((target_collection, target_parent)) = self.tree_item_parent(anchor) else {
            return false;
        };
        if item_collection != target_collection {
            return false;
        }
        if let TreeItemRef::Folder(item_id) = item {
            if let Some(parent) = target_parent {
                if parent == item_id || self.is_descendant_of(parent, item_id) {
                    return false;
                }
                if self.folder_depth(parent) + self.subtree_height(item_id) > MAX_FOLDER_DEPTH {
                    return false;
                }
            } else if self.subtree_height(item_id) > MAX_FOLDER_DEPTH {
                return false;
            }
        }

        let mut siblings = self.combined_siblings(target_collection, target_parent);
        siblings.retain(|sibling| *sibling != item);
        let Some(anchor_index) = siblings.iter().position(|sibling| *sibling == anchor) else {
            return false;
        };
        let insert_at = match position {
            RelativePosition::Before => anchor_index,
            RelativePosition::After => anchor_index + 1,
        };
        siblings.insert(insert_at, item);

        for (index, sibling) in siblings.into_iter().enumerate() {
            self.set_tree_item_order(sibling, target_parent, index as i64);
        }

        cx.emit(ApiClientStoreEvent::TreeChanged);
        cx.notify();
        self.persist_collections(cx);
        true
    }

    /// Reparents `item` to be the last child of `folder_id`, for a drop
    /// directly onto a folder row (as opposed to `reposition_item`, which
    /// needs a sibling to anchor against and so can't target an empty
    /// folder). Rejects the same cases `reposition_item` does: a folder
    /// dropped into its own subtree, exceeding `MAX_FOLDER_DEPTH`, or a
    /// cross-collection move. Mirrors
    /// `db_client_ui::store::DatabaseStore::move_folder`/`move_connection_to_folder`.
    pub fn move_item_into_folder(
        &mut self,
        item: TreeItemRef,
        folder_id: FolderId,
        cx: &mut Context<Self>,
    ) -> bool {
        if item == TreeItemRef::Folder(folder_id) {
            return false;
        }
        let Some((item_collection, _)) = self.tree_item_parent(item) else {
            return false;
        };
        let Some(target_collection) = self
            .folders
            .iter()
            .find(|f| f.id == folder_id)
            .map(|f| f.collection_id)
        else {
            return false;
        };
        if item_collection != target_collection {
            return false;
        }
        if let TreeItemRef::Folder(item_id) = item {
            if folder_id == item_id || self.is_descendant_of(folder_id, item_id) {
                return false;
            }
            if self.folder_depth(folder_id) + self.subtree_height(item_id) > MAX_FOLDER_DEPTH {
                return false;
            }
        }

        let siblings = self.combined_siblings(target_collection, Some(folder_id));
        let order = siblings.len() as i64;
        self.set_tree_item_order(item, Some(folder_id), order);

        cx.emit(ApiClientStoreEvent::TreeChanged);
        cx.notify();
        self.persist_collections(cx);
        true
    }

    // ----- Environments -----

    pub fn create_environment(&mut self, name: String, cx: &mut Context<Self>) -> EnvironmentId {
        let environment = Environment::new(name);
        let id = environment.id;
        self.environments.push(environment);
        cx.emit(ApiClientStoreEvent::EnvironmentsChanged);
        cx.notify();
        self.persist_environments(cx);
        id
    }

    pub fn delete_environment(&mut self, id: EnvironmentId, cx: &mut Context<Self>) {
        self.environments.retain(|e| e.id != id);
        if self.active_environment_id == Some(id) {
            self.active_environment_id = None;
        }
        cx.emit(ApiClientStoreEvent::EnvironmentsChanged);
        cx.notify();
        self.persist_environments(cx);
    }

    pub fn set_active_environment(&mut self, id: Option<EnvironmentId>, cx: &mut Context<Self>) {
        self.active_environment_id = id;
        cx.emit(ApiClientStoreEvent::EnvironmentsChanged);
        cx.notify();
    }

    pub fn active_environment(&self) -> Option<&Environment> {
        self.active_environment_id
            .and_then(|id| self.environments.iter().find(|e| e.id == id))
    }

    /// Mutates an environment's variable list (add/edit/remove/reorder — the
    /// caller decides via `update`), then persists. Pass `None` for the
    /// global environment.
    pub fn update_environment(
        &mut self,
        id: Option<EnvironmentId>,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut Environment),
    ) {
        let environment = match id {
            None => &mut self.global_environment,
            Some(id) => {
                let Some(environment) = self.environments.iter_mut().find(|e| e.id == id) else {
                    return;
                };
                environment
            }
        };
        update(environment);
        cx.emit(ApiClientStoreEvent::EnvironmentsChanged);
        cx.notify();
        self.persist_environments(cx);
    }

    // ----- History -----

    fn persist_history(&self, cx: &mut Context<Self>) {
        if cfg!(test) {
            return;
        }
        let history = self.history.clone();
        cx.background_executor()
            .spawn(async move {
                save_history_to_disk(&history).log_err();
            })
            .detach();
    }

    /// Appends a history entry and evicts the oldest ones past
    /// `MAX_HISTORY_ENTRIES`, newest-first in `self.history`.
    pub fn record_history_entry(&mut self, entry: HistoryEntry, cx: &mut Context<Self>) {
        self.history.insert(0, entry);
        self.history.truncate(MAX_HISTORY_ENTRIES);
        cx.emit(ApiClientStoreEvent::HistoryChanged);
        cx.notify();
        self.persist_history(cx);
    }

    pub fn clear_history(&mut self, cx: &mut Context<Self>) {
        self.history.clear();
        cx.emit(ApiClientStoreEvent::HistoryChanged);
        cx.notify();
        self.persist_history(cx);
    }

    /// Builds the `{{token}}` resolution context for `request`: its own
    /// collection's variables, the active environment (if any), and the
    /// global environment -- the exact precedence
    /// `variable_resolution::VariableContext` documents.
    pub fn variable_context_for(&self, request: &Request) -> api_client::VariableContext<'_> {
        api_client::VariableContext {
            environment: self.active_environment(),
            collection: self
                .collections
                .iter()
                .find(|c| c.id == request.collection_id),
            global: &self.global_environment,
        }
    }

    /// Adds a whole imported collection (its folders and requests included)
    /// as brand-new tree entries. Used by both the cURL-paste importer
    /// (a synthetic one-request collection) and the Postman Collection v2.1
    /// importer (a full folder/request tree).
    pub fn import_collection(
        &mut self,
        collection: Collection,
        folders: Vec<Folder>,
        requests: Vec<Request>,
        cx: &mut Context<Self>,
    ) {
        self.collections.push(collection);
        self.folders.extend(folders);
        self.requests.extend(requests);
        cx.emit(ApiClientStoreEvent::TreeChanged);
        cx.notify();
        self.persist_collections(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};

    fn new_store(cx: &mut TestAppContext) -> Entity<ApiClientStore> {
        cx.new(|cx| ApiClientStore::new(cx))
    }

    #[gpui::test]
    fn create_and_delete_collection(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let id = store.update(cx, |store, cx| {
            store.create_collection("Payments".into(), cx)
        });
        store.read_with(cx, |store, _| {
            assert_eq!(store.collections.len(), 1);
            assert_eq!(store.collections[0].id, id);
        });
        let deleted = store.update(cx, |store, cx| store.delete_collection(id, cx));
        assert!(deleted);
        store.read_with(cx, |store, _| assert!(store.collections.is_empty()));
    }

    #[gpui::test]
    fn deleting_a_non_empty_collection_is_a_no_op(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let collection_id = store.update(cx, |store, cx| {
            store.create_collection("Payments".into(), cx)
        });
        store.update(cx, |store, cx| {
            store.create_request(collection_id, "List".into(), None, cx)
        });
        let deleted = store.update(cx, |store, cx| store.delete_collection(collection_id, cx));
        assert!(!deleted);
        store.read_with(cx, |store, _| assert_eq!(store.collections.len(), 1));
    }

    #[gpui::test]
    fn folder_depth_guard_rejects_a_sixth_nesting_level(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let collection_id = store.update(cx, |store, cx| store.create_collection("A".into(), cx));
        let mut parent = None;
        for level in 0..MAX_FOLDER_DEPTH {
            parent = store.update(cx, |store, cx| {
                store.create_folder(collection_id, format!("L{level}"), parent, cx)
            });
            assert!(parent.is_some(), "level {level} should be allowed");
        }
        let rejected = store.update(cx, |store, cx| {
            store.create_folder(collection_id, "too-deep".into(), parent, cx)
        });
        assert!(rejected.is_none());
    }

    #[gpui::test]
    fn delete_folder_only_removes_an_empty_folder(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let collection_id = store.update(cx, |store, cx| store.create_collection("A".into(), cx));
        let folder_id = store
            .update(cx, |store, cx| {
                store.create_folder(collection_id, "Auth".into(), None, cx)
            })
            .unwrap();
        store.update(cx, |store, cx| {
            store.create_request(collection_id, "Login".into(), Some(folder_id), cx)
        });
        let removed = store.update(cx, |store, cx| store.delete_folder(folder_id, cx));
        assert!(
            !removed,
            "a folder with a request inside must not be deleted"
        );

        let empty_folder_id = store
            .update(cx, |store, cx| {
                store.create_folder(collection_id, "Empty".into(), None, cx)
            })
            .unwrap();
        let removed = store.update(cx, |store, cx| store.delete_folder(empty_folder_id, cx));
        assert!(removed);
    }

    #[gpui::test]
    fn reorder_request_swaps_with_its_neighbor(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let collection_id = store.update(cx, |store, cx| store.create_collection("A".into(), cx));
        let first = store.update(cx, |store, cx| {
            store.create_request(collection_id, "First".into(), None, cx)
        });
        let second = store.update(cx, |store, cx| {
            store.create_request(collection_id, "Second".into(), None, cx)
        });
        store.update(cx, |store, cx| store.reorder_request(first, 1, cx));
        store.read_with(cx, |store, _| {
            let first_order = store.requests.iter().find(|r| r.id == first).unwrap().order;
            let second_order = store
                .requests
                .iter()
                .find(|r| r.id == second)
                .unwrap()
                .order;
            assert!(first_order > second_order);
        });
    }

    #[gpui::test]
    fn reorder_at_the_boundary_is_a_no_op(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let collection_id = store.update(cx, |store, cx| store.create_collection("A".into(), cx));
        let only = store.update(cx, |store, cx| {
            store.create_request(collection_id, "Only".into(), None, cx)
        });
        store.update(cx, |store, cx| store.reorder_request(only, -1, cx));
        store.read_with(cx, |store, _| {
            assert_eq!(
                store.requests.iter().find(|r| r.id == only).unwrap().order,
                0
            );
        });
    }

    #[gpui::test]
    fn reposition_item_reorders_same_parent_siblings(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let collection_id = store.update(cx, |store, cx| store.create_collection("A".into(), cx));
        let a = store.update(cx, |store, cx| {
            store.create_request(collection_id, "A".into(), None, cx)
        });
        let b = store.update(cx, |store, cx| {
            store.create_request(collection_id, "B".into(), None, cx)
        });
        let c = store.update(cx, |store, cx| {
            store.create_request(collection_id, "C".into(), None, cx)
        });

        let moved = store.update(cx, |store, cx| {
            store.reposition_item(
                TreeItemRef::Request(a),
                TreeItemRef::Request(c),
                RelativePosition::After,
                cx,
            )
        });
        assert!(moved);
        store.read_with(cx, |store, _| {
            let mut ordered: Vec<_> = store.requests.iter().map(|r| (r.id, r.order)).collect();
            ordered.sort_by_key(|(_, order)| *order);
            let ids: Vec<_> = ordered.into_iter().map(|(id, _)| id).collect();
            assert_eq!(ids, vec![b, c, a]);
        });
    }

    #[gpui::test]
    fn reposition_item_moves_a_request_into_a_different_folder(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let collection_id = store.update(cx, |store, cx| store.create_collection("A".into(), cx));
        let folder_a = store
            .update(cx, |store, cx| {
                store.create_folder(collection_id, "A".into(), None, cx)
            })
            .unwrap();
        let folder_b = store
            .update(cx, |store, cx| {
                store.create_folder(collection_id, "B".into(), None, cx)
            })
            .unwrap();
        let request = store.update(cx, |store, cx| {
            store.create_request(collection_id, "Req".into(), Some(folder_a), cx)
        });
        let anchor = store.update(cx, |store, cx| {
            store.create_request(collection_id, "AnchorInB".into(), Some(folder_b), cx)
        });

        let moved = store.update(cx, |store, cx| {
            store.reposition_item(
                TreeItemRef::Request(request),
                TreeItemRef::Request(anchor),
                RelativePosition::Before,
                cx,
            )
        });
        assert!(moved);
        store.read_with(cx, |store, _| {
            assert_eq!(
                store
                    .requests
                    .iter()
                    .find(|r| r.id == request)
                    .unwrap()
                    .folder_id,
                Some(folder_b)
            );
        });
    }

    #[gpui::test]
    fn reposition_item_rejects_moving_a_folder_into_its_own_descendant(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let collection_id = store.update(cx, |store, cx| store.create_collection("A".into(), cx));
        let parent = store
            .update(cx, |store, cx| {
                store.create_folder(collection_id, "Parent".into(), None, cx)
            })
            .unwrap();
        let child = store
            .update(cx, |store, cx| {
                store.create_folder(collection_id, "Child".into(), Some(parent), cx)
            })
            .unwrap();
        let anchor_in_child = store
            .update(cx, |store, cx| {
                store.create_folder(collection_id, "AnchorInChild".into(), Some(child), cx)
            })
            .unwrap();

        let moved = store.update(cx, |store, cx| {
            store.reposition_item(
                TreeItemRef::Folder(parent),
                TreeItemRef::Folder(anchor_in_child),
                RelativePosition::Before,
                cx,
            )
        });
        assert!(!moved);
    }

    #[gpui::test]
    fn active_environment_lookup_and_variable_update(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let env_id = store.update(cx, |store, cx| {
            store.create_environment("Staging".into(), cx)
        });
        store.update(cx, |store, cx| {
            store.set_active_environment(Some(env_id), cx)
        });
        store.update(cx, |store, cx| {
            store.update_environment(Some(env_id), cx, |environment| {
                environment.variables.push(api_client::Variable::new(
                    "base_url".into(),
                    "https://staging.example.com".into(),
                ));
            });
        });
        store.read_with(cx, |store, _| {
            let active = store.active_environment().unwrap();
            assert_eq!(active.id, env_id);
            assert!(active.variable("base_url").is_some());
        });
    }

    #[gpui::test]
    fn import_collection_adds_the_collection_its_folders_and_its_requests(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let collection = api_client::Collection::new("Imported".into());
        let collection_id = collection.id;
        let folder = api_client::Folder::new(collection_id, "Auth".into(), None, 0);
        let folder_id = folder.id;
        let mut request = api_client::Request::new(collection_id, "Login".into());
        request.folder_id = Some(folder_id);

        store.update(cx, |store, cx| {
            store.import_collection(collection, vec![folder], vec![request], cx);
        });

        store.read_with(cx, |store, _| {
            assert_eq!(store.collections.len(), 1);
            assert_eq!(store.collections[0].id, collection_id);
            assert_eq!(store.folders.len(), 1);
            assert_eq!(store.requests.len(), 1);
            assert_eq!(store.requests[0].folder_id, Some(folder_id));
        });
    }
}
