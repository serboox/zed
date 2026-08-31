use api_client::{DescriptorPool, GrpcMethodInfo, GrpcServiceInfo, GrpcTlsConfig};
use editor::Editor;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, ScrollHandle, SharedString,
    Subscription, Window,
};
use ui::{
    Icon, IconName, IconSize, Label, LabelSize, ScrollAxes, Scrollbars, WithScrollbar, cyberpunk,
    prelude::*,
};
use util::ResultExt;
use workspace::{Item, item::ItemEvent};

fn new_single_line_editor(
    placeholder: &'static str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        editor
    })
}

struct MetadataRow {
    key_editor: Entity<Editor>,
    value_editor: Entity<Editor>,
    enabled: bool,
}

/// One entry in a streaming call's chronological log -- deliberately not
/// forced into the unary `ResponseData`/`response_view.rs` shape, since
/// Postman itself renders streaming as a timeline rather than a single
/// request/response pane (see the gRPC deep-dive in the work plan).
enum GrpcTimelineEntry {
    Sent(String),
    Received(String),
    Error(String),
}

enum ConnectStatus {
    Idle,
    Connecting,
    Connected { service_count: usize },
    Error(String),
}

enum SendStatus {
    Idle,
    Sending,
    Done,
    Error(String),
}

pub struct GrpcView {
    focus_handle: FocusHandle,
    address_editor: Entity<Editor>,
    tls_enabled: bool,
    ca_certificate_path_editor: Entity<Editor>,
    client_certificate_path_editor: Entity<Editor>,
    client_key_path_editor: Entity<Editor>,
    domain_name_editor: Entity<Editor>,
    connect_status: ConnectStatus,
    pool: Option<DescriptorPool>,
    services: Vec<GrpcServiceInfo>,
    selected_method: Option<GrpcMethodInfo>,
    metadata_rows: Vec<MetadataRow>,
    request_editor: Entity<Editor>,
    response_editor: Entity<Editor>,
    timeline: Vec<GrpcTimelineEntry>,
    send_status: SendStatus,
    services_scroll_handle: ScrollHandle,
    detail_scroll_handle: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl GrpcView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let address_editor = new_single_line_editor("https://localhost:50051", window, cx);
        let ca_certificate_path_editor =
            new_single_line_editor("CA certificate path (optional)", window, cx);
        let client_certificate_path_editor =
            new_single_line_editor("Client certificate path (mTLS, optional)", window, cx);
        let client_key_path_editor =
            new_single_line_editor("Client key path (mTLS, optional)", window, cx);
        let domain_name_editor =
            new_single_line_editor("TLS domain override (optional)", window, cx);
        let request_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_placeholder_text(
                "Select a method, then \"Use Example Message\"",
                window,
                cx,
            );
            editor
        });
        let response_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_read_only(true);
            editor
        });

        Self {
            focus_handle: cx.focus_handle(),
            address_editor,
            tls_enabled: false,
            ca_certificate_path_editor,
            client_certificate_path_editor,
            client_key_path_editor,
            domain_name_editor,
            connect_status: ConnectStatus::Idle,
            pool: None,
            services: Vec::new(),
            selected_method: None,
            metadata_rows: Vec::new(),
            request_editor,
            response_editor,
            timeline: Vec::new(),
            send_status: SendStatus::Idle,
            services_scroll_handle: ScrollHandle::new(),
            detail_scroll_handle: ScrollHandle::new(),
            _subscriptions: Vec::new(),
        }
    }

    fn tls_config(&self, cx: &App) -> GrpcTlsConfig {
        let non_empty = |editor: &Entity<Editor>| {
            let text = editor.read(cx).text(cx);
            (!text.is_empty()).then_some(text)
        };
        GrpcTlsConfig {
            enabled: self.tls_enabled,
            ca_certificate_path: non_empty(&self.ca_certificate_path_editor),
            client_certificate_path: non_empty(&self.client_certificate_path_editor),
            client_key_path: non_empty(&self.client_key_path_editor),
            domain_name: non_empty(&self.domain_name_editor),
        }
    }

    fn toggle_tls(&mut self, cx: &mut Context<Self>) {
        self.tls_enabled = !self.tls_enabled;
        cx.notify();
    }

    fn connect_via_reflection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let address = self.address_editor.read(cx).text(cx);
        if address.is_empty() {
            self.connect_status = ConnectStatus::Error("Enter a server address first.".to_string());
            cx.notify();
            return;
        }
        let tls = self.tls_config(cx);
        self.connect_status = ConnectStatus::Connecting;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let channel = api_client::connect_channel(address, tls).await?;
                api_client::discover_via_reflection(channel).await
            }
            .await;

            this.update(cx, |this, cx| {
                match result {
                    Ok(pool) => {
                        let services = api_client::list_services(&pool);
                        this.connect_status = ConnectStatus::Connected {
                            service_count: services.len(),
                        };
                        this.services = services;
                        this.pool = Some(pool);
                        this.selected_method = None;
                    }
                    Err(error) => this.connect_status = ConnectStatus::Error(error.to_string()),
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn import_proto_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path_rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });

        cx.spawn_in(window, async move |this, cx| {
            let Some(paths) = path_rx
                .await
                .log_err()
                .and_then(|result| result.log_err())
                .flatten()
            else {
                return;
            };
            if paths.is_empty() {
                return;
            }
            let import_paths: Vec<std::path::PathBuf> = paths
                .iter()
                .filter_map(|path| path.parent().map(|parent| parent.to_path_buf()))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();

            let result = cx
                .background_spawn(async move {
                    api_client::descriptor_pool_from_proto_files(&paths, &import_paths)
                })
                .await;

            this.update(cx, |this, cx| {
                match result {
                    Ok(pool) => {
                        let services = api_client::list_services(&pool);
                        this.connect_status = ConnectStatus::Connected {
                            service_count: services.len(),
                        };
                        this.services = services;
                        this.pool = Some(pool);
                        this.selected_method = None;
                    }
                    Err(error) => this.connect_status = ConnectStatus::Error(error.to_string()),
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn select_method(&mut self, method: GrpcMethodInfo, cx: &mut Context<Self>) {
        self.selected_method = Some(method);
        self.timeline.clear();
        self.send_status = SendStatus::Idle;
        cx.notify();
    }

    fn use_example_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pool) = &self.pool else {
            return;
        };
        let Some(method) = &self.selected_method else {
            return;
        };
        if let Some(json) =
            api_client::example_message_json(pool, &method.input_type_name).log_err()
        {
            self.request_editor
                .update(cx, |editor, cx| editor.set_text(json, window, cx));
        }
    }

    fn beautify_request(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.request_editor.read(cx).text(cx);
        let Some(value) = serde_json::from_str::<serde_json::Value>(&text).log_err() else {
            return;
        };
        if let Some(pretty) = serde_json::to_string_pretty(&value).log_err() {
            self.request_editor
                .update(cx, |editor, cx| editor.set_text(pretty, window, cx));
        }
    }

    fn add_metadata_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key_editor = new_single_line_editor("Key", window, cx);
        let value_editor = new_single_line_editor("Value", window, cx);
        self.metadata_rows.push(MetadataRow {
            key_editor,
            value_editor,
            enabled: true,
        });
        cx.notify();
    }

    fn current_metadata(&self, cx: &App) -> Vec<(String, String)> {
        self.metadata_rows
            .iter()
            .filter(|row| row.enabled)
            .map(|row| {
                (
                    row.key_editor.read(cx).text(cx),
                    row.value_editor.read(cx).text(cx),
                )
            })
            .filter(|(key, _)| !key.is_empty())
            .collect()
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(pool), Some(method)) = (self.pool.clone(), self.selected_method.clone()) else {
            return;
        };
        let address = self.address_editor.read(cx).text(cx);
        let tls = self.tls_config(cx);
        let request_json = self.request_editor.read(cx).text(cx);
        let metadata = self.current_metadata(cx);

        self.send_status = SendStatus::Sending;
        self.timeline.clear();
        self.timeline
            .push(GrpcTimelineEntry::Sent(request_json.clone()));
        cx.notify();

        if method.server_streaming {
            self.send_server_streaming(
                address,
                tls,
                pool,
                method,
                request_json,
                metadata,
                window,
                cx,
            );
        } else {
            self.send_unary(
                address,
                tls,
                pool,
                method,
                request_json,
                metadata,
                window,
                cx,
            );
        }
    }

    fn send_unary(
        &mut self,
        address: String,
        tls: GrpcTlsConfig,
        pool: DescriptorPool,
        method: GrpcMethodInfo,
        request_json: String,
        metadata: Vec<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let channel = api_client::connect_channel(address, tls).await?;
                api_client::call_unary(channel, pool, method, request_json, metadata).await
            }
            .await;

            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(json) => {
                        this.response_editor.update(cx, |editor, cx| {
                            editor.set_read_only(false);
                            editor.set_text(json.clone(), window, cx);
                            editor.set_read_only(true);
                        });
                        this.timeline.push(GrpcTimelineEntry::Received(json));
                        this.send_status = SendStatus::Done;
                    }
                    Err(error) => {
                        this.timeline
                            .push(GrpcTimelineEntry::Error(error.to_string()));
                        this.send_status = SendStatus::Error(error.to_string());
                    }
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    /// Server-streaming is the only streaming shape called from here today:
    /// it needs no interleaved client input, so a single upfront request
    /// message is enough to drive the whole call. Client-streaming and
    /// bidirectional streaming need the UI to send additional messages
    /// *while* the call is in flight, which needs its own interactive
    /// send-during-receive design -- deliberately left for a follow-up
    /// rather than shipped half-working.
    fn send_server_streaming(
        &mut self,
        address: String,
        tls: GrpcTlsConfig,
        pool: DescriptorPool,
        method: GrpcMethodInfo,
        request_json: String,
        metadata: Vec<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let receiver = match api_client::call_server_streaming(
                address,
                tls,
                pool,
                method,
                request_json,
                metadata,
            ) {
                Ok(receiver) => receiver,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.timeline
                            .push(GrpcTimelineEntry::Error(error.to_string()));
                        this.send_status = SendStatus::Error(error.to_string());
                        cx.notify();
                    })
                    .log_err();
                    return;
                }
            };

            while let Ok(chunk) = receiver.recv().await {
                let stop = this
                    .update(cx, |this, cx| {
                        match chunk {
                            Ok(json) => this.timeline.push(GrpcTimelineEntry::Received(json)),
                            Err(message) => {
                                this.timeline
                                    .push(GrpcTimelineEntry::Error(message.clone()));
                                this.send_status = SendStatus::Error(message);
                                cx.notify();
                                return true;
                            }
                        }
                        cx.notify();
                        false
                    })
                    .unwrap_or(true);
                if stop {
                    return;
                }
            }

            this.update(cx, |this, cx| {
                if matches!(this.send_status, SendStatus::Sending) {
                    this.send_status = SendStatus::Done;
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn render_service_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = v_flex().gap_2();
        for service in &self.services {
            let mut method_list = v_flex().gap_0p5();
            for method in &service.methods {
                let is_selected = self.selected_method.as_ref() == Some(method);
                let streaming_note = match (method.client_streaming, method.server_streaming) {
                    (false, false) => "unary",
                    (false, true) => "server-streaming",
                    (true, false) => "client-streaming (send unsupported)",
                    (true, true) => "bidi-streaming (send unsupported)",
                };
                let method_for_click = method.clone();
                method_list = method_list.child(
                    div()
                        .id(SharedString::from(format!(
                            "grpc-method-{}",
                            method.full_name
                        )))
                        .debug_selector({
                            let full_name = method.full_name.clone();
                            move || format!("grpc-method-{full_name}")
                        })
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .cursor_pointer()
                        .when(is_selected, |el| {
                            el.bg(cx.theme().colors().element_selected)
                        })
                        .when(!is_selected, |el| {
                            el.hover(|el| el.bg(cx.theme().colors().element_hover))
                        })
                        .child(
                            h_flex()
                                .gap_2()
                                .child(Label::new(method.name.clone()).size(LabelSize::Small))
                                .child(
                                    Label::new(streaming_note)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_method(method_for_click.clone(), cx)
                        })),
                );
            }
            list = list.child(
                v_flex()
                    .gap_1()
                    .child(
                        Label::new(service.full_name.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(method_list),
            );
        }
        list
    }

    fn render_timeline(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = cx;
        let mut list = v_flex().gap_2();
        for entry in &self.timeline {
            let (label, color, text) = match entry {
                GrpcTimelineEntry::Sent(text) => ("Sent", Color::Accent, text.clone()),
                GrpcTimelineEntry::Received(text) => ("Received", Color::Success, text.clone()),
                GrpcTimelineEntry::Error(text) => ("Error", Color::Error, text.clone()),
            };
            list = list.child(
                v_flex()
                    .gap_0p5()
                    .child(Label::new(label).size(LabelSize::Small).color(color))
                    .child(Label::new(text).size(LabelSize::Small).color(Color::Muted)),
            );
        }
        list
    }
}

impl Focusable for GrpcView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for GrpcView {}

impl Item for GrpcView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "gRPC Call".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Terminal))
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(ItemEvent)) {}
}

impl Render for GrpcView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().colors().border;
        let background = cx.theme().colors().background;
        let tls_enabled = self.tls_enabled;

        let connect_row = h_flex()
            .gap_2()
            .child(div().flex_1().child(self.address_editor.clone()))
            .child(
                div()
                    .id("grpc-tls-toggle")
                    .debug_selector(|| "grpc-tls-toggle".to_string())
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(border)
                    .cursor_pointer()
                    .when(tls_enabled, |el| {
                        el.bg(cx.theme().colors().element_selected)
                    })
                    .child(Label::new("TLS").size(LabelSize::Small))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_tls(cx))),
            )
            .child(
                div()
                    .id("grpc-connect-reflection-hitbox")
                    .debug_selector(|| "grpc-connect-reflection".to_string())
                    .child(
                        Button::new("grpc-connect-reflection", "Connect via Reflection")
                            .style(cyberpunk::Rank::Accent.style())
                            .on_click(
                            cx.listener(|this, _, window, cx| {
                                this.connect_via_reflection(window, cx)
                            }),
                        ),
                    ),
            )
            .child(
                div()
                    .id("grpc-import-proto-hitbox")
                    .debug_selector(|| "grpc-import-proto".to_string())
                    .child(
                        Button::new("grpc-import-proto", "Import .proto Files")
                            .style(cyberpunk::Rank::Quiet.style())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.import_proto_files(window, cx)
                            })),
                    ),
            );

        let tls_row = h_flex().gap_2().when(tls_enabled, |row| {
            row.child(self.ca_certificate_path_editor.clone())
                .child(self.client_certificate_path_editor.clone())
                .child(self.client_key_path_editor.clone())
                .child(self.domain_name_editor.clone())
        });

        let (status_icon, status_color, status_text) = match &self.connect_status {
            ConnectStatus::Idle => (IconName::Dash, Color::Muted, "Not connected.".to_string()),
            ConnectStatus::Connecting => (
                IconName::ArrowCircle,
                Color::Muted,
                "Connecting...".to_string(),
            ),
            ConnectStatus::Connected { service_count } => (
                IconName::Check,
                Color::Success,
                format!("Connected -- {service_count} service(s) discovered."),
            ),
            ConnectStatus::Error(message) => (
                IconName::XCircle,
                Color::Error,
                format!("Connection failed: {message}"),
            ),
        };
        let status_label = h_flex()
            .gap_1()
            .child(
                Icon::new(status_icon)
                    .size(IconSize::Small)
                    .color(status_color),
            )
            .child(
                Label::new(status_text)
                    .size(LabelSize::Small)
                    .color(status_color),
            );

        let services_panel = div()
            .id("grpc-services-panel")
            .min_w(px(240.))
            .max_h(px(480.))
            .overflow_scroll()
            .track_scroll(&self.services_scroll_handle)
            .border_r_1()
            .border_color(border)
            .pr_2()
            .child(self.render_service_list(cx))
            .custom_scrollbars(
                Scrollbars::always_visible(ScrollAxes::Vertical)
                    .tracked_scroll_handle(&self.services_scroll_handle),
                window,
                cx,
            );

        let selected_method = self.selected_method.clone();
        let can_send = selected_method
            .as_ref()
            .is_some_and(|method| !method.client_streaming);

        let mut detail_column = v_flex()
            .id("grpc-detail-column")
            .flex_1()
            .min_h_0()
            .gap_2()
            .pl_2()
            .overflow_scroll()
            .track_scroll(&self.detail_scroll_handle);
        if let Some(method) = &selected_method {
            detail_column = detail_column.child(
                Label::new(method.full_name.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        detail_column =
            detail_column
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .id("grpc-use-example-hitbox")
                                .debug_selector(|| "grpc-use-example".to_string())
                                .child(
                                    Button::new("grpc-use-example", "Use Example Message")
                                        .style(cyberpunk::Rank::Quiet.style())
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.use_example_message(window, cx)
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .id("grpc-beautify-hitbox")
                                .debug_selector(|| "grpc-beautify".to_string())
                                .child(
                                    Button::new("grpc-beautify", "Beautify")
                                        .style(cyberpunk::Rank::Quiet.style())
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.beautify_request(window, cx)
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .id("grpc-send-hitbox")
                                .debug_selector(|| "grpc-send".to_string())
                                .when(can_send, |el| {
                                    el.child(Button::new("grpc-send", "Send")
                                        .style(cyberpunk::Rank::Accent.style())
                                        .on_click(
                                        cx.listener(|this, _, window, cx| this.send(window, cx)),
                                    ))
                                })
                                .when(!can_send, |el| {
                                    el.child(
                                        Label::new("Client-streaming send is not yet supported.")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                }),
                        ),
                )
                .child(
                    div()
                        .min_h(px(160.))
                        .px_2()
                        .py_1p5()
                        .rounded_md()
                        .border_1()
                        .border_color(border)
                        .bg(background)
                        .child(self.request_editor.clone()),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new("Metadata")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            div()
                                .id("grpc-add-metadata-hitbox")
                                .debug_selector(|| "grpc-add-metadata".to_string())
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.add_metadata_row(window, cx)
                                }))
                                .child(Icon::new(IconName::Plus).size(IconSize::XSmall)),
                        ),
                );

        let mut metadata_column = v_flex().gap_1();
        for (index, row) in self.metadata_rows.iter().enumerate() {
            let _ = index;
            metadata_column = metadata_column.child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(row.key_editor.clone()))
                    .child(div().flex_1().child(row.value_editor.clone())),
            );
        }
        detail_column = detail_column.child(metadata_column);

        match &self.send_status {
            SendStatus::Idle => {}
            SendStatus::Sending => {
                detail_column = detail_column.child(
                    Label::new("Sending...")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            }
            SendStatus::Done => {
                detail_column = detail_column
                    .child(
                        Label::new("Timeline")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(self.render_timeline(cx));
            }
            SendStatus::Error(message) => {
                detail_column = detail_column
                    .child(
                        Label::new(format!("Call failed: {message}"))
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    )
                    .child(
                        Label::new("Timeline")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(self.render_timeline(cx));
            }
        }

        let detail_column = detail_column.custom_scrollbars(
            Scrollbars::always_visible(ScrollAxes::Vertical)
                .tracked_scroll_handle(&self.detail_scroll_handle),
            window,
            cx,
        );

        v_flex()
            .size_full()
            .p_3()
            .gap_2()
            .track_focus(&self.focus_handle)
            .child(connect_row)
            .child(tls_row)
            .child(status_label)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(services_panel)
                    .child(detail_column),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    const SAMPLE_PROTO: &str = r#"
        syntax = "proto3";
        package greeter;

        message HelloRequest {
            string name = 1;
        }

        message HelloReply {
            string message = 1;
        }

        service Greeter {
            rpc SayHello (HelloRequest) returns (HelloReply);
            rpc SayHelloStream (HelloRequest) returns (stream HelloReply);
        }
    "#;

    fn sample_pool() -> DescriptorPool {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("greeter.proto");
        std::fs::write(&path, SAMPLE_PROTO).expect("write proto file");
        api_client::descriptor_pool_from_proto_files(
            std::slice::from_ref(&path),
            &[dir.path().to_path_buf()],
        )
        .unwrap()
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    async fn build_grpc_view(cx: &mut TestAppContext) -> (Entity<GrpcView>, VisualTestContext) {
        init_test(cx);
        let window = cx.add_window(|window, cx| GrpcView::new(window, cx));
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let view = window.root(&mut cx).unwrap();
        (view, cx)
    }

    fn debug_center(
        cx: &mut VisualTestContext,
        selector: &'static str,
    ) -> gpui::Point<gpui::Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("expected debug bounds for {selector}"))
            .center()
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn the_view_renders_without_panicking(cx: &mut TestAppContext) {
        let (_view, mut cx) = build_grpc_view(cx).await;
        draw(&mut cx);
    }

    #[gpui::test]
    async fn clicking_the_tls_toggle_flips_tls_enabled(cx: &mut TestAppContext) {
        let (view, mut cx) = build_grpc_view(cx).await;
        draw(&mut cx);

        let toggle = debug_center(&mut cx, "grpc-tls-toggle");
        cx.simulate_click(toggle, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, _| assert!(view.tls_enabled));
    }

    #[gpui::test]
    async fn selecting_a_method_and_using_the_example_message_populates_the_request_editor(
        cx: &mut TestAppContext,
    ) {
        let (view, mut cx) = build_grpc_view(cx).await;
        let pool = sample_pool();
        let services = api_client::list_services(&pool);

        view.update(&mut cx, |view, cx| {
            view.services = services;
            view.pool = Some(pool);
            cx.notify();
        });
        draw(&mut cx);

        let method_row = debug_center(&mut cx, "grpc-method-greeter.Greeter.SayHello");
        cx.simulate_click(method_row, gpui::Modifiers::none());
        cx.run_until_parked();
        view.read_with(&cx, |view, _| {
            assert_eq!(
                view.selected_method.as_ref().map(|m| m.name.as_str()),
                Some("SayHello")
            );
        });
        draw(&mut cx);

        let use_example = debug_center(&mut cx, "grpc-use-example");
        cx.simulate_click(use_example, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, cx| {
            let text = view.request_editor.read(cx).text(cx);
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value["name"], "");
        });
    }

    #[gpui::test]
    async fn selecting_a_server_streaming_method_still_allows_sending(cx: &mut TestAppContext) {
        let (view, mut cx) = build_grpc_view(cx).await;
        let pool = sample_pool();
        let services = api_client::list_services(&pool);

        view.update(&mut cx, |view, cx| {
            view.services = services;
            view.pool = Some(pool);
            cx.notify();
        });
        draw(&mut cx);

        let method_row = debug_center(&mut cx, "grpc-method-greeter.Greeter.SayHelloStream");
        cx.simulate_click(method_row, gpui::Modifiers::none());
        cx.run_until_parked();
        draw(&mut cx);

        assert!(
            cx.debug_bounds("grpc-send").is_some(),
            "server-streaming methods should still show a Send button"
        );
    }

    #[gpui::test]
    async fn clicking_add_metadata_adds_an_editable_row(cx: &mut TestAppContext) {
        let (view, mut cx) = build_grpc_view(cx).await;
        draw(&mut cx);

        let add_button = debug_center(&mut cx, "grpc-add-metadata");
        cx.simulate_click(add_button, gpui::Modifiers::none());
        cx.run_until_parked();

        view.update_in(&mut cx, |view, window, cx| {
            let key_editor = view.metadata_rows[0].key_editor.clone();
            key_editor.update(cx, |editor, cx| {
                editor.set_text("authorization", window, cx);
            });
        });
        cx.run_until_parked();

        view.read_with(&cx, |view, cx| {
            let metadata = view.current_metadata(cx);
            assert_eq!(metadata, vec![("authorization".to_string(), String::new())]);
        });
    }
}
