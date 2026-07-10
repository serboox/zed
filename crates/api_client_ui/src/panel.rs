use crate::request_view::RequestView;
use crate::store::{ApiClientStore, ApiClientStoreEvent, GlobalApiClientStore};
use crate::text_prompt_modal::TextPromptModal;
use api_client::{Collection, CollectionId, EnvironmentId, Folder, FolderId, Request, RequestId};
use gpui::{
    AnyElement, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Pixels, Point, Render, ScrollHandle,
    Styled, Subscription, WeakEntity, Window, div,
};
use std::collections::HashSet;
use std::sync::Arc;
use ui::{
    ContextMenu, Icon, IconName, IconSize, Label, LabelSize, ScrollAxes, Scrollbars, Tooltip,
    WithScrollbar, prelude::*, right_click_menu,
};
use util::ResultExt;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};
use zed_actions::api_client_panel::{
    CollapseSelectedEntry, ExpandSelectedEntry, MoveSelectedDown, MoveSelectedUp, NewCollection,
    ToggleFocus,
};

const API_CLIENT_PANEL_KEY: &str = "ApiClientPanel";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectedEntity {
    Collection(CollectionId),
    Folder(FolderId),
    Request(RequestId),
}

enum TreeNode {
    Collection {
        collection: Collection,
        children: Vec<TreeNode>,
    },
    Folder {
        folder: Folder,
        children: Vec<TreeNode>,
    },
    Request(Request),
}

/// Builds the Collection -> Folder -> Request tree in the exact order it is
/// painted, so `flatten_navigable_entities` (keyboard nav) never drifts from
/// what is actually on screen. Mirrors `db_client_ui::panel::build_folder_tree`.
fn build_tree(
    collections: &[Collection],
    folders: &[Folder],
    requests: &[Request],
) -> Vec<TreeNode> {
    fn build_children(
        collection_id: CollectionId,
        parent_id: Option<FolderId>,
        folders: &[Folder],
        requests: &[Request],
    ) -> Vec<TreeNode> {
        let mut folder_children: Vec<&Folder> = folders
            .iter()
            .filter(|f| f.collection_id == collection_id && f.parent_id == parent_id)
            .collect();
        folder_children.sort_by_key(|f| f.order);
        let mut request_children: Vec<&Request> = requests
            .iter()
            .filter(|r| r.collection_id == collection_id && r.folder_id == parent_id)
            .collect();
        request_children.sort_by_key(|r| r.order);

        let mut nodes = Vec::new();
        for folder in folder_children {
            nodes.push((
                folder.order,
                TreeNode::Folder {
                    folder: folder.clone(),
                    children: build_children(collection_id, Some(folder.id), folders, requests),
                },
            ));
        }
        for request in request_children {
            nodes.push((request.order, TreeNode::Request(request.clone())));
        }
        nodes.sort_by_key(|(order, _)| *order);
        nodes.into_iter().map(|(_, node)| node).collect()
    }

    collections
        .iter()
        .map(|collection| TreeNode::Collection {
            collection: collection.clone(),
            children: build_children(collection.id, None, folders, requests),
        })
        .collect()
}

/// Flattens the tree into the same order it is painted, skipping the children
/// of collapsed folders/collections. Keyboard SelectNext/Previous/First/Last
/// walk this list. Mirrors `db_client_ui::panel::flatten_navigable_entities`.
fn flatten_navigable_entities(
    nodes: &[TreeNode],
    collapsed_collections: &HashSet<CollectionId>,
    collapsed_folders: &HashSet<FolderId>,
) -> Vec<SelectedEntity> {
    let mut flat = Vec::new();
    for node in nodes {
        match node {
            TreeNode::Collection {
                collection,
                children,
            } => {
                flat.push(SelectedEntity::Collection(collection.id));
                if !collapsed_collections.contains(&collection.id) {
                    flat.extend(flatten_navigable_entities(
                        children,
                        collapsed_collections,
                        collapsed_folders,
                    ));
                }
            }
            TreeNode::Folder { folder, children } => {
                flat.push(SelectedEntity::Folder(folder.id));
                if !collapsed_folders.contains(&folder.id) {
                    flat.extend(flatten_navigable_entities(
                        children,
                        collapsed_collections,
                        collapsed_folders,
                    ));
                }
            }
            TreeNode::Request(request) => flat.push(SelectedEntity::Request(request.id)),
        }
    }
    flat
}

pub struct ApiClientPanel {
    focus_handle: FocusHandle,
    store: Entity<ApiClientStore>,
    workspace: WeakEntity<Workspace>,
    collapsed_collections: HashSet<CollectionId>,
    collapsed_folders: HashSet<FolderId>,
    selected_entity: Option<SelectedEntity>,
    context_menu: Option<(Entity<ContextMenu>, Point<Pixels>, Subscription)>,
    tree_scroll_handle: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PanelEvent> for ApiClientPanel {}

impl Focusable for ApiClientPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ApiClientPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, _window, cx| {
            let store = cx.new(|cx| ApiClientStore::new(cx));
            cx.set_global(GlobalApiClientStore(store.clone()));
            let focus_handle = cx.focus_handle();
            let workspace_handle = workspace.weak_handle();
            cx.new(|cx| {
                let store_subscription = cx.subscribe(
                    &store,
                    |_this: &mut ApiClientPanel,
                     _store: Entity<ApiClientStore>,
                     _event: &ApiClientStoreEvent,
                     cx: &mut Context<ApiClientPanel>| {
                        cx.notify();
                    },
                );
                Self {
                    focus_handle,
                    store,
                    workspace: workspace_handle,
                    collapsed_collections: HashSet::new(),
                    collapsed_folders: HashSet::new(),
                    selected_entity: None,
                    context_menu: None,
                    tree_scroll_handle: ScrollHandle::new(),
                    _subscriptions: vec![store_subscription],
                }
            })
        })
    }

    fn navigable_entities(&self, cx: &Context<Self>) -> Vec<SelectedEntity> {
        let store = self.store.read(cx);
        let nodes = build_tree(&store.collections, &store.folders, &store.requests);
        flatten_navigable_entities(&nodes, &self.collapsed_collections, &self.collapsed_folders)
    }

    fn toggle_folder_collapsed(&mut self, id: FolderId, cx: &mut Context<Self>) {
        if !self.collapsed_folders.remove(&id) {
            self.collapsed_folders.insert(id);
        }
        cx.notify();
    }

    fn toggle_collection_collapsed(&mut self, id: CollectionId, cx: &mut Context<Self>) {
        if !self.collapsed_collections.remove(&id) {
            self.collapsed_collections.insert(id);
        }
        cx.notify();
    }

    fn select_next(&mut self, _: &menu::SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        let entities = self.navigable_entities(cx);
        if entities.is_empty() {
            return;
        }
        let next = match self
            .selected_entity
            .and_then(|current| entities.iter().position(|e| *e == current))
        {
            Some(index) => entities.get(index + 1).copied().unwrap_or(entities[index]),
            None => entities[0],
        };
        self.selected_entity = Some(next);
        cx.notify();
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entities = self.navigable_entities(cx);
        if entities.is_empty() {
            return;
        }
        let previous = match self
            .selected_entity
            .and_then(|current| entities.iter().position(|e| *e == current))
        {
            Some(0) => entities[0],
            Some(index) => entities[index - 1],
            None => *entities.last().expect("checked non-empty above"),
        };
        self.selected_entity = Some(previous);
        cx.notify();
    }

    fn select_first(
        &mut self,
        _: &menu::SelectFirst,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entities = self.navigable_entities(cx);
        self.selected_entity = entities.into_iter().next();
        cx.notify();
    }

    fn select_last(&mut self, _: &menu::SelectLast, _window: &mut Window, cx: &mut Context<Self>) {
        let entities = self.navigable_entities(cx);
        self.selected_entity = entities.into_iter().next_back();
        cx.notify();
    }

    fn confirm_selected(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        match self.selected_entity {
            Some(SelectedEntity::Folder(id)) => self.toggle_folder_collapsed(id, cx),
            Some(SelectedEntity::Collection(id)) => self.toggle_collection_collapsed(id, cx),
            Some(SelectedEntity::Request(id)) => self.open_request(id, window, cx),
            None => {}
        }
    }

    fn open_request(&mut self, request_id: RequestId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(request) = self
            .store
            .read(cx)
            .requests
            .iter()
            .find(|r| r.id == request_id)
            .cloned()
        else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let store = self.store.clone();
        let workspace_handle = self.workspace.clone();
        workspace.update(cx, |workspace, cx| {
            let view = cx.new(|cx| {
                crate::request_view::RequestView::new(&request, store, workspace_handle, window, cx)
            });
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        });
    }

    fn open_new_grpc_call(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            let view = cx.new(|cx| crate::grpc_view::GrpcView::new(window, cx));
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        });
    }

    fn open_collection_runner(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let store = self.store.clone();
        workspace.update(cx, |workspace, cx| {
            let view = cx.new(|cx| crate::runner_view::RunnerView::new(store, window, cx));
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        });
    }

    fn collapse_selected(&mut self, cx: &mut Context<Self>) {
        match self.selected_entity {
            Some(SelectedEntity::Folder(id)) if !self.collapsed_folders.contains(&id) => {
                self.toggle_folder_collapsed(id, cx);
            }
            Some(SelectedEntity::Collection(id)) if !self.collapsed_collections.contains(&id) => {
                self.toggle_collection_collapsed(id, cx);
            }
            _ => {}
        }
    }

    fn expand_selected(&mut self, cx: &mut Context<Self>) {
        match self.selected_entity {
            Some(SelectedEntity::Folder(id)) if self.collapsed_folders.contains(&id) => {
                self.toggle_folder_collapsed(id, cx);
            }
            Some(SelectedEntity::Collection(id)) if self.collapsed_collections.contains(&id) => {
                self.toggle_collection_collapsed(id, cx);
            }
            _ => {}
        }
    }

    /// Shift+Up/Down reorders the selected folder or request among its
    /// siblings, driving the same store methods as the "Move Up"/"Move Down"
    /// context-menu entries. Collections are a flat top-level list in Phase 1
    /// and are not reordered here.
    fn move_selected(&mut self, direction: i64, cx: &mut Context<Self>) {
        match self.selected_entity {
            Some(SelectedEntity::Folder(id)) => {
                self.store
                    .update(cx, |store, cx| store.reorder_folder(id, direction, cx));
            }
            Some(SelectedEntity::Request(id)) => {
                self.store
                    .update(cx, |store, cx| store.reorder_request(id, direction, cx));
            }
            Some(SelectedEntity::Collection(_)) | None => {}
        }
    }

    fn start_new_collection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                TextPromptModal::new(
                    "New Collection",
                    "Create",
                    "Collection name",
                    "",
                    Arc::new(move |name, _window, cx| {
                        store.update(cx, |store, cx| {
                            store.create_collection(name, cx);
                        });
                    }),
                    window,
                    cx,
                )
            });
        });
    }

    fn start_import_curl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                TextPromptModal::new_multiline(
                    "Import cURL Command",
                    "Import",
                    "Paste a curl command...",
                    Arc::new(move |command, _window, cx| {
                        store.update(cx, |store, cx| {
                            let collection = Collection::new("Imported from cURL".to_string());
                            let collection_id = collection.id;
                            match crate::import::parse_curl(&command, collection_id) {
                                Ok(request) => store.import_collection(
                                    collection,
                                    Vec::new(),
                                    vec![request],
                                    cx,
                                ),
                                Err(error) => log::error!("failed to import curl command: {error}"),
                            }
                        });
                    }),
                    window,
                    cx,
                )
            });
        });
    }

    fn start_import_postman(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                TextPromptModal::new_multiline(
                    "Import Postman Collection",
                    "Import",
                    "Paste a Postman Collection v2.1 JSON document...",
                    Arc::new(move |json, _window, cx| {
                        store.update(
                            cx,
                            |store, cx| match crate::import::parse_postman_collection(&json) {
                                Ok(imported) => store.import_collection(
                                    imported.collection,
                                    imported.folders,
                                    imported.requests,
                                    cx,
                                ),
                                Err(error) => {
                                    log::error!("failed to import Postman collection: {error}")
                                }
                            },
                        );
                    }),
                    window,
                    cx,
                )
            });
        });
    }

    fn start_import_postman_environment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                TextPromptModal::new_multiline(
                    "Import Postman Environment",
                    "Import",
                    "Paste a Postman environment export JSON document...",
                    Arc::new(move |json, _window, cx| {
                        store.update(
                            cx,
                            |store, cx| match crate::import::parse_postman_environment(&json) {
                                Ok(environment) => {
                                    let id = store.create_environment(environment.name.clone(), cx);
                                    store.update_environment(Some(id), cx, |stored| {
                                        stored.variables = environment.variables;
                                    });
                                }
                                Err(error) => {
                                    log::error!("failed to import Postman environment: {error}")
                                }
                            },
                        );
                    }),
                    window,
                    cx,
                )
            });
        });
    }

    fn start_import_openapi(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                TextPromptModal::new_multiline(
                    "Import OpenAPI/Swagger Document",
                    "Import",
                    "Paste an OpenAPI 3.x or Swagger 2.0 JSON document...",
                    Arc::new(move |json, _window, cx| {
                        store.update(cx, |store, cx| match crate::import::parse_openapi_document(
                            &json,
                        ) {
                            Ok(imported) => store.import_collection(
                                imported.collection,
                                imported.folders,
                                imported.requests,
                                cx,
                            ),
                            Err(error) => {
                                log::error!("failed to import OpenAPI/Swagger document: {error}")
                            }
                        });
                    }),
                    window,
                    cx,
                )
            });
        });
    }

    /// Imports every collection and environment from a Postman "Full Data
    /// Export" ZIP file, picked via a native file dialog. Files that fail to
    /// parse are logged individually rather than aborting the whole import
    /// -- matches `crate::full_export::import_full_export`'s per-file error
    /// collection.
    fn start_import_full_export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let path_rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });

        cx.spawn_in(window, async move |_this, cx| {
            let Some(path) = path_rx
                .await
                .log_err()
                .and_then(|result| result.log_err())
                .flatten()
                .and_then(|paths| paths.into_iter().next())
            else {
                return;
            };
            let bytes = cx
                .background_executor()
                .spawn(async move { std::fs::read(&path) })
                .await;
            let Some(bytes) = bytes.log_err() else {
                return;
            };
            let imported = cx
                .background_executor()
                .spawn(async move { crate::full_export::import_full_export(&bytes) })
                .await;
            let imported = match imported {
                Ok(imported) => imported,
                Err(error) => {
                    log::error!("failed to import Postman full data export: {error}");
                    return;
                }
            };
            for failed in &imported.failed {
                log::error!(
                    "failed to import \"{}\" from the Postman full data export: {}",
                    failed.file_name,
                    failed.error
                );
            }
            store.update(cx, |store, cx| {
                for collection in imported.collections {
                    store.import_collection(
                        collection.collection,
                        collection.folders,
                        collection.requests,
                        cx,
                    );
                }
                for environment in imported.environments {
                    let id = store.create_environment(environment.name.clone(), cx);
                    store.update_environment(Some(id), cx, |stored| {
                        stored.variables = environment.variables;
                    });
                }
            });
        })
        .detach();
    }

    /// Exports every collection and environment as a Postman "Full Data
    /// Export" ZIP file -- the same multi-file layout Postman's own "Export
    /// Data" produces, so it round-trips through `start_import_full_export`.
    fn start_export_full_export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let collections = store.collections.clone();
        let folders = store.folders.clone();
        let requests = store.requests.clone();
        let environments = store.environments.clone();

        let path_rx =
            cx.prompt_for_new_path(paths::home_dir(), Some("postman-full-data-export.zip"));
        cx.spawn_in(window, async move |_this, cx| {
            let Some(path) = path_rx
                .await
                .log_err()
                .and_then(|result| result.log_err())
                .flatten()
            else {
                return;
            };
            cx.background_executor()
                .spawn(async move {
                    let scoped_folders: Vec<Vec<Folder>> = collections
                        .iter()
                        .map(|collection| {
                            folders
                                .iter()
                                .filter(|folder| folder.collection_id == collection.id)
                                .cloned()
                                .collect()
                        })
                        .collect();
                    let scoped_requests: Vec<Vec<Request>> = collections
                        .iter()
                        .map(|collection| {
                            requests
                                .iter()
                                .filter(|request| request.collection_id == collection.id)
                                .cloned()
                                .collect()
                        })
                        .collect();
                    let exports: Vec<crate::full_export::CollectionExport> = collections
                        .iter()
                        .zip(scoped_folders.iter())
                        .zip(scoped_requests.iter())
                        .map(|((collection, folders), requests)| {
                            crate::full_export::CollectionExport {
                                collection,
                                folders,
                                requests,
                            }
                        })
                        .collect();
                    match crate::full_export::export_full_export(&exports, &environments) {
                        Ok(bytes) => {
                            std::fs::write(&path, bytes).log_err();
                        }
                        Err(error) => {
                            log::error!("failed to export Postman full data export: {error}")
                        }
                    }
                })
                .await;
        })
        .detach();
    }

    fn start_new_folder(
        &mut self,
        collection_id: CollectionId,
        parent_id: Option<FolderId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                TextPromptModal::new(
                    "New Folder",
                    "Create",
                    "Folder name",
                    "",
                    Arc::new(move |name, _window, cx| {
                        store.update(cx, |store, cx| {
                            store.create_folder(collection_id, name, parent_id, cx);
                        });
                    }),
                    window,
                    cx,
                )
            });
        });
    }

    fn start_new_request(
        &mut self,
        collection_id: CollectionId,
        folder_id: Option<FolderId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                TextPromptModal::new(
                    "New Request",
                    "Create",
                    "Request name",
                    "",
                    Arc::new(move |name, _window, cx| {
                        store.update(cx, |store, cx| {
                            store.create_request(collection_id, name, folder_id, cx);
                        });
                    }),
                    window,
                    cx,
                )
            });
        });
    }

    fn start_rename_collection(
        &mut self,
        id: CollectionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.store.read(cx).collections.iter().find(|c| c.id == id) else {
            return;
        };
        let current_name = current.name.clone();
        let store = self.store.clone();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                TextPromptModal::new(
                    "Rename Collection",
                    "Rename",
                    "Collection name",
                    &current_name,
                    Arc::new(move |name, _window, cx| {
                        store.update(cx, |store, cx| store.rename_collection(id, name, cx));
                    }),
                    window,
                    cx,
                )
            });
        });
    }

    fn start_rename_folder(&mut self, id: FolderId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(current) = self.store.read(cx).folders.iter().find(|f| f.id == id) else {
            return;
        };
        let current_name = current.name.clone();
        let store = self.store.clone();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                TextPromptModal::new(
                    "Rename Folder",
                    "Rename",
                    "Folder name",
                    &current_name,
                    Arc::new(move |name, _window, cx| {
                        store.update(cx, |store, cx| store.rename_folder(id, name, cx));
                    }),
                    window,
                    cx,
                )
            });
        });
    }

    fn start_edit_collection_variables(
        &mut self,
        collection_id: CollectionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                crate::environment_editor::EnvironmentEditorModal::new_for_collection(
                    store,
                    collection_id,
                    window,
                    cx,
                )
            });
        });
    }

    /// Writes `collection_id` and every folder/request under it (with
    /// examples embedded) to a Postman Collection v2.1 JSON file the user
    /// picks via a real save dialog -- the write itself runs off the main
    /// thread, mirroring `db_client_ui::panel::export_database_explorer`'s
    /// `prompt_for_new_path` + `background_spawn` + `std::fs::write` shape.
    fn export_collection_as_postman(
        &mut self,
        collection_id: CollectionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.read(cx);
        let Some(collection) = store
            .collections
            .iter()
            .find(|collection| collection.id == collection_id)
            .cloned()
        else {
            return;
        };
        let folders: Vec<Folder> = store
            .folders
            .iter()
            .filter(|folder| folder.collection_id == collection_id)
            .cloned()
            .collect();
        let requests: Vec<Request> = store
            .requests
            .iter()
            .filter(|request| request.collection_id == collection_id)
            .cloned()
            .collect();

        let default_name = format!("{}.postman_collection.json", collection.name);
        let path_rx = cx.prompt_for_new_path(paths::home_dir(), Some(&default_name));
        cx.spawn_in(window, async move |_this, cx| {
            let Some(path) = path_rx
                .await
                .log_err()
                .and_then(|result| result.log_err())
                .flatten()
            else {
                return;
            };
            cx.background_executor()
                .spawn(async move {
                    let json =
                        crate::export::export_postman_collection(&collection, &folders, &requests);
                    std::fs::write(&path, json).log_err();
                })
                .await;
        })
        .detach();
    }

    /// Writes the currently active environment to a Postman environment
    /// export JSON file. A no-op when no environment is active -- there is
    /// nothing to export and no destructive action to guard against.
    fn export_active_environment_as_postman(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(environment) = self.store.read(cx).active_environment().cloned() else {
            return;
        };
        let default_name = format!("{}.postman_environment.json", environment.name);
        let path_rx = cx.prompt_for_new_path(paths::home_dir(), Some(&default_name));
        cx.spawn_in(window, async move |_this, cx| {
            let Some(path) = path_rx
                .await
                .log_err()
                .and_then(|result| result.log_err())
                .flatten()
            else {
                return;
            };
            cx.background_executor()
                .spawn(async move {
                    let json = crate::export::export_postman_environment(&environment);
                    std::fs::write(&path, json).log_err();
                })
                .await;
        })
        .detach();
    }

    fn start_manage_environments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                crate::environment_editor::EnvironmentEditorModal::new_for_environments(
                    store, window, cx,
                )
            });
        });
    }

    fn delete_collection(&mut self, id: CollectionId, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            store.delete_collection(id, cx);
        });
    }

    fn delete_folder(&mut self, id: FolderId, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            store.delete_folder(id, cx);
        });
    }

    fn delete_request(&mut self, id: RequestId, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            store.delete_request(id, cx);
        });
    }

    fn show_context_menu(
        &mut self,
        menu: Entity<ContextMenu>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&menu, |this, _, _: &gpui::DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((menu, position, subscription));
        cx.notify();
    }

    fn collection_context_menu(
        &self,
        panel: Entity<Self>,
        collection_id: CollectionId,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<ContextMenu> {
        ContextMenu::build(window, cx, move |menu, _, _| {
            menu.entry("New Folder", None, {
                let panel = panel.clone();
                move |window, cx| {
                    panel.update(cx, |panel, cx| {
                        panel.start_new_folder(collection_id, None, window, cx)
                    });
                }
            })
            .entry("New Request", None, {
                let panel = panel.clone();
                move |window, cx| {
                    panel.update(cx, |panel, cx| {
                        panel.start_new_request(collection_id, None, window, cx)
                    });
                }
            })
            .entry("Edit Variables...", None, {
                let panel = panel.clone();
                move |window, cx| {
                    panel.update(cx, |panel, cx| {
                        panel.start_edit_collection_variables(collection_id, window, cx)
                    });
                }
            })
            .entry("Export as Postman Collection...", None, {
                let panel = panel.clone();
                move |window, cx| {
                    panel.update(cx, |panel, cx| {
                        panel.export_collection_as_postman(collection_id, window, cx)
                    });
                }
            })
            .separator()
            .entry("Rename", None, {
                let panel = panel.clone();
                move |window, cx| {
                    panel.update(cx, |panel, cx| {
                        panel.start_rename_collection(collection_id, window, cx)
                    });
                }
            })
            .entry("Delete", None, {
                let panel = panel.clone();
                move |_window, cx| {
                    panel.update(cx, |panel, cx| panel.delete_collection(collection_id, cx));
                }
            })
        })
    }

    fn folder_context_menu(
        &self,
        panel: Entity<Self>,
        folder_id: FolderId,
        collection_id: CollectionId,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<ContextMenu> {
        let (can_move_up, can_move_down) = self.folder_move_bounds(folder_id, cx);
        ContextMenu::build(window, cx, move |menu, _, _| {
            let menu = menu
                .entry("New Folder", None, {
                    let panel = panel.clone();
                    move |window, cx| {
                        panel.update(cx, |panel, cx| {
                            panel.start_new_folder(collection_id, Some(folder_id), window, cx)
                        });
                    }
                })
                .entry("New Request", None, {
                    let panel = panel.clone();
                    move |window, cx| {
                        panel.update(cx, |panel, cx| {
                            panel.start_new_request(collection_id, Some(folder_id), window, cx)
                        });
                    }
                })
                .separator();
            let menu = if can_move_up {
                menu.entry("Move Up", None, {
                    let panel = panel.clone();
                    move |_window, cx| {
                        panel.update(cx, |panel, cx| {
                            panel
                                .store
                                .update(cx, |store, cx| store.reorder_folder(folder_id, -1, cx));
                        });
                    }
                })
            } else {
                menu
            };
            let menu = if can_move_down {
                menu.entry("Move Down", None, {
                    let panel = panel.clone();
                    move |_window, cx| {
                        panel.update(cx, |panel, cx| {
                            panel
                                .store
                                .update(cx, |store, cx| store.reorder_folder(folder_id, 1, cx));
                        });
                    }
                })
            } else {
                menu
            };
            menu.separator()
                .entry("Rename", None, {
                    let panel = panel.clone();
                    move |window, cx| {
                        panel.update(cx, |panel, cx| {
                            panel.start_rename_folder(folder_id, window, cx)
                        });
                    }
                })
                .entry("Delete", None, {
                    let panel = panel.clone();
                    move |_window, cx| {
                        panel.update(cx, |panel, cx| panel.delete_folder(folder_id, cx));
                    }
                })
        })
    }

    fn request_context_menu(
        &self,
        panel: Entity<Self>,
        request_id: RequestId,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<ContextMenu> {
        let (can_move_up, can_move_down) = self.request_move_bounds(request_id, cx);
        ContextMenu::build(window, cx, move |menu, _, _| {
            let menu = if can_move_up {
                menu.entry("Move Up", None, {
                    let panel = panel.clone();
                    move |_window, cx| {
                        panel.update(cx, |panel, cx| {
                            panel
                                .store
                                .update(cx, |store, cx| store.reorder_request(request_id, -1, cx));
                        });
                    }
                })
            } else {
                menu
            };
            let menu = if can_move_down {
                menu.entry("Move Down", None, {
                    let panel = panel.clone();
                    move |_window, cx| {
                        panel.update(cx, |panel, cx| {
                            panel
                                .store
                                .update(cx, |store, cx| store.reorder_request(request_id, 1, cx));
                        });
                    }
                })
            } else {
                menu
            };
            menu.separator()
                .entry("Copy as cURL", None, {
                    let panel = panel.clone();
                    move |_window, cx| {
                        panel.update(cx, |panel, cx| panel.copy_request_as_curl(request_id, cx));
                    }
                })
                .separator()
                .entry("Delete", None, {
                    let panel = panel.clone();
                    move |_window, cx| {
                        panel.update(cx, |panel, cx| panel.delete_request(request_id, cx));
                    }
                })
        })
    }

    fn open_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let store = self.store.clone();
        let workspace_handle = self.workspace.clone();
        workspace.update(cx, |workspace, cx| {
            let view =
                cx.new(|cx| crate::history_view::HistoryView::new(store, workspace_handle, cx));
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        });
    }

    fn copy_request_as_curl(&self, request_id: RequestId, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let Some(request) = store.requests.iter().find(|r| r.id == request_id) else {
            return;
        };
        let context = store.variable_context_for(request);
        let curl = crate::code_generator::generate_curl(request, &context);
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(curl));
    }

    /// Whether `folder_id` can move up/down among its sibling folders, mirroring
    /// `db_client_ui::panel::DatabasePanel::folder_move_bounds` so a menu
    /// entry's visibility always matches whether the move would actually happen.
    fn folder_move_bounds(&self, folder_id: FolderId, cx: &App) -> (bool, bool) {
        let store = self.store.read(cx);
        let Some(folder) = store.folders.iter().find(|f| f.id == folder_id) else {
            return (false, false);
        };
        let mut siblings: Vec<(FolderId, i64)> = store
            .folders
            .iter()
            .filter(|f| f.collection_id == folder.collection_id && f.parent_id == folder.parent_id)
            .map(|f| (f.id, f.order))
            .collect();
        siblings.sort_by_key(|(_, order)| *order);
        let Some(position) = siblings.iter().position(|(id, _)| *id == folder_id) else {
            return (false, false);
        };
        (position > 0, position + 1 < siblings.len())
    }

    fn request_move_bounds(&self, request_id: RequestId, cx: &App) -> (bool, bool) {
        let store = self.store.read(cx);
        let Some(request) = store.requests.iter().find(|r| r.id == request_id) else {
            return (false, false);
        };
        let mut siblings: Vec<(RequestId, i64)> = store
            .requests
            .iter()
            .filter(|r| {
                r.collection_id == request.collection_id && r.folder_id == request.folder_id
            })
            .map(|r| (r.id, r.order))
            .collect();
        siblings.sort_by_key(|(_, order)| *order);
        let Some(position) = siblings.iter().position(|(id, _)| *id == request_id) else {
            return (false, false);
        };
        (position > 0, position + 1 < siblings.len())
    }

    fn render_environment_switcher(&self, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let active_name = store
            .active_environment()
            .map(|environment| environment.name.clone())
            .unwrap_or_else(|| "No Environment".to_string());
        let environments: Vec<(EnvironmentId, String)> = store
            .environments
            .iter()
            .map(|environment| (environment.id, environment.name.clone()))
            .collect();
        let panel = cx.entity();

        div()
            .id("api-client-env-switcher")
            .child(
                ui::PopoverMenu::new("api-client-env-popover")
                    .trigger(
                        Button::new("api-client-env-trigger", active_name)
                            .start_icon(Icon::new(IconName::Settings))
                            .style(ButtonStyle::Subtle),
                    )
                    .menu(move |window, cx| {
                        let panel = panel.clone();
                        let environments = environments.clone();
                        Some(ContextMenu::build(window, cx, move |menu, _, _| {
                            let menu = menu.entry("No Environment", None, {
                                let panel = panel.clone();
                                move |_window, cx| {
                                    panel.update(cx, |panel, cx| {
                                        panel.store.update(cx, |store, cx| {
                                            store.set_active_environment(None, cx)
                                        });
                                    });
                                }
                            });
                            let menu = environments.iter().fold(menu, |menu, (id, name)| {
                                let panel = panel.clone();
                                let id = *id;
                                menu.entry(name.clone(), None, move |_window, cx| {
                                    panel.update(cx, |panel, cx| {
                                        panel.store.update(cx, |store, cx| {
                                            store.set_active_environment(Some(id), cx)
                                        });
                                    });
                                })
                            });
                            menu.separator()
                                .entry("Export Active Environment as Postman...", None, {
                                    let panel = panel.clone();
                                    move |window, cx| {
                                        panel.update(cx, |panel, cx| {
                                            panel.export_active_environment_as_postman(window, cx)
                                        });
                                    }
                                })
                                .entry("Manage Environments...", None, move |window, cx| {
                                    panel.update(cx, |panel, cx| {
                                        panel.start_manage_environments(window, cx)
                                    });
                                })
                        }))
                    }),
            )
            .into_any_element()
    }

    fn render_tree_nodes(
        &self,
        nodes: Vec<TreeNode>,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut elements = Vec::new();
        for node in nodes {
            match node {
                TreeNode::Collection {
                    collection,
                    children,
                } => {
                    let collection_id = collection.id;
                    let is_collapsed = self.collapsed_collections.contains(&collection_id);
                    let is_selected =
                        self.selected_entity == Some(SelectedEntity::Collection(collection_id));
                    let panel = cx.entity();
                    let row = h_flex()
                        .id(ElementId::from(SharedString::from(format!(
                            "api-client-collection-row-{collection_id}"
                        ))))
                        .w_full()
                        .pl(px(8. + depth as f32 * 16.))
                        .py_1()
                        .gap_1()
                        .when(is_selected, |row| {
                            row.bg(cx.theme().colors().element_selected)
                        })
                        .hover(|row| row.bg(cx.theme().colors().element_hover))
                        .child(
                            Icon::new(if is_collapsed {
                                IconName::ChevronRight
                            } else {
                                IconName::ChevronDown
                            })
                            .size(IconSize::XSmall),
                        )
                        .child(Icon::new(IconName::FileTree).size(IconSize::XSmall))
                        .child(Label::new(collection.name.clone()).size(LabelSize::Small))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.selected_entity = Some(SelectedEntity::Collection(collection_id));
                            this.toggle_collection_collapsed(collection_id, cx);
                        }));
                    let menu = right_click_menu(ElementId::from(SharedString::from(format!(
                        "api-client-collection-menu-{collection_id}"
                    ))))
                    .trigger(move |_is_open, _window, _cx| row)
                    .menu(move |window, cx| {
                        let panel_handle = panel.clone();
                        panel.update(cx, |panel, cx| {
                            panel.collection_context_menu(panel_handle, collection_id, window, cx)
                        })
                    });
                    elements.push(menu.into_any_element());
                    if !is_collapsed {
                        elements.extend(self.render_tree_nodes(children, depth + 1, cx));
                    }
                }
                TreeNode::Folder { folder, children } => {
                    let folder_id = folder.id;
                    let collection_id = folder.collection_id;
                    let is_collapsed = self.collapsed_folders.contains(&folder_id);
                    let is_selected =
                        self.selected_entity == Some(SelectedEntity::Folder(folder_id));
                    let panel = cx.entity();
                    let row = h_flex()
                        .id(ElementId::from(SharedString::from(format!(
                            "api-client-folder-row-{folder_id}"
                        ))))
                        .w_full()
                        .pl(px(8. + depth as f32 * 16.))
                        .py_1()
                        .gap_1()
                        .when(is_selected, |row| {
                            row.bg(cx.theme().colors().element_selected)
                        })
                        .hover(|row| row.bg(cx.theme().colors().element_hover))
                        .child(
                            Icon::new(if is_collapsed {
                                IconName::ChevronRight
                            } else {
                                IconName::ChevronDown
                            })
                            .size(IconSize::XSmall),
                        )
                        .child(Icon::new(IconName::Folder).size(IconSize::XSmall))
                        .child(Label::new(folder.name.clone()).size(LabelSize::Small))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.selected_entity = Some(SelectedEntity::Folder(folder_id));
                            this.toggle_folder_collapsed(folder_id, cx);
                        }));
                    let menu = right_click_menu(ElementId::from(SharedString::from(format!(
                        "api-client-folder-menu-{folder_id}"
                    ))))
                    .trigger(move |_is_open, _window, _cx| row)
                    .menu(move |window, cx| {
                        let panel_handle = panel.clone();
                        panel.update(cx, |panel, cx| {
                            panel.folder_context_menu(
                                panel_handle,
                                folder_id,
                                collection_id,
                                window,
                                cx,
                            )
                        })
                    });
                    elements.push(menu.into_any_element());
                    if !is_collapsed {
                        elements.extend(self.render_tree_nodes(children, depth + 1, cx));
                    }
                }
                TreeNode::Request(request) => {
                    let request_id = request.id;
                    let is_selected =
                        self.selected_entity == Some(SelectedEntity::Request(request_id));
                    let panel = cx.entity();
                    let method_label = request.method.as_str().to_string();
                    let method_color = RequestView::method_color(&request.method);
                    let row = h_flex()
                        .id(ElementId::from(SharedString::from(format!(
                            "api-client-request-row-{request_id}"
                        ))))
                        .debug_selector(move || format!("api-client-request-row-{request_id}"))
                        .w_full()
                        .pl(px(24. + depth as f32 * 16.))
                        .py_1()
                        .gap_2()
                        .items_center()
                        .when(is_selected, |row| {
                            row.bg(cx.theme().colors().element_selected)
                        })
                        .hover(|row| row.bg(cx.theme().colors().element_hover))
                        .child(RequestView::render_method_badge(
                            method_label.clone().into(),
                            method_color,
                            cx,
                        ))
                        .child(Label::new(request.name.clone()).size(LabelSize::Small))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.selected_entity = Some(SelectedEntity::Request(request_id));
                            this.open_request(request_id, window, cx);
                        }));
                    let menu = right_click_menu(ElementId::from(SharedString::from(format!(
                        "api-client-request-menu-{request_id}"
                    ))))
                    .trigger(move |_is_open, _window, _cx| row)
                    .menu(move |window, cx| {
                        let panel_handle = panel.clone();
                        panel.update(cx, |panel, cx| {
                            panel.request_context_menu(panel_handle, request_id, window, cx)
                        })
                    });
                    elements.push(menu.into_any_element());
                }
            }
        }
        elements
    }
}

impl Render for ApiClientPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let nodes = build_tree(&store.collections, &store.folders, &store.requests);
        let tree_elements = self.render_tree_nodes(nodes, 0, cx);
        let environment_switcher = self.render_environment_switcher(cx);

        v_flex()
            .key_context("ApiClientPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm_selected))
            .on_action(cx.listener(|this, _: &CollapseSelectedEntry, _window, cx| {
                this.collapse_selected(cx)
            }))
            .on_action(
                cx.listener(|this, _: &ExpandSelectedEntry, _window, cx| this.expand_selected(cx)),
            )
            .on_action(
                cx.listener(|this, _: &MoveSelectedUp, _window, cx| this.move_selected(-1, cx)),
            )
            .on_action(
                cx.listener(|this, _: &MoveSelectedDown, _window, cx| this.move_selected(1, cx)),
            )
            .on_action(cx.listener(|this, _: &NewCollection, window, cx| {
                this.start_new_collection(window, cx)
            }))
            .size_full()
            .relative()
            .overflow_hidden()
            .child(div().absolute().inset_0().on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    let panel = cx.entity();
                    let menu = ContextMenu::build(window, cx, move |menu, _, _| {
                        menu.entry("New Collection", None, {
                            let panel = panel.clone();
                            move |window, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.start_new_collection(window, cx));
                            }
                        })
                        .entry("New gRPC Call", None, {
                            let panel = panel.clone();
                            move |window, cx| {
                                panel.update(cx, |panel, cx| panel.open_new_grpc_call(window, cx));
                            }
                        })
                        .entry("Collection Runner", None, {
                            let panel = panel.clone();
                            move |window, cx| {
                                panel.update(cx, |panel, cx| {
                                    panel.open_collection_runner(window, cx)
                                });
                            }
                        })
                        .separator()
                        .entry("Import cURL Command", None, {
                            let panel = panel.clone();
                            move |window, cx| {
                                panel.update(cx, |panel, cx| panel.start_import_curl(window, cx));
                            }
                        })
                        .entry("Import Postman Collection", None, {
                            let panel = panel.clone();
                            move |window, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.start_import_postman(window, cx));
                            }
                        })
                        .entry("Import Postman Environment", None, {
                            let panel = panel.clone();
                            move |window, cx| {
                                panel.update(cx, |panel, cx| {
                                    panel.start_import_postman_environment(window, cx)
                                });
                            }
                        })
                        .entry("Import OpenAPI/Swagger Document", None, {
                            let panel = panel.clone();
                            move |window, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.start_import_openapi(window, cx));
                            }
                        })
                        .entry("Import Postman Full Data Export (.zip)...", None, {
                            let panel = panel.clone();
                            move |window, cx| {
                                panel.update(cx, |panel, cx| {
                                    panel.start_import_full_export(window, cx)
                                });
                            }
                        })
                        .entry(
                            "Export Postman Full Data Export (.zip)...",
                            None,
                            {
                                move |window, cx| {
                                    panel.update(cx, |panel, cx| {
                                        panel.start_export_full_export(window, cx)
                                    });
                                }
                            },
                        )
                    });
                    this.show_context_menu(menu, event.position, window, cx);
                }),
            ))
            .child(
                h_flex()
                    .flex_none()
                    .justify_between()
                    .items_center()
                    .p_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Label::new("API Client")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                IconButton::new("api-client-open-history", IconName::HistoryRerun)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("History"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_history(window, cx)
                                    })),
                            )
                            .child(environment_switcher),
                    ),
            )
            .child(
                div()
                    .id("api-client-tree")
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .track_scroll(&self.tree_scroll_handle)
                    .children(tree_elements)
                    .custom_scrollbars(
                        Scrollbars::always_visible(ScrollAxes::Both)
                            .tracked_scroll_handle(&self.tree_scroll_handle),
                        window,
                        cx,
                    ),
            )
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                gpui::deferred(
                    gpui::anchored()
                        .position(*position)
                        .child(gpui::div().occlude().child(menu.clone())),
                )
                .with_priority(1)
                .into_any_element()
            }))
    }
}

impl Panel for ApiClientPanel {
    fn persistent_name() -> &'static str {
        API_CLIENT_PANEL_KEY
    }

    fn panel_key() -> &'static str {
        API_CLIENT_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, _position: DockPosition) -> bool {
        true
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(260.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<ui::IconName> {
        Some(IconName::Send)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("API Client")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        9
    }
}

pub(crate) fn init(_cx: &mut App) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ApiClientStore;
    use crate::text_prompt_modal::TextPromptModal;
    use gpui::{TestAppContext, VisualTestContext};
    use project::Project;
    use workspace::Workspace;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    async fn build_panel(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, Entity<ApiClientPanel>, VisualTestContext) {
        init_test(cx);
        let fs = project::FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window = cx.add_window(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let panel = window
            .update(&mut cx, |workspace, window, cx| {
                let workspace_handle = workspace.weak_handle();
                cx.spawn_in(window, async move |_, cx| {
                    ApiClientPanel::load(workspace_handle, cx.clone()).await
                })
            })
            .unwrap();
        cx.run_until_parked();
        let panel = panel.await.unwrap();
        let workspace = window.root(&mut cx).unwrap();
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.add_panel(panel.clone(), window, cx);
        });
        (workspace, panel, cx)
    }

    #[gpui::test]
    async fn panel_can_be_created_and_renders_without_panicking(cx: &mut TestAppContext) {
        let (workspace, _panel, mut cx) = build_panel(cx).await;
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(800.), px(600.)),
            |window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.render(window, cx).into_any_element()
                })
            },
        );
    }

    fn debug_center(cx: &mut VisualTestContext, selector: &'static str) -> gpui::Point<Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("expected debug bounds for {selector}"))
            .center()
    }

    #[gpui::test]
    async fn clicking_a_request_row_opens_its_request_view(cx: &mut TestAppContext) {
        let (workspace, panel, mut cx) = build_panel(cx).await;
        let store = panel.read_with(&cx, |panel, _| panel.store.clone());
        let collection_id = store.update(&mut cx, |store, cx| {
            store.create_collection("Sample API".into(), cx)
        });
        let request_id = store.update(&mut cx, |store, cx| {
            store.create_request(collection_id, "Get users".into(), None, cx)
        });
        cx.run_until_parked();

        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.toggle_panel_focus::<ApiClientPanel>(window, cx);
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        let row = debug_center(
            &mut cx,
            format!("api-client-request-row-{request_id}").leak(),
        );
        cx.simulate_click(row, gpui::Modifiers::none());
        cx.run_until_parked();

        workspace.read_with(&cx, |workspace, cx| {
            assert!(
                workspace
                    .active_item_as::<crate::request_view::RequestView>(cx)
                    .is_some(),
                "clicking a request row must open its RequestView in the active pane"
            );
        });
    }

    #[gpui::test]
    fn flatten_navigable_entities_matches_render_order_and_skips_collapsed_children(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let store = cx.new(|cx| ApiClientStore::new(cx));
        let collection_id = store.update(cx, |store, cx| store.create_collection("A".into(), cx));
        let folder_id = store
            .update(cx, |store, cx| {
                store.create_folder(collection_id, "Folder".into(), None, cx)
            })
            .unwrap();
        let request_in_folder = store.update(cx, |store, cx| {
            store.create_request(collection_id, "InFolder".into(), Some(folder_id), cx)
        });
        let top_level_request = store.update(cx, |store, cx| {
            store.create_request(collection_id, "TopLevel".into(), None, cx)
        });

        let (collections, folders, requests) = store.read_with(cx, |store, _| {
            (
                store.collections.clone(),
                store.folders.clone(),
                store.requests.clone(),
            )
        });
        let nodes = build_tree(&collections, &folders, &requests);

        let expanded = flatten_navigable_entities(&nodes, &HashSet::new(), &HashSet::new());
        assert_eq!(
            expanded,
            vec![
                SelectedEntity::Collection(collection_id),
                SelectedEntity::Folder(folder_id),
                SelectedEntity::Request(request_in_folder),
                SelectedEntity::Request(top_level_request),
            ]
        );

        let mut collapsed_folders = HashSet::new();
        collapsed_folders.insert(folder_id);
        let collapsed = flatten_navigable_entities(&nodes, &HashSet::new(), &collapsed_folders);
        assert_eq!(
            collapsed,
            vec![
                SelectedEntity::Collection(collection_id),
                SelectedEntity::Folder(folder_id),
                SelectedEntity::Request(top_level_request),
            ]
        );
    }

    #[gpui::test]
    async fn keyboard_select_next_and_move_selected_drive_the_real_action_dispatch(
        cx: &mut TestAppContext,
    ) {
        let (_workspace, panel, mut cx) = build_panel(cx).await;
        let store = panel.read_with(&cx, |panel, _| panel.store.clone());
        let collection_id =
            store.update(&mut cx, |store, cx| store.create_collection("A".into(), cx));
        let first = store.update(&mut cx, |store, cx| {
            store.create_request(collection_id, "First".into(), None, cx)
        });
        let second = store.update(&mut cx, |store, cx| {
            store.create_request(collection_id, "Second".into(), None, cx)
        });
        cx.run_until_parked();

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.focus_handle.focus(window, cx);
            panel.select_next(&menu::SelectNext, window, cx);
        });
        panel.read_with(&cx, |panel, _| {
            assert_eq!(
                panel.selected_entity,
                Some(SelectedEntity::Collection(collection_id))
            );
        });
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.select_next(&menu::SelectNext, window, cx);
        });
        panel.read_with(&cx, |panel, _| {
            assert_eq!(panel.selected_entity, Some(SelectedEntity::Request(first)));
        });

        panel.update_in(&mut cx, |panel, _window, cx| {
            panel.move_selected(1, cx);
        });
        cx.run_until_parked();
        store.read_with(&cx, |store, _| {
            let first_order = store.requests.iter().find(|r| r.id == first).unwrap().order;
            let second_order = store
                .requests
                .iter()
                .find(|r| r.id == second)
                .unwrap()
                .order;
            assert!(
                first_order > second_order,
                "Move Down should push `first` past `second`"
            );
        });
    }

    #[gpui::test]
    async fn importing_a_curl_command_through_the_real_modal_creates_a_request_in_a_new_collection(
        cx: &mut TestAppContext,
    ) {
        let (workspace, panel, mut cx) = build_panel(cx).await;
        let store = panel.read_with(&cx, |panel, _| panel.store.clone());

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.start_import_curl(window, cx)
        });
        cx.run_until_parked();

        let modal = workspace
            .read_with(&cx, |workspace, cx| {
                workspace.active_modal::<TextPromptModal>(cx)
            })
            .expect("the import modal should be open");
        modal.update_in(&mut cx, |modal, window, cx| {
            modal.editor.update(cx, |editor, cx| {
                editor.set_text(
                    "curl https://api.example.com/ping -H \"Accept: application/json\"",
                    window,
                    cx,
                );
            });
        });
        modal.update_in(&mut cx, |modal, window, cx| modal.confirm(window, cx));
        cx.run_until_parked();

        store.read_with(&cx, |store, _| {
            assert_eq!(store.collections.len(), 1);
            assert_eq!(store.collections[0].name, "Imported from cURL");
            assert_eq!(store.requests.len(), 1);
            assert_eq!(store.requests[0].url, "https://api.example.com/ping");
            assert_eq!(store.requests[0].headers.len(), 1);
        });
    }

    #[gpui::test]
    async fn importing_a_postman_collection_through_the_real_modal_creates_the_full_tree(
        cx: &mut TestAppContext,
    ) {
        let (workspace, panel, mut cx) = build_panel(cx).await;
        let store = panel.read_with(&cx, |panel, _| panel.store.clone());

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.start_import_postman(window, cx)
        });
        cx.run_until_parked();

        let modal = workspace
            .read_with(&cx, |workspace, cx| {
                workspace.active_modal::<TextPromptModal>(cx)
            })
            .expect("the import modal should be open");
        let postman_json = r#"{
            "info": { "name": "Sample API" },
            "item": [
                {
                    "name": "Get users",
                    "request": { "method": "GET", "url": "https://api.example.com/users", "header": [] }
                }
            ]
        }"#;
        modal.update_in(&mut cx, |modal, window, cx| {
            modal.editor.update(cx, |editor, cx| {
                editor.set_text(postman_json, window, cx);
            });
        });
        modal.update_in(&mut cx, |modal, window, cx| modal.confirm(window, cx));
        cx.run_until_parked();

        store.read_with(&cx, |store, _| {
            assert_eq!(store.collections.len(), 1);
            assert_eq!(store.collections[0].name, "Sample API");
            assert_eq!(store.requests.len(), 1);
            assert_eq!(store.requests[0].name, "Get users");
        });
    }

    #[gpui::test]
    async fn importing_a_full_data_export_zip_through_the_real_file_dialog_creates_every_collection_and_environment(
        cx: &mut TestAppContext,
    ) {
        let (_workspace, panel, mut cx) = build_panel(cx).await;
        let store = panel.read_with(&cx, |panel, _| panel.store.clone());

        let collection = api_client::Collection::new("API A".to_string());
        let mut request = Request::new(collection.id, "Get users".to_string());
        request.url = "https://api.example.com/users".to_string();
        let environment = api_client::Environment::new("Staging".to_string());
        let zip_bytes = crate::full_export::export_full_export(
            &[crate::full_export::CollectionExport {
                collection: &collection,
                folders: &[],
                requests: &[request],
            }],
            &[environment],
        )
        .unwrap();

        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("full-export.zip");
        std::fs::write(&zip_path, &zip_bytes).unwrap();

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.start_import_full_export(window, cx)
        });
        cx.simulate_path_prompt_response(move |_options| Some(vec![zip_path]));
        cx.run_until_parked();

        store.read_with(&cx, |store, _| {
            assert_eq!(store.collections.len(), 1);
            assert_eq!(store.collections[0].name, "API A");
            assert_eq!(store.requests.len(), 1);
            assert_eq!(store.requests[0].url, "https://api.example.com/users");
            assert_eq!(store.environments.len(), 1);
            assert_eq!(store.environments[0].name, "Staging");
        });
    }

    #[gpui::test]
    async fn exporting_a_full_data_export_zip_through_the_real_file_dialog_writes_every_collection_and_environment(
        cx: &mut TestAppContext,
    ) {
        let (_workspace, panel, mut cx) = build_panel(cx).await;
        let store = panel.read_with(&cx, |panel, _| panel.store.clone());
        let collection_id = store.update(&mut cx, |store, cx| {
            store.create_collection("API A".into(), cx)
        });
        store.update(&mut cx, |store, cx| {
            store.create_request(collection_id, "Get users".into(), None, cx)
        });

        let temp_dir = tempfile::tempdir().unwrap();
        let zip_path = temp_dir.path().join("full-export.zip");

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.start_export_full_export(window, cx)
        });
        cx.simulate_new_path_selection({
            let zip_path = zip_path.clone();
            move |_directory| Some(zip_path)
        });
        cx.run_until_parked();

        let bytes = std::fs::read(&zip_path).expect("the export file should have been written");
        let imported = crate::full_export::import_full_export(&bytes).unwrap();
        assert!(imported.failed.is_empty());
        assert_eq!(imported.collections.len(), 1);
        assert_eq!(imported.collections[0].collection.name, "API A");
        assert_eq!(imported.collections[0].requests.len(), 1);
    }

    #[gpui::test]
    async fn importing_an_openapi_document_through_the_real_modal_creates_a_request_per_operation(
        cx: &mut TestAppContext,
    ) {
        let (workspace, panel, mut cx) = build_panel(cx).await;
        let store = panel.read_with(&cx, |panel, _| panel.store.clone());

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.start_import_openapi(window, cx)
        });
        cx.run_until_parked();

        let modal = workspace
            .read_with(&cx, |workspace, cx| {
                workspace.active_modal::<TextPromptModal>(cx)
            })
            .expect("the import modal should be open");
        let openapi_json = r#"{
            "openapi": "3.0.0",
            "info": { "title": "Sample API" },
            "servers": [{ "url": "https://api.example.com" }],
            "paths": {
                "/ping": { "get": { "summary": "Ping" } }
            }
        }"#;
        modal.update_in(&mut cx, |modal, window, cx| {
            modal.editor.update(cx, |editor, cx| {
                editor.set_text(openapi_json, window, cx);
            });
        });
        modal.update_in(&mut cx, |modal, window, cx| modal.confirm(window, cx));
        cx.run_until_parked();

        store.read_with(&cx, |store, _| {
            assert_eq!(store.collections.len(), 1);
            assert_eq!(store.collections[0].name, "Sample API");
            assert_eq!(store.requests.len(), 1);
            assert_eq!(store.requests[0].name, "Ping");
            assert_eq!(store.requests[0].url, "https://api.example.com/ping");
        });
    }

    #[gpui::test]
    async fn importing_a_postman_environment_through_the_real_modal_creates_it_with_its_variables(
        cx: &mut TestAppContext,
    ) {
        let (workspace, panel, mut cx) = build_panel(cx).await;
        let store = panel.read_with(&cx, |panel, _| panel.store.clone());

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.start_import_postman_environment(window, cx)
        });
        cx.run_until_parked();

        let modal = workspace
            .read_with(&cx, |workspace, cx| {
                workspace.active_modal::<TextPromptModal>(cx)
            })
            .expect("the import modal should be open");
        let environment_json = r#"{
            "name": "Staging",
            "values": [
                { "key": "base_url", "value": "https://staging.example.com", "enabled": true, "type": "default" },
                { "key": "api_key", "value": "shh", "enabled": true, "type": "secret" }
            ]
        }"#;
        modal.update_in(&mut cx, |modal, window, cx| {
            modal.editor.update(cx, |editor, cx| {
                editor.set_text(environment_json, window, cx);
            });
        });
        modal.update_in(&mut cx, |modal, window, cx| modal.confirm(window, cx));
        cx.run_until_parked();

        store.read_with(&cx, |store, _| {
            assert_eq!(store.environments.len(), 1);
            let environment = &store.environments[0];
            assert_eq!(environment.name, "Staging");
            assert_eq!(environment.variables.len(), 2);
            let api_key = environment
                .variables
                .iter()
                .find(|v| v.key == "api_key")
                .unwrap();
            assert!(api_key.secret);
            assert_eq!(api_key.current_value, "shh");
        });
    }

    #[gpui::test]
    async fn exporting_a_collection_as_postman_writes_a_reimportable_file(cx: &mut TestAppContext) {
        let (_workspace, panel, mut cx) = build_panel(cx).await;
        let store = panel.read_with(&cx, |panel, _| panel.store.clone());
        let collection_id = store.update(&mut cx, |store, cx| {
            store.create_collection("Sample API".into(), cx)
        });
        store.update(&mut cx, |store, cx| {
            store.create_request(collection_id, "Get users".into(), None, cx);
        });

        let output_dir = tempfile::tempdir().unwrap();
        let output_path = output_dir.path().join("export.postman_collection.json");
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.export_collection_as_postman(collection_id, window, cx)
        });
        cx.simulate_new_path_selection(|_| Some(output_path.clone()));
        cx.run_until_parked();

        let written = std::fs::read_to_string(&output_path)
            .expect("export_collection_as_postman should have written the file");
        let reimported = crate::import::parse_postman_collection(&written).unwrap();
        assert_eq!(reimported.collection.name, "Sample API");
        assert_eq!(reimported.requests.len(), 1);
        assert_eq!(reimported.requests[0].name, "Get users");
    }

    #[gpui::test]
    async fn exporting_the_active_environment_as_postman_writes_a_reimportable_file(
        cx: &mut TestAppContext,
    ) {
        let (_workspace, panel, mut cx) = build_panel(cx).await;
        let store = panel.read_with(&cx, |panel, _| panel.store.clone());
        let environment_id = store.update(&mut cx, |store, cx| {
            let id = store.create_environment("Staging".into(), cx);
            store.update_environment(Some(id), cx, |environment| {
                environment.variables.push(api_client::Variable::new(
                    "base_url".into(),
                    "https://staging.example.com".into(),
                ));
            });
            id
        });
        store.update(&mut cx, |store, cx| {
            store.set_active_environment(Some(environment_id), cx);
        });

        let output_dir = tempfile::tempdir().unwrap();
        let output_path = output_dir.path().join("export.postman_environment.json");
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.export_active_environment_as_postman(window, cx)
        });
        cx.simulate_new_path_selection(|_| Some(output_path.clone()));
        cx.run_until_parked();

        let written = std::fs::read_to_string(&output_path)
            .expect("export_active_environment_as_postman should have written the file");
        let reimported = crate::import::parse_postman_environment(&written).unwrap();
        assert_eq!(reimported.name, "Staging");
        assert_eq!(reimported.variables.len(), 1);
        assert_eq!(reimported.variables[0].key, "base_url");
    }
}
