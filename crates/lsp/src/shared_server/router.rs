use collections::{BTreeMap, HashMap, HashSet};
use serde_json::{Value, json};

pub(crate) type ClientId = u64;

/// What the router asks its owner to do with a message or with the server.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Outgoing {
    ToServer(Value),
    ToClient(ClientId, Value),
    /// The server answered `initialize`; later clients may join only when it
    /// can be told about their folders.
    Ready {
        accepts_more_folders: bool,
    },
    /// The last window left; the process should be shut down.
    Shutdown,
}

struct Client {
    folders: Vec<WorkspaceFolder>,
    open_documents: HashMap<String, ClientDocument>,
    joined_after_start: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct WorkspaceFolder {
    uri: String,
    name: String,
}

impl WorkspaceFolder {
    fn to_json(&self) -> Value {
        json!({ "uri": self.uri, "name": self.name })
    }
}

struct ClientDocument {
    version: i64,
    text: String,
    language_id: String,
}

/// The server holds one text per URI. `offset` turns the owner's version
/// numbers into the server's, which must keep growing across owners.
struct SharedDocument {
    owner: ClientId,
    offset: i64,
}

enum ServerState {
    NotStarted,
    Initializing,
    Ready { result: Value },
}

struct ServerRequest {
    method: String,
    params: Value,
    answered: bool,
    waiting: HashSet<ClientId>,
}

/// Lets several windows talk to one language server process. Each window
/// speaks LSP as if it had the server to itself.
pub(crate) struct Router {
    clients: BTreeMap<ClientId, Client>,
    server: ServerState,
    initializing_client: Option<ClientId>,
    next_request_id: i64,
    forwarded_requests: HashMap<i64, (ClientId, Value)>,
    server_requests: HashMap<String, ServerRequest>,
    documents: HashMap<String, SharedDocument>,
    last_active: Option<ClientId>,
    /// Capabilities the server registered so far, repeated to every window
    /// that joins later so it watches the same files.
    registrations: Vec<Value>,
    next_replay_id: u64,
}

impl Router {
    pub(crate) fn new() -> Self {
        Self {
            clients: BTreeMap::default(),
            server: ServerState::NotStarted,
            initializing_client: None,
            next_request_id: 1,
            forwarded_requests: HashMap::default(),
            server_requests: HashMap::default(),
            documents: HashMap::default(),
            last_active: None,
            registrations: Vec::new(),
            next_replay_id: 1,
        }
    }

    pub(crate) fn has_clients(&self) -> bool {
        !self.clients.is_empty()
    }

    /// Adds a window whose first message, `initialize`, is `initialize_request`.
    pub(crate) fn attach(&mut self, client: ClientId, initialize_request: Value) -> Vec<Outgoing> {
        let params = &initialize_request["params"];
        self.clients.insert(
            client,
            Client {
                folders: folders_of_initialize(params),
                open_documents: HashMap::default(),
                joined_after_start: !matches!(self.server, ServerState::NotStarted),
            },
        );
        self.last_active = Some(client);
        match &self.server {
            ServerState::NotStarted => {
                self.server = ServerState::Initializing;
                self.initializing_client = Some(client);
                vec![self.forward_request(client, initialize_request)]
            }
            ServerState::Ready { result } => vec![Outgoing::ToClient(
                client,
                json!({ "jsonrpc": "2.0", "id": initialize_request["id"], "result": result }),
            )],
            ServerState::Initializing => {
                log::error!("a window joined a shared language server before it started");
                vec![Outgoing::ToClient(
                    client,
                    error_response(&initialize_request["id"], "the shared server is starting"),
                )]
            }
        }
    }

    pub(crate) fn detach(&mut self, client: ClientId) -> Vec<Outgoing> {
        let Some(departed) = self.clients.remove(&client) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if self.last_active == Some(client) {
            self.last_active = self.clients.keys().next().copied();
        }
        for uri in departed.open_documents.keys() {
            self.release_document(client, uri, &mut out);
        }
        self.forwarded_requests
            .retain(|_, (requester, _)| *requester != client);

        let mut unanswered = Vec::new();
        for (id, request) in &mut self.server_requests {
            if request.waiting.remove(&client) && !request.answered && request.waiting.is_empty() {
                unanswered.push(id.clone());
            }
        }
        for id in unanswered {
            if let Some(request) = self.server_requests.remove(&id) {
                out.push(Outgoing::ToServer(json!({
                    "jsonrpc": "2.0",
                    "id": serde_json::from_str::<Value>(&id).unwrap_or(Value::Null),
                    "result": empty_result(&request.method, &request.params),
                })));
            }
        }

        let removed = departed
            .folders
            .iter()
            .filter(|folder| !self.folder_is_held(&folder.uri))
            .map(WorkspaceFolder::to_json)
            .collect::<Vec<_>>();
        if !removed.is_empty() && self.has_clients() {
            out.push(Outgoing::ToServer(did_change_folders(Vec::new(), removed)));
        }
        if !self.has_clients() {
            out.push(Outgoing::Shutdown);
        }
        out
    }

    pub(crate) fn from_client(&mut self, client: ClientId, message: Value) -> Vec<Outgoing> {
        if !self.clients.contains_key(&client) {
            return Vec::new();
        }
        let method = message["method"].as_str().map(str::to_owned);
        let has_id = message.get("id").is_some_and(|id| !id.is_null());
        match (method, has_id) {
            (Some(method), true) => self.client_request(client, &method, message),
            (Some(method), false) => self.client_notification(client, &method, message),
            (None, true) => self.client_response(client, message),
            (None, false) => Vec::new(),
        }
    }

    pub(crate) fn from_server(&mut self, message: Value) -> Vec<Outgoing> {
        let method = message["method"].as_str().map(str::to_owned);
        let has_id = message.get("id").is_some_and(|id| !id.is_null());
        match (method, has_id) {
            (Some(method), true) => self.server_request(&method, message),
            (Some(method), false) => self.server_notification(&method, message),
            (None, true) => self.server_response(message),
            (None, false) => Vec::new(),
        }
    }

    fn client_request(&mut self, client: ClientId, method: &str, message: Value) -> Vec<Outgoing> {
        self.last_active = Some(client);
        if method == "shutdown" {
            return vec![Outgoing::ToClient(
                client,
                json!({ "jsonrpc": "2.0", "id": message["id"], "result": null }),
            )];
        }
        let mut out = Vec::new();
        if let Some(uri) = document_uri(&message["params"]) {
            self.claim_document(client, &uri, &mut out);
        }
        out.push(self.forward_request(client, message));
        out
    }

    fn forward_request(&mut self, client: ClientId, mut message: Value) -> Outgoing {
        let id = self.next_request_id;
        self.next_request_id += 1;
        let client_id = std::mem::replace(&mut message["id"], json!(id));
        self.forwarded_requests.insert(id, (client, client_id));
        Outgoing::ToServer(message)
    }

    fn client_notification(
        &mut self,
        client: ClientId,
        method: &str,
        mut message: Value,
    ) -> Vec<Outgoing> {
        let mut out = Vec::new();
        match method {
            "exit" => {}
            "initialized" => {
                let Some(state) = self.clients.get(&client) else {
                    return out;
                };
                if state.joined_after_start {
                    let added = state
                        .folders
                        .iter()
                        .filter(|folder| !self.folder_is_held_by_other(client, &folder.uri))
                        .map(WorkspaceFolder::to_json)
                        .collect::<Vec<_>>();
                    if !added.is_empty() {
                        out.push(Outgoing::ToServer(did_change_folders(added, Vec::new())));
                    }
                    if !self.registrations.is_empty() {
                        let id = format!("shared-server-replay-{}", self.next_replay_id);
                        self.next_replay_id += 1;
                        out.push(Outgoing::ToClient(
                            client,
                            json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "method": "client/registerCapability",
                                "params": { "registrations": self.registrations },
                            }),
                        ));
                    }
                } else {
                    out.push(Outgoing::ToServer(message));
                }
            }
            "textDocument/didOpen" => {
                let item = &message["params"]["textDocument"];
                let (Some(uri), Some(text)) = (item["uri"].as_str(), item["text"].as_str()) else {
                    return out;
                };
                let uri = uri.to_owned();
                let document = ClientDocument {
                    version: item["version"].as_i64().unwrap_or(0),
                    text: text.to_owned(),
                    language_id: item["languageId"].as_str().unwrap_or_default().to_owned(),
                };
                if let Some(state) = self.clients.get_mut(&client) {
                    state.open_documents.insert(uri.clone(), document);
                }
                match self.documents.get(&uri) {
                    Some(shared) if shared.owner != client => {}
                    _ => {
                        self.documents.insert(
                            uri,
                            SharedDocument {
                                owner: client,
                                offset: 0,
                            },
                        );
                        out.push(Outgoing::ToServer(message));
                    }
                }
            }
            "textDocument/didChange" => {
                let Some(uri) = message["params"]["textDocument"]["uri"].as_str() else {
                    return out;
                };
                let uri = uri.to_owned();
                let version = message["params"]["textDocument"]["version"].as_i64();
                if let Some(document) = self
                    .clients
                    .get_mut(&client)
                    .and_then(|state| state.open_documents.get_mut(&uri))
                {
                    if let Some(changes) = message["params"]["contentChanges"].as_array() {
                        for change in changes {
                            apply_change(&mut document.text, change);
                        }
                    }
                    if let Some(version) = version {
                        document.version = version;
                    }
                }
                if let Some(shared) = self.documents.get(&uri)
                    && shared.owner == client
                {
                    if let Some(version) = version {
                        message["params"]["textDocument"]["version"] =
                            json!(version + shared.offset);
                    }
                    out.push(Outgoing::ToServer(message));
                }
            }
            "textDocument/didSave" => {
                if let Some(uri) = message["params"]["textDocument"]["uri"].as_str()
                    && self
                        .documents
                        .get(uri)
                        .is_some_and(|shared| shared.owner == client)
                {
                    out.push(Outgoing::ToServer(message));
                }
            }
            "textDocument/didClose" => {
                let Some(uri) = message["params"]["textDocument"]["uri"].as_str() else {
                    return out;
                };
                let uri = uri.to_owned();
                if let Some(state) = self.clients.get_mut(&client) {
                    state.open_documents.remove(&uri);
                }
                self.release_document(client, &uri, &mut out);
            }
            "workspace/didChangeWorkspaceFolders" => {
                let event = &message["params"]["event"];
                let added = parse_folders(&event["added"]);
                let removed = parse_folders(&event["removed"]);
                let added_for_server = added
                    .iter()
                    .filter(|folder| !self.folder_is_held(&folder.uri))
                    .map(WorkspaceFolder::to_json)
                    .collect::<Vec<_>>();
                if let Some(state) = self.clients.get_mut(&client) {
                    state
                        .folders
                        .retain(|folder| removed.iter().all(|gone| gone.uri != folder.uri));
                    state.folders.extend(added);
                }
                let removed_for_server = removed
                    .iter()
                    .filter(|folder| !self.folder_is_held(&folder.uri))
                    .map(WorkspaceFolder::to_json)
                    .collect::<Vec<_>>();
                if !added_for_server.is_empty() || !removed_for_server.is_empty() {
                    out.push(Outgoing::ToServer(did_change_folders(
                        added_for_server,
                        removed_for_server,
                    )));
                }
            }
            "$/cancelRequest" => {
                let client_id = &message["params"]["id"];
                if let Some(server_id) = self
                    .forwarded_requests
                    .iter()
                    .find(|(_, (requester, id))| *requester == client && id == client_id)
                    .map(|(server_id, _)| *server_id)
                {
                    message["params"]["id"] = json!(server_id);
                    out.push(Outgoing::ToServer(message));
                }
            }
            _ => out.push(Outgoing::ToServer(message)),
        }
        out
    }

    fn client_response(&mut self, client: ClientId, message: Value) -> Vec<Outgoing> {
        let key = message["id"].to_string();
        let Some(request) = self.server_requests.get_mut(&key) else {
            return Vec::new();
        };
        request.waiting.remove(&client);
        let mut out = Vec::new();
        if !request.answered {
            request.answered = true;
            out.push(Outgoing::ToServer(message));
        }
        if request.waiting.is_empty() {
            self.server_requests.remove(&key);
        }
        out
    }

    fn server_response(&mut self, mut message: Value) -> Vec<Outgoing> {
        let Some(id) = message["id"].as_i64() else {
            return Vec::new();
        };
        let Some((client, client_id)) = self.forwarded_requests.remove(&id) else {
            return Vec::new();
        };
        message["id"] = client_id;
        let mut out = Vec::new();
        if self.initializing_client == Some(client)
            && matches!(self.server, ServerState::Initializing)
        {
            self.initializing_client = None;
            let result = message["result"].clone();
            let change_notifications =
                &result["capabilities"]["workspace"]["workspaceFolders"]["changeNotifications"];
            let accepts_more_folders = message.get("error").is_none()
                && (change_notifications.as_bool() == Some(true)
                    || change_notifications.is_string());
            self.server = ServerState::Ready { result };
            out.push(Outgoing::Ready {
                accepts_more_folders,
            });
        }
        self.map_versions_for(client, &mut message["result"]);
        out.insert(0, Outgoing::ToClient(client, message));
        out
    }

    fn server_request(&mut self, method: &str, mut message: Value) -> Vec<Outgoing> {
        match method {
            "client/registerCapability" => {
                if let Some(registrations) = message["params"]["registrations"].as_array() {
                    self.registrations.extend(registrations.iter().cloned());
                }
            }
            "client/unregisterCapability" => {
                if let Some(unregistrations) = message["params"]["unregisterations"].as_array() {
                    self.registrations.retain(|registration| {
                        unregistrations
                            .iter()
                            .all(|gone| gone["id"] != registration["id"])
                    });
                }
            }
            _ => {}
        }
        if method == "workspace/workspaceFolders" {
            let folders = self.all_folders();
            let result = if folders.is_empty() {
                Value::Null
            } else {
                Value::Array(folders.iter().map(WorkspaceFolder::to_json).collect())
            };
            return vec![Outgoing::ToServer(
                json!({ "jsonrpc": "2.0", "id": message["id"], "result": result }),
            )];
        }

        let targets: Vec<ClientId> = if is_broadcast_request(method) {
            self.clients.keys().copied().collect()
        } else {
            let uri = match method {
                "workspace/configuration" => message["params"]["items"][0]["scopeUri"]
                    .as_str()
                    .map(str::to_owned),
                "workspace/applyEdit" => first_edited_uri(&message["params"]["edit"]),
                _ => document_uri(&message["params"]),
            };
            self.client_for(uri.as_deref()).into_iter().collect()
        };
        if targets.is_empty() {
            return vec![Outgoing::ToServer(json!({
                "jsonrpc": "2.0",
                "id": message["id"],
                "result": empty_result(method, &message["params"]),
            }))];
        }
        self.server_requests.insert(
            message["id"].to_string(),
            ServerRequest {
                method: method.to_owned(),
                params: message["params"].clone(),
                answered: false,
                waiting: targets.iter().copied().collect(),
            },
        );
        if method == "workspace/applyEdit"
            && let Some(&client) = targets.first()
        {
            self.map_versions_for(client, &mut message["params"]["edit"]);
        }
        targets
            .into_iter()
            .map(|client| Outgoing::ToClient(client, message.clone()))
            .collect()
    }

    fn server_notification(&mut self, method: &str, mut message: Value) -> Vec<Outgoing> {
        let uri = document_uri(&message["params"]);
        let Some(uri) = uri else {
            return self
                .clients
                .keys()
                .map(|client| Outgoing::ToClient(*client, message.clone()))
                .collect();
        };
        if let Some(shared) = self.documents.get(&uri) {
            let owner = shared.owner;
            if method == "textDocument/publishDiagnostics"
                && let Some(version) = message["params"]["version"].as_i64()
            {
                message["params"]["version"] = json!(version - shared.offset);
            }
            return vec![Outgoing::ToClient(owner, message)];
        }
        let holders = self.clients_with_folder_for(&uri);
        let targets: Vec<ClientId> = if holders.is_empty() {
            self.clients.keys().copied().collect()
        } else {
            holders
        };
        targets
            .into_iter()
            .map(|client| Outgoing::ToClient(client, message.clone()))
            .collect()
    }

    /// Hands the server's single copy of `uri` to `client`, replacing another
    /// window's text with this window's.
    fn claim_document(&mut self, client: ClientId, uri: &str, out: &mut Vec<Outgoing>) {
        let Some(document) = self
            .clients
            .get(&client)
            .and_then(|state| state.open_documents.get(uri))
        else {
            return;
        };
        let Some(shared) = self.documents.get_mut(uri) else {
            return;
        };
        if shared.owner == client {
            return;
        }
        let previous_server_version = self
            .clients
            .get(&shared.owner)
            .and_then(|state| state.open_documents.get(uri))
            .map(|owner_document| owner_document.version + shared.offset)
            .unwrap_or(0);
        let server_version = previous_server_version + 1;
        shared.owner = client;
        shared.offset = server_version - document.version;
        out.push(Outgoing::ToServer(json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": uri, "version": server_version },
                "contentChanges": [{ "text": document.text }],
            },
        })));
    }

    /// Called when `client` no longer has `uri` open: another window that has
    /// it open takes the server's copy over, or the server closes it.
    fn release_document(&mut self, client: ClientId, uri: &str, out: &mut Vec<Outgoing>) {
        let Some(shared) = self.documents.get(uri) else {
            return;
        };
        if shared.owner != client {
            return;
        }
        let successor = self
            .clients
            .iter()
            .find(|(id, state)| **id != client && state.open_documents.contains_key(uri))
            .map(|(id, _)| *id);
        match successor {
            Some(successor) => {
                let Some(document) = self
                    .clients
                    .get(&successor)
                    .and_then(|state| state.open_documents.get(uri))
                else {
                    return;
                };
                // Reopening resets the server's version count, so the
                // successor's own numbers can be used as they are.
                let reopen = json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/didOpen",
                    "params": {
                        "textDocument": {
                            "uri": uri,
                            "languageId": document.language_id,
                            "version": document.version,
                            "text": document.text,
                        },
                    },
                });
                out.push(Outgoing::ToServer(json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/didClose",
                    "params": { "textDocument": { "uri": uri } },
                })));
                out.push(Outgoing::ToServer(reopen));
                self.documents.insert(
                    uri.to_owned(),
                    SharedDocument {
                        owner: successor,
                        offset: 0,
                    },
                );
            }
            None => {
                self.documents.remove(uri);
                out.push(Outgoing::ToServer(json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/didClose",
                    "params": { "textDocument": { "uri": uri } },
                })));
            }
        }
    }

    fn map_versions_for(&self, client: ClientId, value: &mut Value) {
        let Some(changes) = value
            .get_mut("documentChanges")
            .and_then(Value::as_array_mut)
        else {
            return;
        };
        for change in changes {
            let Some(identifier) = change.get_mut("textDocument") else {
                continue;
            };
            let Some(uri) = identifier["uri"].as_str() else {
                continue;
            };
            let Some(shared) = self.documents.get(uri) else {
                continue;
            };
            if shared.owner == client
                && let Some(version) = identifier["version"].as_i64()
            {
                identifier["version"] = json!(version - shared.offset);
            }
        }
    }

    fn client_for(&self, uri: Option<&str>) -> Option<ClientId> {
        if let Some(uri) = uri {
            if let Some(shared) = self.documents.get(uri) {
                return Some(shared.owner);
            }
            if let Some(client) = self.clients_with_folder_for(uri).first() {
                return Some(*client);
            }
        }
        self.last_active
            .filter(|client| self.clients.contains_key(client))
            .or_else(|| self.clients.keys().next().copied())
    }

    /// Windows whose folder contains `uri`, the deepest folder first.
    fn clients_with_folder_for(&self, uri: &str) -> Vec<ClientId> {
        let mut matches = self
            .clients
            .iter()
            .filter_map(|(client, state)| {
                state
                    .folders
                    .iter()
                    .filter(|folder| uri_is_inside(uri, &folder.uri))
                    .map(|folder| folder.uri.len())
                    .max()
                    .map(|depth| (*client, depth))
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|(_, depth)| std::cmp::Reverse(*depth));
        matches.into_iter().map(|(client, _)| client).collect()
    }

    fn all_folders(&self) -> Vec<WorkspaceFolder> {
        let mut seen = HashSet::default();
        self.clients
            .values()
            .flat_map(|state| state.folders.iter())
            .filter(|folder| seen.insert(folder.uri.clone()))
            .cloned()
            .collect()
    }

    fn folder_is_held(&self, uri: &str) -> bool {
        self.clients
            .values()
            .any(|state| state.folders.iter().any(|folder| folder.uri == uri))
    }

    fn folder_is_held_by_other(&self, client: ClientId, uri: &str) -> bool {
        self.clients.iter().any(|(id, state)| {
            *id != client && state.folders.iter().any(|folder| folder.uri == uri)
        })
    }
}

fn is_broadcast_request(method: &str) -> bool {
    matches!(
        method,
        "client/registerCapability"
            | "client/unregisterCapability"
            | "window/workDoneProgress/create"
    ) || (method.starts_with("workspace/") && method.ends_with("/refresh"))
}

fn empty_result(method: &str, params: &Value) -> Value {
    match method {
        "workspace/configuration" => Value::Array(
            params["items"]
                .as_array()
                .map(|items| items.iter().map(|_| Value::Null).collect())
                .unwrap_or_default(),
        ),
        "workspace/applyEdit" => json!({ "applied": false }),
        _ => Value::Null,
    }
}

fn error_response(id: &Value, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32002, "message": message },
    })
}

fn did_change_folders(added: Vec<Value>, removed: Vec<Value>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "workspace/didChangeWorkspaceFolders",
        "params": { "event": { "added": added, "removed": removed } },
    })
}

fn folders_of_initialize(params: &Value) -> Vec<WorkspaceFolder> {
    let folders = parse_folders(&params["workspaceFolders"]);
    if !folders.is_empty() {
        return folders;
    }
    params["rootUri"]
        .as_str()
        .map(|uri| {
            vec![WorkspaceFolder {
                uri: uri.to_owned(),
                name: uri.rsplit('/').next().unwrap_or(uri).to_owned(),
            }]
        })
        .unwrap_or_default()
}

fn parse_folders(value: &Value) -> Vec<WorkspaceFolder> {
    value
        .as_array()
        .map(|folders| {
            folders
                .iter()
                .filter_map(|folder| {
                    Some(WorkspaceFolder {
                        uri: folder["uri"].as_str()?.to_owned(),
                        name: folder["name"].as_str().unwrap_or_default().to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn document_uri(params: &Value) -> Option<String> {
    params["textDocument"]["uri"]
        .as_str()
        .or_else(|| params["uri"].as_str())
        .map(str::to_owned)
}

fn first_edited_uri(edit: &Value) -> Option<String> {
    if let Some(changes) = edit["changes"].as_object()
        && let Some(uri) = changes.keys().next()
    {
        return Some(uri.clone());
    }
    edit["documentChanges"]
        .as_array()?
        .iter()
        .find_map(|change| {
            change["textDocument"]["uri"]
                .as_str()
                .or_else(|| change["uri"].as_str())
                .map(str::to_owned)
        })
}

fn uri_is_inside(uri: &str, folder: &str) -> bool {
    let folder = folder.trim_end_matches('/');
    uri == folder
        || uri
            .strip_prefix(folder)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Applies one LSP content change, whose positions count UTF-16 code units.
pub(crate) fn apply_change(text: &mut String, change: &Value) {
    let Some(new_text) = change["text"].as_str() else {
        return;
    };
    let range = &change["range"];
    if range.is_null() {
        *text = new_text.to_owned();
        return;
    }
    let start = byte_offset(text, &range["start"]);
    let end = byte_offset(text, &range["end"]).max(start);
    text.replace_range(start..end, new_text);
}

fn byte_offset(text: &str, position: &Value) -> usize {
    let line = position["line"].as_u64().unwrap_or(0) as usize;
    let character = position["character"].as_u64().unwrap_or(0) as usize;
    let mut line_start = 0;
    for _ in 0..line {
        match text[line_start..].find('\n') {
            Some(newline) => line_start += newline + 1,
            None => return text.len(),
        }
    }
    let line_end = text[line_start..]
        .find('\n')
        .map(|newline| line_start + newline)
        .unwrap_or(text.len());
    let mut units = 0;
    for (index, character_in_line) in text[line_start..line_end].char_indices() {
        if units >= character {
            return line_start + index;
        }
        units += character_in_line.len_utf16();
    }
    line_end
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: ClientId = 1;
    const SECOND: ClientId = 2;

    fn initialize(id: i64, folder: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "rootUri": folder,
                "workspaceFolders": [{ "uri": folder, "name": folder.rsplit('/').next() }],
                "initializationOptions": null,
            },
        })
    }

    fn server_messages(out: &[Outgoing]) -> Vec<Value> {
        out.iter()
            .filter_map(|message| match message {
                Outgoing::ToServer(message) => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    fn client_messages(out: &[Outgoing], client: ClientId) -> Vec<Value> {
        out.iter()
            .filter_map(|message| match message {
                Outgoing::ToClient(target, message) if *target == client => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    /// Starts a server with two windows attached: `/first` and `/second`.
    fn two_windows() -> Router {
        let mut router = Router::new();
        let out = router.attach(FIRST, initialize(7, "file:///first"));
        let forwarded = server_messages(&out);
        assert_eq!(forwarded.len(), 1);
        let server_id = forwarded[0]["id"].clone();

        let out = router.from_server(json!({
            "jsonrpc": "2.0",
            "id": server_id,
            "result": {
                "capabilities": {
                    "workspace": { "workspaceFolders": { "supported": true, "changeNotifications": true } }
                }
            },
        }));
        assert_eq!(client_messages(&out, FIRST)[0]["id"], json!(7));
        assert!(out.contains(&Outgoing::Ready {
            accepts_more_folders: true
        }));
        let out = router.from_client(
            FIRST,
            json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
        );
        assert_eq!(server_messages(&out)[0]["method"], "initialized");

        let out = router.attach(SECOND, initialize(3, "file:///second"));
        assert!(server_messages(&out).is_empty(), "the server starts once");
        assert_eq!(client_messages(&out, SECOND)[0]["id"], json!(3));
        let out = router.from_client(
            SECOND,
            json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
        );
        assert_eq!(
            server_messages(&out),
            vec![json!({
                "jsonrpc": "2.0",
                "method": "workspace/didChangeWorkspaceFolders",
                "params": { "event": {
                    "added": [{ "uri": "file:///second", "name": "second" }],
                    "removed": [],
                } },
            })],
            "a window that joins later adds its folder instead of initializing again"
        );
        router
    }

    fn open(uri: &str, version: i64, text: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": { "textDocument": {
                "uri": uri, "languageId": "yaml", "version": version, "text": text,
            } },
        })
    }

    fn change(uri: &str, version: i64, start: (u32, u32), end: (u32, u32), text: &str) -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{
                    "range": {
                        "start": { "line": start.0, "character": start.1 },
                        "end": { "line": end.0, "character": end.1 },
                    },
                    "text": text,
                }],
            },
        })
    }

    #[test]
    fn requests_from_each_window_get_their_own_answers() {
        let mut router = two_windows();
        let hover = |id: i64| {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "textDocument/hover",
                "params": { "position": { "line": 0, "character": 0 } },
            })
        };
        let first = server_messages(&router.from_client(FIRST, hover(5)));
        let second = server_messages(&router.from_client(SECOND, hover(5)));
        assert_ne!(
            first[0]["id"], second[0]["id"],
            "two windows may use the same id; the server must not see a clash"
        );

        let out = router
            .from_server(json!({ "jsonrpc": "2.0", "id": second[0]["id"], "result": "second" }));
        assert_eq!(
            client_messages(&out, SECOND),
            vec![json!({ "jsonrpc": "2.0", "id": 5, "result": "second" })]
        );
        assert!(client_messages(&out, FIRST).is_empty());
    }

    #[test]
    fn diagnostics_go_to_the_window_that_owns_the_file() {
        let mut router = two_windows();
        let diagnostics = |uri: &str| {
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": { "uri": uri, "diagnostics": [] },
            })
        };
        let out = router.from_server(diagnostics("file:///second/config.yaml"));
        assert_eq!(client_messages(&out, SECOND).len(), 1);
        assert!(client_messages(&out, FIRST).is_empty());

        let out = router.from_server(diagnostics("file:///elsewhere/x.yaml"));
        assert_eq!(client_messages(&out, FIRST).len(), 1);
        assert_eq!(client_messages(&out, SECOND).len(), 1);
    }

    #[test]
    fn settings_are_asked_from_the_window_of_the_file() {
        let mut router = two_windows();
        let out = router.from_server(json!({
            "jsonrpc": "2.0",
            "id": 40,
            "method": "workspace/configuration",
            "params": { "items": [{ "scopeUri": "file:///second/a.yaml", "section": "yaml" }] },
        }));
        assert_eq!(client_messages(&out, SECOND).len(), 1);
        assert!(client_messages(&out, FIRST).is_empty());

        let out = router.from_client(
            SECOND,
            json!({ "jsonrpc": "2.0", "id": 40, "result": [{}] }),
        );
        assert_eq!(
            server_messages(&out),
            vec![json!({ "jsonrpc": "2.0", "id": 40, "result": [{}] })]
        );
    }

    #[test]
    fn a_registration_reaches_every_window_and_is_answered_once() {
        let mut router = two_windows();
        let out = router.from_server(json!({
            "jsonrpc": "2.0",
            "id": "reg-1",
            "method": "client/registerCapability",
            "params": { "registrations": [] },
        }));
        assert_eq!(client_messages(&out, FIRST).len(), 1);
        assert_eq!(client_messages(&out, SECOND).len(), 1);

        let answer = json!({ "jsonrpc": "2.0", "id": "reg-1", "result": null });
        assert_eq!(
            server_messages(&router.from_client(FIRST, answer.clone())).len(),
            1
        );
        assert!(server_messages(&router.from_client(SECOND, answer)).is_empty());
    }

    #[test]
    fn a_window_that_joins_later_gets_the_earlier_registrations() {
        let mut router = Router::new();
        let out = router.attach(FIRST, initialize(1, "file:///first"));
        let server_id = server_messages(&out)[0]["id"].clone();
        router.from_server(json!({
            "jsonrpc": "2.0",
            "id": server_id,
            "result": { "capabilities": { "workspace": { "workspaceFolders": { "changeNotifications": true } } } },
        }));
        router.from_client(
            FIRST,
            json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
        );
        let watcher = json!({
            "id": "watch-yaml",
            "method": "workspace/didChangeWatchedFiles",
            "registerOptions": { "watchers": [{ "globPattern": "**/*.yaml" }] },
        });
        let formatting = json!({ "id": "format", "method": "textDocument/formatting" });
        router.from_server(json!({
            "jsonrpc": "2.0", "id": 1, "method": "client/registerCapability",
            "params": { "registrations": [watcher, formatting] },
        }));
        router.from_server(json!({
            "jsonrpc": "2.0", "id": 2, "method": "client/unregisterCapability",
            "params": { "unregisterations": [{ "id": "format", "method": "textDocument/formatting" }] },
        }));

        router.attach(SECOND, initialize(1, "file:///second"));
        let out = router.from_client(
            SECOND,
            json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
        );
        let replayed = client_messages(&out, SECOND);
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0]["method"], "client/registerCapability");
        assert_eq!(replayed[0]["params"]["registrations"], json!([watcher]));

        let out = router.from_client(
            SECOND,
            json!({ "jsonrpc": "2.0", "id": replayed[0]["id"], "result": null }),
        );
        assert!(
            server_messages(&out).is_empty(),
            "the server never asked this, so it must not get the answer"
        );
    }

    #[test]
    fn the_server_lists_the_folders_of_every_window() {
        let mut router = two_windows();
        let out = router.from_server(json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "workspace/workspaceFolders",
            "params": null,
        }));
        assert_eq!(
            server_messages(&out)[0]["result"],
            json!([
                { "uri": "file:///first", "name": "first" },
                { "uri": "file:///second", "name": "second" },
            ])
        );
    }

    #[test]
    fn shutdown_from_one_window_keeps_the_server_for_the_other() {
        let mut router = two_windows();
        let out = router.from_client(
            FIRST,
            json!({ "jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": null }),
        );
        assert!(server_messages(&out).is_empty());
        assert_eq!(client_messages(&out, FIRST)[0]["result"], Value::Null);

        let out = router.detach(FIRST);
        assert!(!out.contains(&Outgoing::Shutdown));
        assert_eq!(
            server_messages(&out),
            vec![json!({
                "jsonrpc": "2.0",
                "method": "workspace/didChangeWorkspaceFolders",
                "params": { "event": {
                    "added": [],
                    "removed": [{ "uri": "file:///first", "name": "first" }],
                } },
            })]
        );

        let out = router.detach(SECOND);
        assert!(
            out.contains(&Outgoing::Shutdown),
            "the last window stops the server"
        );
    }

    #[test]
    fn a_window_that_leaves_answers_nothing_more() {
        let mut router = two_windows();
        let out = router.from_server(json!({
            "jsonrpc": "2.0",
            "id": 41,
            "method": "workspace/configuration",
            "params": { "items": [{ "scopeUri": "file:///first/a.yaml" }, { "section": "x" }] },
        }));
        assert_eq!(client_messages(&out, FIRST).len(), 1);
        let out = router.detach(FIRST);
        assert!(
            server_messages(&out)
                .contains(&json!({ "jsonrpc": "2.0", "id": 41, "result": [null, null] })),
            "the server must not wait forever for a window that is gone"
        );
    }

    #[test]
    fn one_file_open_in_two_windows_has_one_text_on_the_server() {
        let mut router = two_windows();
        let uri = "file:///shared/a.yaml";
        let out = router.from_client(FIRST, open(uri, 0, "one"));
        assert_eq!(server_messages(&out).len(), 1);
        let out = router.from_client(SECOND, open(uri, 0, "one"));
        assert!(
            server_messages(&out).is_empty(),
            "the server already has this file"
        );

        let out = router.from_client(SECOND, change(uri, 1, (0, 3), (0, 3), " two"));
        assert!(
            server_messages(&out).is_empty(),
            "only the window whose text the server holds reports edits"
        );
        let out = router.from_client(FIRST, change(uri, 1, (0, 3), (0, 3), " 1"));
        assert_eq!(
            server_messages(&out)[0]["params"]["textDocument"]["version"],
            json!(1)
        );

        let out = router.from_client(
            SECOND,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "textDocument/hover",
                "params": { "textDocument": { "uri": uri }, "position": { "line": 0, "character": 0 } },
            }),
        );
        let sent = server_messages(&out);
        assert_eq!(
            sent[0],
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": { "uri": uri, "version": 2 },
                    "contentChanges": [{ "text": "one two" }],
                },
            }),
            "a request from the other window first hands it the server's copy"
        );
        assert_eq!(sent[1]["method"], "textDocument/hover");

        let out = router.from_server(json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": { "uri": uri, "version": 2, "diagnostics": [] },
        }));
        assert_eq!(
            client_messages(&out, SECOND)[0]["params"]["version"],
            json!(1),
            "versions are given back in the window's own numbering"
        );
        assert!(client_messages(&out, FIRST).is_empty());

        let out = router.from_client(
            SECOND,
            json!({ "jsonrpc": "2.0", "method": "textDocument/didClose", "params": { "textDocument": { "uri": uri } } }),
        );
        let sent = server_messages(&out);
        assert_eq!(sent[0]["method"], "textDocument/didClose");
        assert_eq!(sent[1]["method"], "textDocument/didOpen");
        assert_eq!(sent[1]["params"]["textDocument"]["text"], "one 1");

        let out = router.from_client(
            FIRST,
            json!({ "jsonrpc": "2.0", "method": "textDocument/didClose", "params": { "textDocument": { "uri": uri } } }),
        );
        assert_eq!(server_messages(&out)[0]["method"], "textDocument/didClose");
    }

    #[test]
    fn content_changes_count_utf16_units() {
        let mut text = "añ😀b\nsecond".to_owned();
        apply_change(
            &mut text,
            &json!({
                "range": { "start": { "line": 0, "character": 4 }, "end": { "line": 0, "character": 5 } },
                "text": "B",
            }),
        );
        assert_eq!(text, "añ😀B\nsecond");
        apply_change(
            &mut text,
            &json!({
                "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 6 } },
                "text": "2nd",
            }),
        );
        assert_eq!(text, "añ😀B\n2nd");
        apply_change(&mut text, &json!({ "text": "whole" }));
        assert_eq!(text, "whole");
    }
}
