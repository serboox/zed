use std::sync::Arc;

use anyhow::Result;
use api_client::{
    Collection, CollectionId, Environment, EnvironmentId, Folder, FolderId, HistoryEntry, Request,
    RequestId, TreeOrder,
};
use credentials_provider::CredentialsProvider;
use gpui::{App, AsyncApp, Context, Entity, EventEmitter, Global};
use serde::{Deserialize, Serialize};
use util::ResultExt;
use uuid::Uuid;

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
    /// How the tree is ordered on screen. A document written before this field
    /// existed reads as by-name, which is how a list of names is expected to
    /// read; the dragged order is kept in `order` either way, so switching back
    /// to it loses nothing.
    #[serde(default)]
    tree_order: TreeOrder,
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

/// The full request/response exchange behind one `HistoryEntry`. Kept only
/// in memory for the life of the running app -- `HistoryEntry` itself is
/// small enough to persist to disk and survive a restart, but headers,
/// bodies and the environment used are not worth writing to disk on every
/// send. Looked up by `HistoryEntry::id`, never by position in `history`,
/// since that position shifts as new entries arrive and old ones are
/// evicted.
#[derive(Debug, Clone)]
pub struct HistoryExchangeDetail {
    pub request: api_client::ResolvedRequest,
    pub outcome: HistoryExchangeOutcome,
    pub environment_name: Option<String>,
}

#[derive(Debug, Clone)]
pub enum HistoryExchangeOutcome {
    Success(crate::response_view::ResponseData),
    Error(String),
}

pub struct ApiClientStore {
    pub collections: Vec<Collection>,
    pub tree_order: TreeOrder,
    pub folders: Vec<Folder>,
    pub requests: Vec<Request>,
    pub environments: Vec<Environment>,
    pub global_environment: Environment,
    pub active_environment_id: Option<EnvironmentId>,
    pub history: Vec<HistoryEntry>,
    /// Session-only detail for entries still recent enough to have one --
    /// see `HistoryExchangeDetail`. Kept in step with `history` by
    /// `record_history_entry`/`clear_history` rather than growing without
    /// bound.
    pub history_details: std::collections::HashMap<Uuid, HistoryExchangeDetail>,
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
                    tree_order,
                } = collections;
                let provider = cx.update(|cx| zed_credentials_provider::global(cx));
                for request in &mut requests {
                    read_request_secret(&provider, request, cx).await.log_err();
                }
                this.update(cx, |store, cx| {
                    store.collections = collections;
                    store.tree_order = tree_order;
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
            tree_order: TreeOrder::default(),
            folders: Vec::new(),
            requests: Vec::new(),
            environments: Vec::new(),
            global_environment: Environment::global(),
            active_environment_id: None,
            history: Vec::new(),
            history_details: std::collections::HashMap::new(),
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
        let tree_order = self.tree_order;
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
                        tree_order,
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

    /// Changes how the tree is ordered, and remembers it for the next session.
    pub fn set_tree_order(&mut self, order: TreeOrder, cx: &mut Context<Self>) {
        if self.tree_order == order {
            return;
        }
        self.tree_order = order;
        cx.emit(ApiClientStoreEvent::TreeChanged);
        self.persist_collections(cx);
        cx.notify();
    }

    // ----- Collections -----

    pub fn create_collection(&mut self, name: String, cx: &mut Context<Self>) -> CollectionId {
        let mut collection = Collection::new(name);
        collection.order = self.next_collection_order();
        let id = collection.id;
        self.collections.push(collection);
        cx.emit(ApiClientStoreEvent::TreeChanged);
        cx.notify();
        self.persist_collections(cx);
        id
    }

    fn next_collection_order(&self) -> i64 {
        self.collections
            .iter()
            .map(|collection| collection.order)
            .max()
            .map_or(0, |order| order + 1)
    }

    /// Reorders `collection_id` among all top-level collections by swapping
    /// order with the neighbor in `direction` (-1 up, +1 down). No-op at the
    /// boundary. Mirrors `reorder_folder`/`reorder_request`, but collections
    /// have no `parent_id` to scope siblings by -- the whole list is one
    /// sibling group.
    pub fn reorder_collection(
        &mut self,
        collection_id: CollectionId,
        direction: i64,
        cx: &mut Context<Self>,
    ) {
        let Some(order) = self
            .collections
            .iter()
            .find(|collection| collection.id == collection_id)
            .map(|collection| collection.order)
        else {
            return;
        };
        let mut siblings: Vec<(CollectionId, i64)> = self
            .collections
            .iter()
            .map(|collection| (collection.id, collection.order))
            .collect();
        siblings.sort_by_key(|(_, order)| *order);
        let Some(position) = siblings.iter().position(|(id, _)| *id == collection_id) else {
            return;
        };
        let target = position as i64 + direction;
        if target < 0 || target as usize >= siblings.len() {
            return;
        }
        let (neighbor_id, neighbor_order) = siblings[target as usize];
        for collection in self.collections.iter_mut() {
            if collection.id == collection_id {
                collection.order = neighbor_order;
            } else if collection.id == neighbor_id {
                collection.order = order;
            }
        }
        cx.emit(ApiClientStoreEvent::TreeChanged);
        cx.notify();
        self.persist_collections(cx);
    }

    /// Moves `collection_id` to sit immediately before/after `anchor_id`
    /// among the top-level collections. Mirrors `reposition_item`'s
    /// insert-among-siblings shape, but for the flat collection list rather
    /// than folder/request `TreeItemRef`s.
    pub fn reposition_collection(
        &mut self,
        collection_id: CollectionId,
        anchor_id: CollectionId,
        position: RelativePosition,
        cx: &mut Context<Self>,
    ) -> bool {
        if collection_id == anchor_id {
            return false;
        }
        let mut siblings: Vec<(CollectionId, i64)> = self
            .collections
            .iter()
            .map(|collection| (collection.id, collection.order))
            .collect();
        siblings.sort_by_key(|(_, order)| *order);
        let Some(item_index) = siblings.iter().position(|(id, _)| *id == collection_id) else {
            return false;
        };
        let (item, _) = siblings.remove(item_index);
        let Some(anchor_index) = siblings.iter().position(|(id, _)| *id == anchor_id) else {
            return false;
        };
        let insert_at = match position {
            RelativePosition::Before => anchor_index,
            RelativePosition::After => anchor_index + 1,
        };
        siblings.insert(insert_at, (item, 0));

        for (index, (id, _)) in siblings.into_iter().enumerate() {
            if let Some(collection) = self.collections.iter_mut().find(|c| c.id == id) {
                collection.order = index as i64;
            }
        }
        cx.emit(ApiClientStoreEvent::TreeChanged);
        cx.notify();
        self.persist_collections(cx);
        true
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

    /// How much a collection holds, so a reader can be told what deleting it
    /// takes with it.
    pub fn collection_contents(&self, id: CollectionId) -> (usize, usize) {
        let folders = self
            .folders
            .iter()
            .filter(|folder| folder.collection_id == id)
            .count();
        let requests = self
            .requests
            .iter()
            .filter(|request| request.collection_id == id)
            .count();
        (folders, requests)
    }

    /// Removes a collection with everything inside it. The guarded
    /// [`Self::delete_collection`] is for a collection already known to be
    /// empty; this is what a reader asking to delete a full one means.
    pub fn delete_collection_with_contents(
        &mut self,
        id: CollectionId,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self
            .collections
            .iter()
            .any(|collection| collection.id == id)
        {
            return false;
        }
        self.requests.retain(|request| request.collection_id != id);
        self.folders.retain(|folder| folder.collection_id != id);
        self.collections.retain(|collection| collection.id != id);
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

    /// How much a folder holds, counting nested folders and every request under
    /// them, so deleting it can say what goes.
    pub fn folder_contents(&self, id: FolderId) -> (usize, usize) {
        let descendants = self.folder_and_descendants(id);
        let folders = descendants.len() - 1;
        let requests = self
            .requests
            .iter()
            .filter(|request| {
                request
                    .folder_id
                    .is_some_and(|folder_id| descendants.contains(&folder_id))
            })
            .count();
        (folders, requests)
    }

    fn folder_and_descendants(&self, id: FolderId) -> Vec<FolderId> {
        let mut collected = vec![id];
        let mut index = 0;
        while index < collected.len() {
            let current = collected[index];
            for folder in &self.folders {
                if folder.parent_id == Some(current) && !collected.contains(&folder.id) {
                    collected.push(folder.id);
                }
            }
            index += 1;
        }
        collected
    }

    /// Removes a folder with everything inside it, nested folders included.
    pub fn delete_folder_with_contents(&mut self, id: FolderId, cx: &mut Context<Self>) -> bool {
        if !self.folders.iter().any(|folder| folder.id == id) {
            return false;
        }
        let doomed = self.folder_and_descendants(id);
        self.requests.retain(|request| {
            !request
                .folder_id
                .is_some_and(|folder_id| doomed.contains(&folder_id))
        });
        self.folders.retain(|folder| !doomed.contains(&folder.id));
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

    /// Clones `id` into a new request in the same collection/folder, right
    /// after the original in sibling order, with every field copied
    /// (headers, body, auth, scripts) except its id and name -- mirrors how
    /// `create_request` assigns a fresh id/order, but starts from an existing
    /// request instead of a blank one.
    pub fn duplicate_request(
        &mut self,
        id: RequestId,
        cx: &mut Context<Self>,
    ) -> Option<RequestId> {
        let source = self.requests.iter().find(|r| r.id == id)?.clone();
        let order = self.next_order_in(source.collection_id, source.folder_id);
        let mut duplicate = source.clone();
        duplicate.id = RequestId::new_v4();
        duplicate.name = format!("Copy of {}", source.name);
        duplicate.order = order;
        let new_id = duplicate.id;
        self.requests.push(duplicate);
        cx.emit(ApiClientStoreEvent::TreeChanged);
        cx.notify();
        self.persist_collections(cx);
        Some(new_id)
    }

    pub fn rename_request(&mut self, id: RequestId, name: String, cx: &mut Context<Self>) {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return;
        }
        let Some(request) = self.requests.iter_mut().find(|r| r.id == id) else {
            return;
        };
        request.name = trimmed.to_string();
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
        // A request pinned to an environment that is gone is pinned to nothing,
        // and one that was sent to it has nowhere to be sent: both are dropped
        // rather than left pointing at a name that no longer exists.
        let mut requests_repinned = false;
        for request in &mut self.requests {
            let pinned = request.pinned_environments();
            let left = pinned
                .iter()
                .copied()
                .filter(|pinned| *pinned != id)
                .collect::<Vec<_>>();
            if left.len() != pinned.len() {
                request.pin_to_environments(left);
                requests_repinned = true;
            }
            if request.chosen_environment() == Some(id) {
                request.choose_environment(None);
                requests_repinned = true;
            }
            if request.compared_with() == Some(id) {
                request.compare_with(None);
                requests_repinned = true;
            }
        }
        cx.emit(ApiClientStoreEvent::EnvironmentsChanged);
        cx.notify();
        self.persist_environments(cx);
        if requests_repinned {
            self.persist_collections(cx);
        }
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

    pub fn environment_by_id(&self, id: EnvironmentId) -> Option<&Environment> {
        self.environments
            .iter()
            .find(|environment| environment.id == id)
    }

    /// The environment a request's variables actually resolve against:
    /// its own pinned environment when it has one, falling back to
    /// whichever environment is currently active store-wide.
    pub fn effective_environment_for(&self, request: &Request) -> Option<&Environment> {
        // What the reader chose for this request, and the active environment
        // otherwise. A pinned environment is one the picker keeps at hand, not
        // one anything is sent to.
        request
            .chosen_environment()
            .and_then(|id| self.environment_by_id(id))
            .or_else(|| self.active_environment())
    }

    /// Sends this request to `environment_id` from now on, or back to whichever
    /// environment is active when given `None`. Leaves its pins alone.
    pub fn choose_request_environment(
        &mut self,
        request_id: RequestId,
        environment_id: Option<EnvironmentId>,
        cx: &mut Context<Self>,
    ) {
        let Some(request) = self.requests.iter_mut().find(|r| r.id == request_id) else {
            return;
        };
        request.choose_environment(environment_id);
        cx.notify();
        self.persist_collections(cx);
    }

    /// Asks for the next send of this request to be compared against
    /// `environment_id`, or for no comparison when given `None`. Nothing is
    /// sent here: the comparison waits for Send.
    pub fn set_request_comparison_environment(
        &mut self,
        request_id: RequestId,
        environment_id: Option<EnvironmentId>,
        cx: &mut Context<Self>,
    ) {
        let Some(request) = self.requests.iter_mut().find(|r| r.id == request_id) else {
            return;
        };
        request.compare_with(environment_id);
        cx.notify();
        self.persist_collections(cx);
    }

    /// What it takes to send this request against one environment: the client
    /// and the request with every `{{token}}` in it resolved. Handed out rather
    /// than sent here, so whoever asked can await it in their own window.
    pub fn what_to_send(
        &self,
        request_id: RequestId,
        environment_id: EnvironmentId,
    ) -> Option<(reqwest::Client, api_client::ResolvedRequest)> {
        // An environment that has been deleted is not one to send against: the
        // request would go out resolved against nothing at all and come back
        // labelled with a name that no longer exists.
        self.environment_by_id(environment_id)?;
        let request = self.requests.iter().find(|r| r.id == request_id)?;
        let context = self.variable_context_for_environment(request, environment_id);
        let dynamic = api_client::SystemDynamicVariableSource;
        let resolve = |text: &str| {
            api_client::resolve(text, &context, &dynamic, api_client::ResolveMode::ForSend)
        };
        Some((
            self.http_client.clone(),
            api_client::build_resolved_request(request, &resolve),
        ))
    }

    /// Adds or removes one pinned environment, leaving the rest alone. Several
    /// are what comparing across environments works from.
    pub fn toggle_request_pinned_environment(
        &mut self,
        request_id: RequestId,
        environment_id: EnvironmentId,
        cx: &mut Context<Self>,
    ) {
        let Some(request) = self.requests.iter_mut().find(|r| r.id == request_id) else {
            return;
        };
        request.toggle_pinned_environment(environment_id);
        cx.notify();
        self.persist_collections(cx);
    }

    /// Every environment a request is pinned to, resolved to the environments
    /// themselves and skipping any that have since been deleted.
    pub fn pinned_environments_for(&self, request_id: RequestId) -> Vec<Environment> {
        let Some(request) = self.requests.iter().find(|r| r.id == request_id) else {
            return Vec::new();
        };
        request
            .pinned_environments()
            .into_iter()
            .filter_map(|id| self.environment_by_id(id).cloned())
            .collect()
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
    /// `MAX_HISTORY_ENTRIES`, newest-first in `self.history`. Any
    /// `history_details` entry whose owning `HistoryEntry` was just evicted
    /// is dropped along with it, so the detail map never outlives the entry
    /// it belongs to.
    pub fn record_history_entry(&mut self, entry: HistoryEntry, cx: &mut Context<Self>) {
        self.history.insert(0, entry);
        self.history.truncate(MAX_HISTORY_ENTRIES);
        let live_ids: std::collections::HashSet<Uuid> =
            self.history.iter().map(|entry| entry.id).collect();
        self.history_details.retain(|id, _| live_ids.contains(id));
        cx.emit(ApiClientStoreEvent::HistoryChanged);
        cx.notify();
        self.persist_history(cx);
    }

    /// Records the full request/response detail behind a history entry --
    /// see `HistoryExchangeDetail`. Called alongside `record_history_entry`
    /// for the entry it belongs to; a stale or unknown `entry_id` is simply
    /// never looked up, so there is nothing to guard against here.
    pub fn record_history_detail(&mut self, entry_id: Uuid, detail: HistoryExchangeDetail) {
        self.history_details.insert(entry_id, detail);
    }

    /// The full exchange behind a history entry, if it is still recent
    /// enough to have one -- `None` both for an entry that predates this
    /// session (loaded from disk without detail) and for one evicted past
    /// `MAX_HISTORY_ENTRIES`.
    pub fn history_detail(&self, entry_id: Uuid) -> Option<&HistoryExchangeDetail> {
        self.history_details.get(&entry_id)
    }

    pub fn clear_history(&mut self, cx: &mut Context<Self>) {
        self.history.clear();
        self.history_details.clear();
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
            environment: self.effective_environment_for(request),
            collection: self
                .collections
                .iter()
                .find(|c| c.id == request.collection_id),
            global: &self.global_environment,
        }
    }

    /// Same as `variable_context_for`, but resolves against an explicitly
    /// chosen environment instead of the request's pinned/active one --
    /// used by the response-diff tab's "compare against another
    /// environment" mode, where the user picks a one-off environment for a
    /// single comparison request without changing what the request would
    /// normally resolve against.
    pub fn variable_context_for_environment(
        &self,
        request: &Request,
        environment_id: EnvironmentId,
    ) -> api_client::VariableContext<'_> {
        api_client::VariableContext {
            environment: self.environment_by_id(environment_id),
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

    /// A reader deleting a full collection means the whole thing. The guarded
    /// call refuses a non-empty one, which is why the menu entry used to do
    /// nothing at all.
    #[gpui::test]
    fn deleting_a_collection_with_contents_takes_everything_in_it(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let (doomed, kept) = store.update(cx, |store, cx| {
            let doomed = store.create_collection("Doomed".into(), cx);
            let folder = store
                .create_folder(doomed, "icons".into(), None, cx)
                .expect("folder");
            let nested = store
                .create_folder(doomed, "nested".into(), Some(folder), cx)
                .expect("nested folder");
            store.create_request(doomed, "one".into(), Some(folder), cx);
            store.create_request(doomed, "two".into(), Some(nested), cx);
            store.create_request(doomed, "root".into(), None, cx);

            let kept = store.create_collection("Kept".into(), cx);
            store.create_request(kept, "survivor".into(), None, cx);
            (doomed, kept)
        });

        store.read_with(cx, |store, _| {
            assert_eq!(store.collection_contents(doomed), (2, 3));
        });
        assert!(
            !store.update(cx, |store, cx| store.delete_collection(doomed, cx)),
            "the guarded call has to keep refusing a full collection"
        );

        assert!(store.update(cx, |store, cx| {
            store.delete_collection_with_contents(doomed, cx)
        }));
        store.read_with(cx, |store, _| {
            assert!(
                store.collections.iter().all(|c| c.id != doomed),
                "the collection has to be gone"
            );
            assert!(store.folders.iter().all(|f| f.collection_id != doomed));
            assert!(store.requests.iter().all(|r| r.collection_id != doomed));
            assert_eq!(
                store
                    .requests
                    .iter()
                    .filter(|r| r.collection_id == kept)
                    .count(),
                1,
                "another collection must not be touched"
            );
        });
    }

    #[gpui::test]
    fn deleting_a_folder_with_contents_takes_its_nested_folders_too(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let (collection, folder, sibling) = store.update(cx, |store, cx| {
            let collection = store.create_collection("Contract".into(), cx);
            let folder = store
                .create_folder(collection, "icons".into(), None, cx)
                .expect("folder");
            let nested = store
                .create_folder(collection, "flags".into(), Some(folder), cx)
                .expect("nested folder");
            store.create_request(collection, "one".into(), Some(folder), cx);
            store.create_request(collection, "two".into(), Some(nested), cx);
            let sibling = store
                .create_folder(collection, "orders".into(), None, cx)
                .expect("sibling folder");
            store.create_request(collection, "kept".into(), Some(sibling), cx);
            (collection, folder, sibling)
        });

        store.read_with(cx, |store, _| {
            assert_eq!(store.folder_contents(folder), (1, 2));
        });
        assert!(
            !store.update(cx, |store, cx| store.delete_folder(folder, cx)),
            "the guarded call has to keep refusing a full folder"
        );

        assert!(store.update(cx, |store, cx| {
            store.delete_folder_with_contents(folder, cx)
        }));
        store.read_with(cx, |store, _| {
            assert_eq!(
                store
                    .folders
                    .iter()
                    .filter(|f| f.collection_id == collection)
                    .map(|f| f.id)
                    .collect::<Vec<_>>(),
                vec![sibling],
                "only the sibling folder survives"
            );
            assert_eq!(
                store
                    .requests
                    .iter()
                    .map(|r| r.name.as_str())
                    .collect::<Vec<_>>(),
                vec!["kept"],
                "a request outside the folder must not be swept up"
            );
        });
    }

    #[test]
    fn contents_are_described_for_a_reader() {
        assert_eq!(crate::panel::describe_contents(0, 1), "1 request");
        assert_eq!(crate::panel::describe_contents(0, 3), "3 requests");
        assert_eq!(crate::panel::describe_contents(2, 0), "2 folders");
        assert_eq!(
            crate::panel::describe_contents(1, 5),
            "1 folder and 5 requests"
        );
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
    fn a_request_s_pinned_environment_overrides_the_globally_active_one(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let staging_id = store.update(cx, |store, cx| {
            store.create_environment("Staging".into(), cx)
        });
        let production_id = store.update(cx, |store, cx| {
            store.create_environment("Production".into(), cx)
        });
        store.update(cx, |store, cx| {
            store.update_environment(Some(staging_id), cx, |environment| {
                environment.variables.push(api_client::Variable::new(
                    "base_url".into(),
                    "https://staging.example.com".into(),
                ));
            });
            store.update_environment(Some(production_id), cx, |environment| {
                environment.variables.push(api_client::Variable::new(
                    "base_url".into(),
                    "https://prod.example.com".into(),
                ));
            });
            store.set_active_environment(Some(production_id), cx);
        });

        let collection = api_client::Collection::new("Sample".into());
        let mut request = api_client::Request::new(collection.id, "Get users".into());
        request.pinned_environment_id = Some(staging_id);

        store.read_with(cx, |store, _| {
            let effective = store.effective_environment_for(&request).unwrap();
            assert_eq!(
                effective.id, staging_id,
                "a pinned environment must win over the globally active one"
            );
        });

        // Unpinned requests keep resolving against whatever is active.
        let mut unpinned_request = request;
        unpinned_request.pinned_environment_id = None;
        store.read_with(cx, |store, _| {
            let effective = store.effective_environment_for(&unpinned_request).unwrap();
            assert_eq!(effective.id, production_id);
        });
    }

    /// The Diff tab's "vs Environment" comparison must resolve against
    /// whichever environment the user explicitly picked for that one-off
    /// comparison -- never silently falling back to the request's own
    /// pinned environment or the store's globally active one, which would
    /// make the comparison compare the wrong thing without any visible
    /// indication of the mistake.
    #[gpui::test]
    fn variable_context_for_environment_ignores_the_pinned_and_active_environment(
        cx: &mut TestAppContext,
    ) {
        let store = new_store(cx);
        let staging_id = store.update(cx, |store, cx| {
            store.create_environment("Staging".into(), cx)
        });
        let production_id = store.update(cx, |store, cx| {
            store.create_environment("Production".into(), cx)
        });
        let comparison_id = store.update(cx, |store, cx| {
            store.create_environment("Comparison".into(), cx)
        });
        store.update(cx, |store, cx| {
            store.update_environment(Some(staging_id), cx, |environment| {
                environment.variables.push(api_client::Variable::new(
                    "base_url".into(),
                    "https://staging.example.com".into(),
                ));
            });
            store.update_environment(Some(comparison_id), cx, |environment| {
                environment.variables.push(api_client::Variable::new(
                    "base_url".into(),
                    "https://comparison.example.com".into(),
                ));
            });
            store.set_active_environment(Some(production_id), cx);
        });

        let collection = api_client::Collection::new("Sample".into());
        let mut request = api_client::Request::new(collection.id, "Get users".into());
        request.pinned_environment_id = Some(staging_id);

        store.read_with(cx, |store, _| {
            let context = store.variable_context_for_environment(&request, comparison_id);
            assert_eq!(
                context.environment.map(|environment| environment.id),
                Some(comparison_id),
                "the explicitly picked comparison environment must win over both the pinned and active environment"
            );
        });
    }

    #[gpui::test]
    fn setting_a_request_s_pinned_environment_persists_it(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let env_id = store.update(cx, |store, cx| {
            store.create_environment("Staging".into(), cx)
        });
        let collection = api_client::Collection::new("Sample".into());
        let collection_id = collection.id;
        store.update(cx, |store, _| store.collections.push(collection));
        let request_id = store.update(cx, |store, cx| {
            store.create_request(collection_id, "Get users".into(), None, cx)
        });
        store.update(cx, |store, cx| {
            store.choose_request_environment(request_id, Some(env_id), cx);
        });
        store.read_with(cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(request.pinned_environment_id, Some(env_id));
        });
        store.update(cx, |store, cx| {
            store.choose_request_environment(request_id, None, cx);
        });
        store.read_with(cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(request.pinned_environment_id, None);
        });
    }

    /// An environment can be deleted from under a request that was pinned to it.
    /// The pin has to go with it, and what a send follows is the next pin the
    /// reader made -- not whichever environment happens to be active.
    #[gpui::test]
    fn deleting_an_environment_takes_the_pins_to_it(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let (doomed, production, elsewhere) = store.update(cx, |store, cx| {
            (
                store.create_environment("Doomed".into(), cx),
                store.create_environment("Production".into(), cx),
                store.create_environment("Elsewhere".into(), cx),
            )
        });
        let collection = api_client::Collection::new("Sample".into());
        let collection_id = collection.id;
        store.update(cx, |store, _| store.collections.push(collection));
        let request_id = store.update(cx, |store, cx| {
            store.create_request(collection_id, "Get users".into(), None, cx)
        });
        store.update(cx, |store, cx| {
            store.set_active_environment(Some(elsewhere), cx);
            store.toggle_request_pinned_environment(request_id, doomed, cx);
            store.toggle_request_pinned_environment(request_id, production, cx);
            store.choose_request_environment(request_id, Some(doomed), cx);
        });

        store.update(cx, |store, cx| store.delete_environment(doomed, cx));

        store.read_with(cx, |store, _| {
            assert_eq!(
                store
                    .pinned_environments_for(request_id)
                    .iter()
                    .map(|environment| environment.id)
                    .collect::<Vec<_>>(),
                vec![production],
                "the pin to the deleted environment goes with it"
            );
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(
                request.pinned_environments(),
                vec![production],
                "and it is gone from the request itself, not merely hidden"
            );
            assert_eq!(
                request.chosen_environment(),
                None,
                "the request is no longer sent to an environment that is gone"
            );
            assert_eq!(
                store
                    .effective_environment_for(request)
                    .map(|environment| environment.id),
                Some(elsewhere),
                "so it follows the active environment until the reader chooses again"
            );
        });
    }

    /// The same request read before the deletion has been noticed: a request
    /// sent to an environment that is gone has to fall back rather than send
    /// itself nowhere.
    #[gpui::test]
    fn a_send_falls_back_when_the_chosen_environment_is_gone(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let (doomed, elsewhere) = store.update(cx, |store, cx| {
            (
                store.create_environment("Doomed".into(), cx),
                store.create_environment("Elsewhere".into(), cx),
            )
        });
        store.update(cx, |store, cx| {
            store.set_active_environment(Some(elsewhere), cx)
        });
        let collection = api_client::Collection::new("Sample".into());
        let mut request = api_client::Request::new(collection.id, "Get users".into());
        request.choose_environment(Some(doomed));
        // Straight off the environment list, so the request keeps the stale
        // choice the way a collection saved before the deletion would.
        store.update(cx, |store, _| {
            store
                .environments
                .retain(|environment| environment.id != doomed)
        });

        store.read_with(cx, |store, _| {
            assert_eq!(
                store
                    .effective_environment_for(&request)
                    .map(|environment| environment.id),
                Some(elsewhere)
            );
        });
    }

    /// An environment that has been deleted is nothing to send against: the
    /// request would go out resolved against nothing and come back labelled with
    /// a name that no longer exists.
    #[gpui::test]
    fn what_to_send_refuses_an_environment_that_is_gone(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let doomed = store.update(cx, |store, cx| {
            store.create_environment("Doomed".into(), cx)
        });
        let collection = api_client::Collection::new("Sample".into());
        let collection_id = collection.id;
        store.update(cx, |store, _| store.collections.push(collection));
        let request_id = store.update(cx, |store, cx| {
            store.create_request(collection_id, "Get users".into(), None, cx)
        });
        store.update(cx, |store, cx| {
            store.update_request(request_id, cx, |request| {
                request.url = "https://example.com/ping".into();
            });
        });

        store.read_with(cx, |store, _| {
            assert!(
                store.what_to_send(request_id, doomed).is_some(),
                "an environment that is there is one to send against"
            );
        });

        store.update(cx, |store, cx| store.delete_environment(doomed, cx));
        store.read_with(cx, |store, _| {
            assert!(store.what_to_send(request_id, doomed).is_none());
        });
    }

    #[gpui::test]
    fn pinning_an_environment_is_not_choosing_it(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let staging_id = store.update(cx, |store, cx| {
            store.create_environment("Staging".into(), cx)
        });
        let production_id = store.update(cx, |store, cx| {
            store.create_environment("Production".into(), cx)
        });
        let other_id = store.update(cx, |store, cx| store.create_environment("Local".into(), cx));
        let collection = api_client::Collection::new("Sample".into());
        let collection_id = collection.id;
        store.update(cx, |store, _| store.collections.push(collection));
        let request_id = store.update(cx, |store, cx| {
            store.create_request(collection_id, "Get users".into(), None, cx)
        });
        store.update(cx, |store, cx| {
            store.set_active_environment(Some(other_id), cx);
            store.toggle_request_pinned_environment(request_id, staging_id, cx);
            store.toggle_request_pinned_environment(request_id, production_id, cx);
        });

        store.read_with(cx, |store, _| {
            assert_eq!(
                store
                    .pinned_environments_for(request_id)
                    .iter()
                    .map(|environment| environment.id)
                    .collect::<Vec<_>>(),
                vec![staging_id, production_id],
                "pinning a second environment must not drop the first"
            );
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(
                store.effective_environment_for(request).map(|it| it.id),
                Some(other_id),
                "and pinning must not send the request anywhere: it still follows \
                 the active environment"
            );
        });

        store.update(cx, |store, cx| {
            store.choose_request_environment(request_id, Some(production_id), cx);
        });
        store.read_with(cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(
                store.effective_environment_for(request).map(|it| it.id),
                Some(production_id),
                "choosing one is what sends the request there"
            );
            assert_eq!(
                request.pinned_environments(),
                vec![staging_id, production_id],
                "and choosing must leave the pins as they were"
            );
        });

        store.update(cx, |store, cx| {
            store.toggle_request_pinned_environment(request_id, production_id, cx);
        });
        store.read_with(cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(
                request.pinned_environments(),
                vec![staging_id],
                "unpinning takes the pin"
            );
            assert_eq!(
                store.effective_environment_for(request).map(|it| it.id),
                Some(production_id),
                "and leaves where the request is sent alone"
            );
        });

        store.update(cx, |store, cx| {
            store.choose_request_environment(request_id, None, cx);
        });
        store.read_with(cx, |store, _| {
            let request = store.requests.iter().find(|r| r.id == request_id).unwrap();
            assert_eq!(
                store.effective_environment_for(request).map(|it| it.id),
                Some(other_id),
                "and with no choice the request follows the active environment again"
            );
        });
    }

    /// A pin whose environment has since been deleted must simply not show up,
    /// rather than leaving a hole the caller has to guess the meaning of.
    #[gpui::test]
    fn a_pin_to_a_deleted_environment_is_skipped(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let staging_id = store.update(cx, |store, cx| {
            store.create_environment("Staging".into(), cx)
        });
        let collection = api_client::Collection::new("Sample".into());
        let collection_id = collection.id;
        store.update(cx, |store, _| store.collections.push(collection));
        let request_id = store.update(cx, |store, cx| {
            store.create_request(collection_id, "Get users".into(), None, cx)
        });
        store.update(cx, |store, cx| {
            store.toggle_request_pinned_environment(request_id, staging_id, cx);
            store.environments.retain(|it| it.id != staging_id);
        });
        store.read_with(cx, |store, _| {
            assert!(store.pinned_environments_for(request_id).is_empty());
        });
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

    #[gpui::test]
    fn new_collections_are_ordered_after_existing_ones(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let first = store.update(cx, |store, cx| store.create_collection("A".into(), cx));
        let second = store.update(cx, |store, cx| store.create_collection("B".into(), cx));
        store.read_with(cx, |store, _| {
            let first = store.collections.iter().find(|c| c.id == first).unwrap();
            let second = store.collections.iter().find(|c| c.id == second).unwrap();
            assert!(second.order > first.order);
        });
    }

    #[gpui::test]
    fn reorder_collection_swaps_with_its_neighbor(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let first = store.update(cx, |store, cx| store.create_collection("A".into(), cx));
        let second = store.update(cx, |store, cx| store.create_collection("B".into(), cx));

        store.update(cx, |store, cx| store.reorder_collection(second, -1, cx));

        store.read_with(cx, |store, _| {
            let first = store.collections.iter().find(|c| c.id == first).unwrap();
            let second = store.collections.iter().find(|c| c.id == second).unwrap();
            assert!(second.order < first.order, "B must now sort before A");
        });
    }

    #[gpui::test]
    fn reorder_collection_at_the_boundary_is_a_no_op(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let first = store.update(cx, |store, cx| store.create_collection("A".into(), cx));
        let order_before = store.read_with(cx, |store, _| store.collections[0].order);

        store.update(cx, |store, cx| store.reorder_collection(first, -1, cx));

        store.read_with(cx, |store, _| {
            assert_eq!(store.collections[0].order, order_before);
        });
    }

    #[gpui::test]
    fn reposition_collection_moves_it_next_to_a_different_collection(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let a = store.update(cx, |store, cx| store.create_collection("A".into(), cx));
        let b = store.update(cx, |store, cx| store.create_collection("B".into(), cx));
        let c = store.update(cx, |store, cx| store.create_collection("C".into(), cx));

        let moved = store.update(cx, |store, cx| {
            store.reposition_collection(c, a, RelativePosition::Before, cx)
        });
        assert!(moved);

        store.read_with(cx, |store, _| {
            let mut ordered = store.collections.clone();
            ordered.sort_by_key(|collection| collection.order);
            let order: Vec<CollectionId> = ordered.into_iter().map(|c| c.id).collect();
            assert_eq!(order, vec![c, a, b]);
        });
    }

    #[gpui::test]
    fn duplicate_request_copies_every_field_except_id_and_name(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let collection_id = store.update(cx, |store, cx| store.create_collection("A".into(), cx));
        let request_id = store.update(cx, |store, cx| {
            store.create_request(collection_id, "Get users".into(), None, cx)
        });
        store.update(cx, |store, cx| {
            store.update_request(request_id, cx, |request| {
                request.url = "https://api.example.com/users".into();
                request.headers.push(api_client::Header {
                    key: "Accept".into(),
                    value: "application/json".into(),
                    enabled: true,
                    description: None,
                });
            });
        });

        let duplicate_id = store
            .update(cx, |store, cx| store.duplicate_request(request_id, cx))
            .expect("duplicate_request must succeed for an existing request");

        store.read_with(cx, |store, _| {
            assert_eq!(store.requests.len(), 2);
            let original = store.requests.iter().find(|r| r.id == request_id).unwrap();
            let duplicate = store
                .requests
                .iter()
                .find(|r| r.id == duplicate_id)
                .unwrap();
            assert_ne!(duplicate.id, original.id);
            assert_eq!(duplicate.name, "Copy of Get users");
            assert_eq!(duplicate.url, original.url);
            assert_eq!(duplicate.headers.len(), original.headers.len());
            assert_eq!(duplicate.headers[0].key, original.headers[0].key);
            assert_eq!(duplicate.headers[0].value, original.headers[0].value);
            assert_eq!(duplicate.collection_id, original.collection_id);
            assert_eq!(duplicate.folder_id, original.folder_id);
        });
    }

    #[gpui::test]
    fn duplicate_request_is_none_for_an_unknown_id(cx: &mut TestAppContext) {
        let store = new_store(cx);
        let missing = RequestId::new_v4();
        let duplicate = store.update(cx, |store, cx| store.duplicate_request(missing, cx));
        assert!(duplicate.is_none());
    }
}
