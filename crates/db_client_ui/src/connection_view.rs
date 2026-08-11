use crate::driver_icon::brand_icon;
use db_client::{
    ConnectionConfig, DatabaseDriver, KubernetesRelayCommandKind, KubernetesTargetKind,
    KubernetesTunnelModeKind, SshAuthMethod, SslMode, kubernetes_tunnel_caveat,
};
use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, Window,
};
use ui::{
    Button, ButtonCommon, ButtonStyle, Checkbox, Icon, IconName, Label, LabelSize, ToggleState,
    prelude::*,
};
use uuid::Uuid;
use workspace::{Item, item::ItemEvent};

const FOLDER_PLACEHOLDER: &str = "Folder (optional)";
const COLOR_PLACEHOLDER: &str = "#rrggbb (optional)";

/// Parses a `#rrggbb` hex string to an RGB triple `(r, g, b)` in 0–255.
/// Returns `None` for invalid or empty input.
fn parse_hex_color(s: &str) -> Option<(u8, u8, u8)> {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

fn rgb_to_u32(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// Named environment color presets. The hex is what gets stored in
/// `ConnectionConfig::env_color`; the label is what the user picks by.
pub(crate) const ENV_COLOR_PRESETS: &[(&str, &str)] = &[
    ("Local", "#3fb950"),
    ("Development", "#2dd4bf"),
    ("Staging", "#d29922"),
    ("Production", "#f85149"),
    ("Neutral", "#8b949e"),
];

#[derive(Clone)]
enum TestState {
    Idle,
    Testing,
    Success,
    Failure(String),
}

pub struct ConnectionView {
    focus_handle: FocusHandle,
    title: SharedString,
    selected_driver: DatabaseDriver,
    label_editor: Entity<Editor>,
    host_editor: Entity<Editor>,
    port_editor: Entity<Editor>,
    username_editor: Entity<Editor>,
    password_editor: Entity<Editor>,
    database_editor: Entity<Editor>,
    folder_editor: Entity<Editor>,
    color_editor: Entity<Editor>,
    auto_connect: bool,
    read_only: bool,
    use_ssh: bool,
    ssh_host_editor: Entity<Editor>,
    ssh_port_editor: Entity<Editor>,
    ssh_username_editor: Entity<Editor>,
    ssh_key_path_editor: Entity<Editor>,
    ssh_auth_method: SshAuthMethod,
    ssh_password_editor: Entity<Editor>,
    ssl_mode: SslMode,
    ssl_ca_path_editor: Entity<Editor>,
    ssl_client_cert_path_editor: Entity<Editor>,
    ssl_client_key_path_editor: Entity<Editor>,
    use_kubernetes_tunnel: bool,
    k8s_context_editor: Entity<Editor>,
    k8s_namespace_editor: Entity<Editor>,
    k8s_kubeconfig_path_editor: Entity<Editor>,
    k8s_tunnel_mode: KubernetesTunnelModeKind,
    k8s_relay_command: KubernetesRelayCommandKind,
    k8s_target_kind: KubernetesTargetKind,
    k8s_target_name_editor: Entity<Editor>,
    test_state: TestState,
    pub on_confirm: Option<Box<dyn FnOnce(ConnectionConfig, &mut App)>>,
}

impl ConnectionView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let make_editor = |placeholder: &'static str,
                           initial: &str,
                           window: &mut Window,
                           cx: &mut Context<ConnectionView>| {
            let initial = initial.to_string();
            cx.new(|cx| {
                let mut ed = Editor::single_line(window, cx);
                ed.set_placeholder_text(placeholder, window, cx);
                if !initial.is_empty() {
                    ed.set_text(initial, window, cx);
                }
                ed
            })
        };

        let focus_handle = cx.focus_handle();
        let label_editor = make_editor("Connection name (optional)", "", window, cx);
        let host_editor = make_editor("Host / IP", "127.0.0.1", window, cx);
        let port_editor = make_editor("Port", "3306", window, cx);
        let username_editor = make_editor("Username", "root", window, cx);
        let password_editor = make_editor("Password", "", window, cx);
        password_editor.update(cx, |editor, cx| editor.set_masked(true, cx));
        let database_editor = make_editor("Database (optional)", "", window, cx);
        let folder_editor = make_editor(FOLDER_PLACEHOLDER, "", window, cx);
        let color_editor = make_editor(COLOR_PLACEHOLDER, "", window, cx);
        let ssh_host_editor = make_editor("SSH Host / IP", "", window, cx);
        let ssh_port_editor = make_editor("SSH Port", "22", window, cx);
        let ssh_username_editor = make_editor("SSH Username", "", window, cx);
        let ssh_key_path_editor = make_editor("~/.ssh/id_rsa", "", window, cx);
        let ssh_password_editor = make_editor("SSH Password", "", window, cx);
        ssh_password_editor.update(cx, |editor, cx| editor.set_masked(true, cx));
        let ssl_ca_path_editor = make_editor("CA Certificate Path (optional)", "", window, cx);
        let ssl_client_cert_path_editor =
            make_editor("Client Certificate Path (optional)", "", window, cx);
        let ssl_client_key_path_editor = make_editor("Client Key Path (optional)", "", window, cx);
        let k8s_context_editor = make_editor("Kubeconfig Context", "", window, cx);
        let k8s_namespace_editor = make_editor("Namespace", "default", window, cx);
        let k8s_kubeconfig_path_editor = make_editor(
            "Kubeconfig Path (optional, defaults to ~/.kube/config)",
            "",
            window,
            cx,
        );
        let k8s_target_name_editor = make_editor("Pod or Service name", "", window, cx);

        Self {
            focus_handle,
            title: "New Connection".into(),
            selected_driver: DatabaseDriver::MySQL,
            label_editor,
            host_editor,
            port_editor,
            username_editor,
            password_editor,
            database_editor,
            folder_editor,
            color_editor,
            auto_connect: true,
            read_only: false,
            use_ssh: false,
            ssh_host_editor,
            ssh_port_editor,
            ssh_username_editor,
            ssh_key_path_editor,
            ssh_auth_method: SshAuthMethod::KeyFile,
            ssh_password_editor,
            ssl_mode: SslMode::Disabled,
            ssl_ca_path_editor,
            ssl_client_cert_path_editor,
            ssl_client_key_path_editor,
            use_kubernetes_tunnel: false,
            k8s_context_editor,
            k8s_namespace_editor,
            k8s_kubeconfig_path_editor,
            k8s_tunnel_mode: KubernetesTunnelModeKind::PortForward,
            k8s_relay_command: KubernetesRelayCommandKind::Socat,
            k8s_target_kind: KubernetesTargetKind::Pod,
            k8s_target_name_editor,
            test_state: TestState::Idle,
            on_confirm: None,
        }
    }

    pub fn new_with_config(
        config: &ConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let make_editor = |placeholder: &'static str,
                           initial: &str,
                           window: &mut Window,
                           cx: &mut Context<ConnectionView>| {
            let initial = initial.to_string();
            cx.new(|cx| {
                let mut ed = Editor::single_line(window, cx);
                ed.set_placeholder_text(placeholder, window, cx);
                if !initial.is_empty() {
                    ed.set_text(initial, window, cx);
                }
                ed
            })
        };

        let focus_handle = cx.focus_handle();
        let label_editor = make_editor("Connection name (optional)", &config.label, window, cx);
        let host_editor = make_editor("Host / IP", &config.host, window, cx);
        let port_str = if config.port > 0 {
            config.port.to_string()
        } else {
            String::new()
        };
        let port_editor = make_editor("Port", &port_str, window, cx);
        let username_editor = make_editor("Username", &config.username, window, cx);
        let password_editor = make_editor("Password", &config.password, window, cx);
        password_editor.update(cx, |editor, cx| editor.set_masked(true, cx));
        let db_initial = config.database.as_deref().unwrap_or("");
        let database_editor = make_editor("Database (optional)", db_initial, window, cx);
        let folder_editor = make_editor(
            FOLDER_PLACEHOLDER,
            config.folder.as_deref().unwrap_or(""),
            window,
            cx,
        );
        let color_editor = make_editor(
            COLOR_PLACEHOLDER,
            config.env_color.as_deref().unwrap_or(""),
            window,
            cx,
        );

        let use_ssh = config.uses_ssh();
        let ssh_host_editor = make_editor(
            "SSH Host / IP",
            config.ssh_host.as_deref().unwrap_or(""),
            window,
            cx,
        );
        let ssh_port_str = config.ssh_port.to_string();
        let ssh_port_editor = make_editor("SSH Port", &ssh_port_str, window, cx);
        let ssh_username_editor = make_editor(
            "SSH Username",
            config.ssh_username.as_deref().unwrap_or(""),
            window,
            cx,
        );
        let ssh_key_path_editor = make_editor(
            "~/.ssh/id_rsa",
            config.ssh_private_key_path.as_deref().unwrap_or(""),
            window,
            cx,
        );
        let ssh_password_editor = make_editor("SSH Password", &config.ssh_password, window, cx);
        ssh_password_editor.update(cx, |editor, cx| editor.set_masked(true, cx));
        let ssl_ca_path_editor = make_editor(
            "CA Certificate Path (optional)",
            config.ssl_ca_path.as_deref().unwrap_or(""),
            window,
            cx,
        );
        let ssl_client_cert_path_editor = make_editor(
            "Client Certificate Path (optional)",
            config.ssl_client_cert_path.as_deref().unwrap_or(""),
            window,
            cx,
        );
        let ssl_client_key_path_editor = make_editor(
            "Client Key Path (optional)",
            config.ssl_client_key_path.as_deref().unwrap_or(""),
            window,
            cx,
        );

        let use_kubernetes_tunnel = config.uses_kubernetes_tunnel();
        let k8s_context_editor = make_editor(
            "Kubeconfig Context",
            config.k8s_context.as_deref().unwrap_or(""),
            window,
            cx,
        );
        let k8s_namespace_editor = make_editor("Namespace", &config.k8s_namespace, window, cx);
        let k8s_kubeconfig_path_editor = make_editor(
            "Kubeconfig Path (optional, defaults to ~/.kube/config)",
            config.k8s_kubeconfig_path.as_deref().unwrap_or(""),
            window,
            cx,
        );
        let k8s_target_name_editor =
            make_editor("Pod or Service name", &config.k8s_target_name, window, cx);

        Self {
            focus_handle,
            title: "Edit Connection".into(),
            selected_driver: config.driver,
            label_editor,
            host_editor,
            port_editor,
            username_editor,
            password_editor,
            database_editor,
            folder_editor,
            color_editor,
            auto_connect: config.auto_connect,
            read_only: config.read_only,
            use_ssh,
            ssh_host_editor,
            ssh_port_editor,
            ssh_username_editor,
            ssh_key_path_editor,
            ssh_auth_method: config.ssh_auth_method,
            ssh_password_editor,
            ssl_mode: config.ssl_mode,
            ssl_ca_path_editor,
            ssl_client_cert_path_editor,
            ssl_client_key_path_editor,
            use_kubernetes_tunnel,
            k8s_context_editor,
            k8s_namespace_editor,
            k8s_kubeconfig_path_editor,
            k8s_tunnel_mode: config.k8s_tunnel_mode,
            k8s_relay_command: config.k8s_relay_command,
            k8s_target_kind: config.k8s_target_kind,
            k8s_target_name_editor,
            test_state: TestState::Idle,
            on_confirm: None,
        }
    }

    pub fn with_on_confirm(
        mut self,
        callback: impl FnOnce(ConnectionConfig, &mut App) + 'static,
    ) -> Self {
        self.on_confirm = Some(Box::new(callback));
        self
    }

    fn run_test_connection(&mut self, cx: &mut Context<Self>) {
        let Some(config) = self.build_config(cx) else {
            self.test_state = TestState::Failure("Enter host and username first.".to_string());
            cx.notify();
            return;
        };
        self.test_state = TestState::Testing;
        cx.notify();
        let task = crate::store::test_connection(config, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.test_state = match result {
                    Ok(()) => TestState::Success,
                    Err(error) => TestState::Failure(error.to_string()),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set_driver(&mut self, driver: DatabaseDriver, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_driver == driver {
            return;
        }
        self.selected_driver = driver;
        let default_port = driver.default_port().to_string();
        self.port_editor.update(cx, |ed, cx| {
            ed.set_text(default_port, window, cx);
        });
        cx.notify();
    }

    fn read_text(editor: &Entity<Editor>, cx: &App) -> String {
        editor.read(cx).text(cx)
    }

    pub fn build_config(&self, cx: &App) -> Option<ConnectionConfig> {
        let label_raw = Self::read_text(&self.label_editor, cx);
        let driver = self.selected_driver;

        let folder_raw = Self::read_text(&self.folder_editor, cx);
        let folder_trimmed = folder_raw.trim();
        let folder = if folder_trimmed.is_empty() {
            None
        } else {
            Some(folder_trimmed.to_string())
        };

        let color_raw = Self::read_text(&self.color_editor, cx);
        let env_color = if parse_hex_color(color_raw.trim()).is_some() {
            let normalized = format!(
                "#{}",
                color_raw.trim().trim_start_matches('#').to_lowercase()
            );
            Some(normalized)
        } else {
            None
        };

        let ssh_host = if self.use_ssh {
            let h = Self::read_text(&self.ssh_host_editor, cx);
            if h.is_empty() { None } else { Some(h) }
        } else {
            None
        };
        let ssh_port: u16 = if self.use_ssh {
            Self::read_text(&self.ssh_port_editor, cx)
                .parse()
                .unwrap_or(22)
        } else {
            22
        };
        let ssh_username = if self.use_ssh {
            let u = Self::read_text(&self.ssh_username_editor, cx);
            if u.is_empty() { None } else { Some(u) }
        } else {
            None
        };
        let ssh_private_key_path = if self.use_ssh {
            let k = Self::read_text(&self.ssh_key_path_editor, cx);
            if k.is_empty() { None } else { Some(k) }
        } else {
            None
        };
        let ssh_password = if self.use_ssh {
            Self::read_text(&self.ssh_password_editor, cx)
        } else {
            String::new()
        };
        let ssh_auth_method = self.ssh_auth_method;

        let ssl_mode = self.ssl_mode;
        let non_empty = |text: String| if text.is_empty() { None } else { Some(text) };
        let ssl_ca_path = non_empty(Self::read_text(&self.ssl_ca_path_editor, cx));
        let ssl_client_cert_path =
            non_empty(Self::read_text(&self.ssl_client_cert_path_editor, cx));
        let ssl_client_key_path = non_empty(Self::read_text(&self.ssl_client_key_path_editor, cx));

        let k8s_context = if self.use_kubernetes_tunnel {
            let c = Self::read_text(&self.k8s_context_editor, cx);
            if c.is_empty() { None } else { Some(c) }
        } else {
            None
        };
        let k8s_namespace = {
            let n = Self::read_text(&self.k8s_namespace_editor, cx);
            if n.is_empty() {
                "default".to_string()
            } else {
                n
            }
        };
        let k8s_kubeconfig_path = non_empty(Self::read_text(&self.k8s_kubeconfig_path_editor, cx));
        let k8s_tunnel_mode = self.k8s_tunnel_mode;
        let k8s_relay_command = self.k8s_relay_command;
        let k8s_target_kind = self.k8s_target_kind;
        let k8s_target_name = Self::read_text(&self.k8s_target_name_editor, cx);

        if driver.is_file_based() {
            let path = Self::read_text(&self.host_editor, cx);
            if path.is_empty() {
                return None;
            }
            let label = if label_raw.is_empty() {
                std::path::Path::new(&path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&path)
                    .to_string()
            } else {
                label_raw
            };
            return Some(ConnectionConfig {
                id: Uuid::new_v4(),
                label,
                driver,
                host: path,
                port: 0,
                username: String::new(),
                password: String::new(),
                database: None,
                auto_connect: self.auto_connect,
                ssh_host,
                ssh_port,
                ssh_username,
                ssh_private_key_path,
                ssh_auth_method,
                ssh_password,
                ssl_mode,
                ssl_ca_path,
                ssl_client_cert_path,
                ssl_client_key_path,
                k8s_context,
                k8s_namespace,
                k8s_kubeconfig_path,
                k8s_tunnel_mode,
                k8s_relay_command,
                k8s_target_kind,
                k8s_target_name,
                folder,
                folder_id: None,
                order: 0,
                env_color,
                read_only: self.read_only,
            });
        }

        let host = Self::read_text(&self.host_editor, cx);
        let port_str = Self::read_text(&self.port_editor, cx);
        let username = Self::read_text(&self.username_editor, cx);
        let password = Self::read_text(&self.password_editor, cx);
        let database_raw = Self::read_text(&self.database_editor, cx);

        if host.is_empty() || username.is_empty() {
            return None;
        }
        let port: u16 = port_str.parse().unwrap_or_else(|_| driver.default_port());

        Some(ConnectionConfig {
            id: Uuid::new_v4(),
            label: if label_raw.is_empty() {
                format!("{}@{}", username, host)
            } else {
                label_raw
            },
            driver,
            host,
            port,
            username,
            password,
            database: if database_raw.is_empty() {
                None
            } else {
                Some(database_raw)
            },
            auto_connect: self.auto_connect,
            ssh_host,
            ssh_port,
            ssh_username,
            ssh_private_key_path,
            ssh_auth_method,
            ssh_password,
            ssl_mode,
            ssl_ca_path,
            ssl_client_cert_path,
            ssl_client_key_path,
            k8s_context,
            k8s_namespace,
            k8s_kubeconfig_path,
            k8s_tunnel_mode,
            k8s_relay_command,
            k8s_target_kind,
            k8s_target_name,
            folder,
            folder_id: None,
            order: 0,
            env_color,
            read_only: self.read_only,
        })
    }

    fn render_chip(
        label: &'static str,
        is_selected: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &gpui::ClickEvent, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        div()
            .id(SharedString::from(format!("chip-{label}")))
            .debug_selector(move || format!("chip-{label}"))
            .px_2()
            .py_0p5()
            .rounded_md()
            .cursor_pointer()
            .when(is_selected, |el| el.bg(colors.element_selected))
            .when(!is_selected, |el| {
                el.hover(|el| el.bg(colors.element_hover))
            })
            .child(
                Label::new(label)
                    .size(LabelSize::Small)
                    .color(if is_selected {
                        Color::Default
                    } else {
                        Color::Muted
                    }),
            )
            .on_click(cx.listener(on_click))
    }

    fn render_field(
        label: &'static str,
        editor: Entity<Editor>,
        border: gpui::Hsla,
        field_bg: gpui::Hsla,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
            .child(
                div()
                    .w_full()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(border)
                    .bg(field_bg)
                    .child(editor),
            )
    }

    fn render_driver_row(
        label: &'static str,
        driver: DatabaseDriver,
        selected: DatabaseDriver,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = driver == selected;
        let colors = cx.theme().colors();
        h_flex()
            .id(SharedString::from(format!("driver-row-{label}")))
            .w_full()
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded_md()
            .cursor_pointer()
            .when(is_selected, |row| row.bg(colors.element_selected))
            .when(!is_selected, |row| {
                row.hover(|row| row.bg(colors.element_hover))
            })
            .child(brand_icon(driver, IconSize::Small))
            .child(Label::new(label).color(if is_selected {
                Color::Default
            } else {
                Color::Muted
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.set_driver(driver, window, cx);
            }))
    }
}

impl EventEmitter<DismissEvent> for ConnectionView {}

impl Focusable for ConnectionView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for ConnectionView {
    type Event = DismissEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Plus))
    }

    fn to_item_events(_event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(ItemEvent::CloseItem);
    }
}

impl Render for ConnectionView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_file_based = self.selected_driver.is_file_based();
        let selected_driver = self.selected_driver;
        let use_ssh = self.use_ssh;
        let use_kubernetes_tunnel = self.use_kubernetes_tunnel;
        let k8s_tunnel_mode = self.k8s_tunnel_mode;
        let k8s_relay_command = self.k8s_relay_command;
        let k8s_target_kind = self.k8s_target_kind;

        let colors = cx.theme().colors();
        let page_bg = colors.editor_background;
        let card_bg = colors.elevated_surface_background;
        let card_border = colors.border_variant;
        let field_border = colors.border;
        let field_bg = colors.background;
        let divider = colors.border_variant;

        let sidebar = v_flex()
            .w(px(220.))
            .flex_none()
            .gap_0p5()
            .p_3()
            .border_r_1()
            .border_color(divider)
            .child(
                Label::new("Databases")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(Self::render_driver_row(
                "MySQL",
                DatabaseDriver::MySQL,
                selected_driver,
                cx,
            ))
            .child(Self::render_driver_row(
                "PostgreSQL",
                DatabaseDriver::PostgreSQL,
                selected_driver,
                cx,
            ))
            .child(Self::render_driver_row(
                "MongoDB",
                DatabaseDriver::MongoDB,
                selected_driver,
                cx,
            ))
            .child(Self::render_driver_row(
                "Cassandra",
                DatabaseDriver::Cassandra,
                selected_driver,
                cx,
            ))
            .child(Self::render_driver_row(
                "SQLite",
                DatabaseDriver::SQLite,
                selected_driver,
                cx,
            ))
            .child(Self::render_driver_row(
                "Aerospike",
                DatabaseDriver::Aerospike,
                selected_driver,
                cx,
            ))
            .child(Self::render_driver_row(
                "Redis",
                DatabaseDriver::Redis,
                selected_driver,
                cx,
            ))
            .child(Self::render_driver_row(
                "ClickHouse",
                DatabaseDriver::ClickHouse,
                selected_driver,
                cx,
            ));

        let header = h_flex()
            .gap_2()
            .items_center()
            .child(brand_icon(selected_driver, IconSize::Medium))
            .child(
                Label::new(format!("{} — {}", self.title, selected_driver)).size(LabelSize::Large),
            );

        let fields = v_flex()
            .flex_1()
            .gap_3()
            .p_4()
            .child(header)
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Self::render_field(
                        "Name",
                        self.label_editor.clone(),
                        field_border,
                        field_bg,
                    )))
                    .child(div().flex_1().child(Self::render_field(
                        "Folder",
                        self.folder_editor.clone(),
                        field_border,
                        field_bg,
                    ))),
            )
            .child({
                let color_raw = Self::read_text(&self.color_editor, cx);
                let current_norm = parse_hex_color(color_raw.trim()).map(|_| {
                    format!(
                        "#{}",
                        color_raw.trim().trim_start_matches('#').to_lowercase()
                    )
                });
                let swatch_color = parse_hex_color(color_raw.trim())
                    .map(|(r, g, b)| gpui::rgb(rgb_to_u32(r, g, b)));
                let accent = cx.theme().colors().text_accent;

                let mut presets = h_flex().gap_3().flex_wrap();
                for (name, hex) in ENV_COLOR_PRESETS {
                    let hex = *hex;
                    let is_selected = current_norm.as_deref() == Some(hex);
                    let dot_color =
                        parse_hex_color(hex).map(|(r, g, b)| gpui::rgb(rgb_to_u32(r, g, b)));
                    presets = presets.child(
                        h_flex()
                            .id(SharedString::from(format!("env-preset-{name}")))
                            .gap_1p5()
                            .items_center()
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.color_editor.update(cx, |editor, cx| {
                                    editor.set_text(hex, window, cx);
                                });
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .size(px(16.))
                                    .rounded_full()
                                    .border_2()
                                    .border_color(if is_selected { accent } else { field_border })
                                    .when_some(dot_color, |el, color| el.bg(color)),
                            )
                            .child(Label::new(SharedString::from(*name)).size(LabelSize::Small)),
                    );
                }
                presets = presets.child(
                    Button::new("env-preset-none", "No Color")
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.color_editor.update(cx, |editor, cx| {
                                editor.set_text("", window, cx);
                            });
                            cx.notify();
                        })),
                );

                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_end()
                            .child(div().flex_1().child(Self::render_field(
                                "Environment Color",
                                self.color_editor.clone(),
                                field_border,
                                field_bg,
                            )))
                            .child(
                                div()
                                    .w(px(24.))
                                    .h(px(24.))
                                    .mb(px(2.))
                                    .rounded_full()
                                    .border_1()
                                    .border_color(field_border)
                                    .when_some(swatch_color, |el, color| el.bg(color))
                                    .when(swatch_color.is_none(), |el| el.bg(field_bg)),
                            ),
                    )
                    .child(presets)
            })
            .child(if is_file_based {
                div().w_full().child(Self::render_field(
                    "File Path",
                    self.host_editor.clone(),
                    field_border,
                    field_bg,
                ))
            } else {
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Self::render_field(
                        "Host",
                        self.host_editor.clone(),
                        field_border,
                        field_bg,
                    )))
                    .child(div().w(px(120.)).child(Self::render_field(
                        "Port",
                        self.port_editor.clone(),
                        field_border,
                        field_bg,
                    )))
            })
            .when(!is_file_based, |el| {
                el.child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(Self::render_field(
                            "Username",
                            self.username_editor.clone(),
                            field_border,
                            field_bg,
                        )))
                        .child(div().flex_1().child(Self::render_field(
                            "Password",
                            self.password_editor.clone(),
                            field_border,
                            field_bg,
                        ))),
                )
                .child(Self::render_field(
                    "Database",
                    self.database_editor.clone(),
                    field_border,
                    field_bg,
                ))
                .when(
                    matches!(
                        selected_driver,
                        DatabaseDriver::MySQL | DatabaseDriver::PostgreSQL
                    ),
                    |el| {
                        el.child(
                            v_flex()
                                .gap_2()
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            Label::new("SSL Mode")
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(Self::render_chip(
                                            "Disabled",
                                            self.ssl_mode == SslMode::Disabled,
                                            cx,
                                            |this, _, _, cx| {
                                                this.ssl_mode = SslMode::Disabled;
                                                cx.notify();
                                            },
                                        ))
                                        .child(Self::render_chip(
                                            "Require",
                                            self.ssl_mode == SslMode::Require,
                                            cx,
                                            |this, _, _, cx| {
                                                this.ssl_mode = SslMode::Require;
                                                cx.notify();
                                            },
                                        ))
                                        .child(Self::render_chip(
                                            "Verify CA",
                                            self.ssl_mode == SslMode::VerifyCa,
                                            cx,
                                            |this, _, _, cx| {
                                                this.ssl_mode = SslMode::VerifyCa;
                                                cx.notify();
                                            },
                                        ))
                                        .child(Self::render_chip(
                                            "Verify Full",
                                            self.ssl_mode == SslMode::VerifyFull,
                                            cx,
                                            |this, _, _, cx| {
                                                this.ssl_mode = SslMode::VerifyFull;
                                                cx.notify();
                                            },
                                        )),
                                )
                                .when(
                                    matches!(
                                        self.ssl_mode,
                                        SslMode::VerifyCa | SslMode::VerifyFull
                                    ),
                                    |el| {
                                        el.child(Self::render_field(
                                            "CA Certificate Path",
                                            self.ssl_ca_path_editor.clone(),
                                            field_border,
                                            field_bg,
                                        ))
                                        .child(
                                            h_flex()
                                                .gap_2()
                                                .child(div().flex_1().child(Self::render_field(
                                                    "Client Certificate Path",
                                                    self.ssl_client_cert_path_editor.clone(),
                                                    field_border,
                                                    field_bg,
                                                )))
                                                .child(div().flex_1().child(Self::render_field(
                                                    "Client Key Path",
                                                    self.ssl_client_key_path_editor.clone(),
                                                    field_border,
                                                    field_bg,
                                                ))),
                                        )
                                    },
                                ),
                        )
                    },
                )
            })
            .child(
                h_flex()
                    .items_center()
                    .gap_1p5()
                    .child(
                        Checkbox::new(
                            "use-ssh",
                            if use_ssh {
                                ToggleState::Selected
                            } else {
                                ToggleState::Unselected
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.use_ssh = !this.use_ssh;
                            cx.notify();
                        })),
                    )
                    .child(Label::new("SSH Tunnel").size(LabelSize::Small)),
            )
            .when(use_ssh, |el| {
                el.child(
                    v_flex()
                        .gap_2()
                        .pl_3()
                        .border_l_2()
                        .border_color(divider)
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().flex_1().child(Self::render_field(
                                    "SSH Host",
                                    self.ssh_host_editor.clone(),
                                    field_border,
                                    field_bg,
                                )))
                                .child(div().w(px(96.)).child(Self::render_field(
                                    "Port",
                                    self.ssh_port_editor.clone(),
                                    field_border,
                                    field_bg,
                                ))),
                        )
                        .child(Self::render_field(
                            "SSH Username",
                            self.ssh_username_editor.clone(),
                            field_border,
                            field_bg,
                        ))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(Self::render_chip(
                                    "Key File",
                                    self.ssh_auth_method == SshAuthMethod::KeyFile,
                                    cx,
                                    |this, _, _, cx| {
                                        this.ssh_auth_method = SshAuthMethod::KeyFile;
                                        cx.notify();
                                    },
                                ))
                                .child(Self::render_chip(
                                    "Password",
                                    self.ssh_auth_method == SshAuthMethod::Password,
                                    cx,
                                    |this, _, _, cx| {
                                        this.ssh_auth_method = SshAuthMethod::Password;
                                        cx.notify();
                                    },
                                )),
                        )
                        .child(if self.ssh_auth_method == SshAuthMethod::KeyFile {
                            Self::render_field(
                                "Private Key Path",
                                self.ssh_key_path_editor.clone(),
                                field_border,
                                field_bg,
                            )
                        } else {
                            Self::render_field(
                                "SSH Password",
                                self.ssh_password_editor.clone(),
                                field_border,
                                field_bg,
                            )
                        }),
                )
            })
            .child(
                h_flex()
                    .items_center()
                    .gap_1p5()
                    .child(
                        div()
                            .debug_selector(|| "use-kubernetes-tunnel-checkbox".to_string())
                            .child(
                                Checkbox::new(
                                    "use-kubernetes-tunnel",
                                    if use_kubernetes_tunnel {
                                        ToggleState::Selected
                                    } else {
                                        ToggleState::Unselected
                                    },
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.use_kubernetes_tunnel = !this.use_kubernetes_tunnel;
                                    cx.notify();
                                })),
                            ),
                    )
                    .child(Label::new("Kubernetes Tunnel").size(LabelSize::Small)),
            )
            .when(use_kubernetes_tunnel, |el| {
                el.child(
                    v_flex()
                        .gap_2()
                        .pl_3()
                        .border_l_2()
                        .border_color(divider)
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().flex_1().child(Self::render_field(
                                    "Kubeconfig Context",
                                    self.k8s_context_editor.clone(),
                                    field_border,
                                    field_bg,
                                )))
                                .child(div().flex_1().child(Self::render_field(
                                    "Namespace",
                                    self.k8s_namespace_editor.clone(),
                                    field_border,
                                    field_bg,
                                ))),
                        )
                        .child(Self::render_field(
                            "Kubeconfig Path (optional)",
                            self.k8s_kubeconfig_path_editor.clone(),
                            field_border,
                            field_bg,
                        ))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(Self::render_chip(
                                    "Pod",
                                    k8s_target_kind == KubernetesTargetKind::Pod,
                                    cx,
                                    |this, _, _, cx| {
                                        this.k8s_target_kind = KubernetesTargetKind::Pod;
                                        cx.notify();
                                    },
                                ))
                                .child(Self::render_chip(
                                    "Service",
                                    k8s_target_kind == KubernetesTargetKind::Service,
                                    cx,
                                    |this, _, _, cx| {
                                        this.k8s_target_kind = KubernetesTargetKind::Service;
                                        cx.notify();
                                    },
                                )),
                        )
                        .child(Self::render_field(
                            if k8s_target_kind == KubernetesTargetKind::Pod {
                                "Pod name"
                            } else {
                                "Service name"
                            },
                            self.k8s_target_name_editor.clone(),
                            field_border,
                            field_bg,
                        ))
                        .child(
                            v_flex()
                                .gap_1()
                                .child(
                                    Label::new("Tunnel mode")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(Self::render_chip(
                                            "Port-forward",
                                            k8s_tunnel_mode == KubernetesTunnelModeKind::PortForward,
                                            cx,
                                            |this, _, _, cx| {
                                                this.k8s_tunnel_mode =
                                                    KubernetesTunnelModeKind::PortForward;
                                                cx.notify();
                                            },
                                        ))
                                        .child(Self::render_chip(
                                            "Exec (kubectl exec)",
                                            k8s_tunnel_mode == KubernetesTunnelModeKind::Exec,
                                            cx,
                                            |this, _, _, cx| {
                                                this.k8s_tunnel_mode =
                                                    KubernetesTunnelModeKind::Exec;
                                                cx.notify();
                                            },
                                        )),
                                )
                                .child(
                                    Label::new(match k8s_tunnel_mode {
                                        KubernetesTunnelModeKind::PortForward => {
                                            "Requires the 'portforward' RBAC permission on the target pod or service."
                                        }
                                        KubernetesTunnelModeKind::Exec => {
                                            "Requires the 'exec' RBAC permission. Bridges the connection through a relay binary already present in the container."
                                        }
                                    })
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                                ),
                        )
                        .when(k8s_tunnel_mode == KubernetesTunnelModeKind::Exec, |el| {
                            el.child(
                                h_flex()
                                    .gap_2()
                                    .child(Self::render_chip(
                                        "socat",
                                        k8s_relay_command == KubernetesRelayCommandKind::Socat,
                                        cx,
                                        |this, _, _, cx| {
                                            this.k8s_relay_command =
                                                KubernetesRelayCommandKind::Socat;
                                            cx.notify();
                                        },
                                    ))
                                    .child(Self::render_chip(
                                        "nc",
                                        k8s_relay_command == KubernetesRelayCommandKind::Nc,
                                        cx,
                                        |this, _, _, cx| {
                                            this.k8s_relay_command = KubernetesRelayCommandKind::Nc;
                                            cx.notify();
                                        },
                                    )),
                            )
                        })
                        .when_some(kubernetes_tunnel_caveat(selected_driver), |el, caveat| {
                            el.child(
                                div()
                                    .debug_selector(|| "k8s-tunnel-caveat".to_string())
                                    .p_2()
                                    .rounded_md()
                                    .bg(page_bg)
                                    .border_1()
                                    .border_color(divider)
                                    .child(
                                        Label::new(caveat)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            )
                        }),
                )
            })
            .child(div().h(px(1.)).w_full().bg(divider))
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        h_flex()
                            .items_center()
                            .gap_3()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_1p5()
                                    .child(
                                        Checkbox::new(
                                            "auto-connect",
                                            if self.auto_connect {
                                                ToggleState::Selected
                                            } else {
                                                ToggleState::Unselected
                                            },
                                        )
                                        .on_click(
                                            cx.listener(|this, _state, _, cx| {
                                                this.auto_connect = !this.auto_connect;
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                    .child(
                                        Label::new("Auto-connect on startup")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .debug_selector(|| "read-only-checkbox".to_string())
                                            .child(
                                                Checkbox::new(
                                                    "read-only",
                                                    if self.read_only {
                                                        ToggleState::Selected
                                                    } else {
                                                        ToggleState::Unselected
                                                    },
                                                )
                                                .on_click(cx.listener(|this, _state, _, cx| {
                                                    this.read_only = !this.read_only;
                                                    cx.notify();
                                                })),
                                            ),
                                    )
                                    .child(
                                        Label::new("Read-only (block all writes)")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .when_some(
                                match &self.test_state {
                                    TestState::Idle => None,
                                    TestState::Testing => {
                                        Some(("Testing…".to_string(), Color::Muted))
                                    }
                                    TestState::Success => {
                                        Some(("Connected".to_string(), Color::Success))
                                    }
                                    TestState::Failure(message) => {
                                        Some((message.clone(), Color::Error))
                                    }
                                },
                                |el, (message, color)| {
                                    el.child(
                                        Label::new(message).size(LabelSize::Small).color(color),
                                    )
                                },
                            )
                            .child(
                                Button::new("test", "Test Connection")
                                    .style(ButtonStyle::Subtle)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.run_test_connection(cx);
                                    })),
                            )
                            .child(
                                Button::new("cancel", "Cancel")
                                    .style(ButtonStyle::Subtle)
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(DismissEvent);
                                    })),
                            )
                            .child(
                                // Saving is all this does: the connection is
                                // written down and the pane closes. Nothing is
                                // dialled until the reader opens it.
                                Button::new("save", "Save")
                                    .style(ButtonStyle::Filled)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(config) = this.build_config(cx) {
                                            if let Some(callback) = this.on_confirm.take() {
                                                callback(config, cx);
                                            }
                                        }
                                        cx.emit(DismissEvent);
                                    })),
                            ),
                    ),
            );

        let card = h_flex()
            .w_full()
            .flex_1()
            .items_stretch()
            .rounded_lg()
            .border_1()
            .border_color(card_border)
            .bg(card_bg)
            .child(sidebar)
            .child(fields);

        div()
            .id("connection-view")
            .track_focus(&self.focus_handle)
            .key_context("ConnectionView")
            .size_full()
            .bg(page_bg)
            .flex()
            .flex_col()
            .p_6()
            .overflow_y_scroll()
            .child(card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use settings::SettingsStore;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    #[test]
    fn env_color_presets_parse_to_valid_hex() {
        for (name, hex) in ENV_COLOR_PRESETS {
            assert!(
                parse_hex_color(hex).is_some(),
                "preset {name} has invalid hex {hex}"
            );
        }
    }

    #[gpui::test]
    async fn build_config_uses_selected_env_color_preset(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|window, cx| ConnectionView::new(window, cx));

        let production_hex = ENV_COLOR_PRESETS
            .iter()
            .find(|(name, _)| *name == "Production")
            .map(|(_, hex)| *hex)
            .expect("Production preset exists");

        window
            .update(cx, |view, window, cx| {
                view.host_editor
                    .update(cx, |ed, cx| ed.set_text("db.example.com", window, cx));
                view.username_editor
                    .update(cx, |ed, cx| ed.set_text("alice", window, cx));
                // Picking a preset sets the color editor to its hex.
                view.color_editor
                    .update(cx, |ed, cx| ed.set_text(production_hex, window, cx));
            })
            .unwrap();

        let config = window
            .read_with(cx, |view, cx| view.build_config(cx))
            .unwrap()
            .expect("config should build");
        assert_eq!(config.env_color.as_deref(), Some(production_hex));
    }

    #[gpui::test]
    async fn clicking_the_read_only_checkbox_toggles_it_in_the_built_config(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let window = cx.add_window(|window, cx| ConnectionView::new(window, cx));

        window
            .update(cx, |view, window, cx| {
                view.host_editor
                    .update(cx, |ed, cx| ed.set_text("127.0.0.1", window, cx));
                view.username_editor
                    .update(cx, |ed, cx| ed.set_text("root", window, cx));
            })
            .unwrap();

        let default_config = window
            .read_with(cx, |view, cx| view.build_config(cx))
            .unwrap()
            .expect("config should build");
        assert!(
            !default_config.read_only,
            "a new connection must default to read-write"
        );

        let cx = &mut gpui::VisualTestContext::from_window(*window, cx);
        let checkbox = cx
            .debug_bounds("read-only-checkbox")
            .expect("the read-only checkbox should be rendered")
            .center();
        cx.simulate_click(checkbox, gpui::Modifiers::none());

        let config = window
            .read_with(cx, |view, cx| view.build_config(cx))
            .unwrap()
            .expect("config should build");
        assert!(
            config.read_only,
            "a real click on the read-only checkbox must flip the built config's flag"
        );
    }

    #[gpui::test]
    async fn clicking_ssl_mode_and_ssh_auth_chips_updates_the_built_config(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let window = cx.add_window(|window, cx| ConnectionView::new(window, cx));

        window
            .update(cx, |view, window, cx| {
                view.host_editor
                    .update(cx, |ed, cx| ed.set_text("db.example.com", window, cx));
                view.username_editor
                    .update(cx, |ed, cx| ed.set_text("root", window, cx));
                view.use_ssh = true;
                view.ssh_host_editor
                    .update(cx, |ed, cx| ed.set_text("bastion.example.com", window, cx));
                view.ssh_username_editor
                    .update(cx, |ed, cx| ed.set_text("tunneluser", window, cx));
            })
            .unwrap();

        let default_config = window
            .read_with(cx, |view, cx| view.build_config(cx))
            .unwrap()
            .expect("config should build");
        assert_eq!(default_config.ssl_mode, SslMode::Disabled);
        assert_eq!(default_config.ssh_auth_method, SshAuthMethod::KeyFile);

        let cx = &mut gpui::VisualTestContext::from_window(*window, cx);

        let require_chip = cx
            .debug_bounds("chip-Require")
            .expect("the SSL Require chip should be rendered for a MySQL connection")
            .center();
        cx.simulate_click(require_chip, gpui::Modifiers::none());

        let password_chip = cx
            .debug_bounds("chip-Password")
            .expect("the SSH auth-method Password chip should be rendered while SSH is enabled")
            .center();
        cx.simulate_click(password_chip, gpui::Modifiers::none());

        window
            .update(cx, |view, window, cx| {
                view.ssh_password_editor
                    .update(cx, |ed, cx| ed.set_text("tunnel-secret", window, cx));
            })
            .unwrap();

        let config = window
            .read_with(cx, |view, cx| view.build_config(cx))
            .unwrap()
            .expect("config should build");
        assert_eq!(
            config.ssl_mode,
            SslMode::Require,
            "a real click on the Require chip must select SSL Require in the built config"
        );
        assert_eq!(
            config.ssh_auth_method,
            SshAuthMethod::Password,
            "a real click on the Password chip must switch the built config off key-file auth"
        );
        assert_eq!(config.ssh_password, "tunnel-secret");
    }

    #[gpui::test]
    async fn new_with_config_preserves_an_existing_read_only_flag(cx: &mut TestAppContext) {
        init_test(cx);
        let mut source = ConnectionConfig::default();
        source.host = "127.0.0.1".to_string();
        source.username = "root".to_string();
        source.read_only = true;

        let window =
            cx.add_window(|window, cx| ConnectionView::new_with_config(&source, window, cx));

        let config = window
            .read_with(cx, |view, cx| view.build_config(cx))
            .unwrap()
            .expect("config should build");
        assert!(
            config.read_only,
            "editing an existing read-only connection must not silently clear the flag"
        );
    }

    #[gpui::test]
    async fn build_config_reads_network_fields(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|window, cx| ConnectionView::new(window, cx));

        window
            .update(cx, |view, window, cx| {
                view.set_driver(DatabaseDriver::PostgreSQL, window, cx);
                view.label_editor
                    .update(cx, |ed, cx| ed.set_text("Prod", window, cx));
                view.host_editor
                    .update(cx, |ed, cx| ed.set_text("db.example.com", window, cx));
                view.port_editor
                    .update(cx, |ed, cx| ed.set_text("6543", window, cx));
                view.username_editor
                    .update(cx, |ed, cx| ed.set_text("alice", window, cx));
                view.password_editor
                    .update(cx, |ed, cx| ed.set_text("secret", window, cx));
                view.database_editor
                    .update(cx, |ed, cx| ed.set_text("shop", window, cx));
            })
            .unwrap();

        let config = window
            .read_with(cx, |view, cx| view.build_config(cx))
            .unwrap()
            .expect("config should build when host and username are set");

        assert_eq!(config.driver, DatabaseDriver::PostgreSQL);
        assert_eq!(config.label, "Prod");
        assert_eq!(config.host, "db.example.com");
        assert_eq!(config.port, 6543);
        assert_eq!(config.username, "alice");
        assert_eq!(config.password, "secret");
        assert_eq!(config.database.as_deref(), Some("shop"));
        assert!(config.ssh_host.is_none());
    }

    #[gpui::test]
    async fn build_config_includes_ssh_fields_when_enabled(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|window, cx| ConnectionView::new(window, cx));

        window
            .update(cx, |view, window, cx| {
                view.host_editor
                    .update(cx, |ed, cx| ed.set_text("127.0.0.1", window, cx));
                view.username_editor
                    .update(cx, |ed, cx| ed.set_text("root", window, cx));
                view.use_ssh = true;
                view.ssh_host_editor
                    .update(cx, |ed, cx| ed.set_text("bastion.example.com", window, cx));
                view.ssh_port_editor
                    .update(cx, |ed, cx| ed.set_text("2222", window, cx));
                view.ssh_username_editor
                    .update(cx, |ed, cx| ed.set_text("tunnel", window, cx));
                view.ssh_key_path_editor.update(cx, |ed, cx| {
                    ed.set_text("/home/u/.ssh/id_ed25519", window, cx)
                });
            })
            .unwrap();

        let config = window
            .read_with(cx, |view, cx| view.build_config(cx))
            .unwrap()
            .expect("config should build");

        assert_eq!(config.ssh_host.as_deref(), Some("bastion.example.com"));
        assert_eq!(config.ssh_port, 2222);
        assert_eq!(config.ssh_username.as_deref(), Some("tunnel"));
        assert_eq!(
            config.ssh_private_key_path.as_deref(),
            Some("/home/u/.ssh/id_ed25519")
        );
    }

    #[gpui::test]
    async fn build_config_derives_label_for_file_based_driver(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|window, cx| ConnectionView::new(window, cx));

        window
            .update(cx, |view, window, cx| {
                view.set_driver(DatabaseDriver::SQLite, window, cx);
                view.host_editor
                    .update(cx, |ed, cx| ed.set_text("/var/data/shop.db", window, cx));
            })
            .unwrap();

        let config = window
            .read_with(cx, |view, cx| view.build_config(cx))
            .unwrap()
            .expect("config should build for a file path");

        assert_eq!(config.driver, DatabaseDriver::SQLite);
        assert_eq!(config.host, "/var/data/shop.db");
        assert_eq!(config.port, 0);
        assert_eq!(config.label, "shop.db");
    }

    #[gpui::test]
    async fn build_config_returns_none_without_host(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|window, cx| ConnectionView::new(window, cx));

        window
            .update(cx, |view, window, cx| {
                view.host_editor
                    .update(cx, |ed, cx| ed.set_text("", window, cx));
                view.username_editor
                    .update(cx, |ed, cx| ed.set_text("root", window, cx));
            })
            .unwrap();

        let config = window
            .read_with(cx, |view, cx| view.build_config(cx))
            .unwrap();
        assert!(config.is_none(), "missing host must fail validation");
    }

    #[gpui::test]
    async fn password_fields_render_masked(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|window, cx| ConnectionView::new(window, cx));

        window
            .update(cx, |view, window, cx| {
                view.password_editor
                    .update(cx, |ed, cx| ed.set_text("s3cr3t-pw", window, cx));
                view.ssh_password_editor
                    .update(cx, |ed, cx| ed.set_text("ssh-s3cr3t", window, cx));
                view.username_editor
                    .update(cx, |ed, cx| ed.set_text("alice", window, cx));
            })
            .unwrap();

        window
            .update(cx, |view, window, cx| {
                // The rendered (display) text of a masked editor is bullets, not
                // the real characters; a plain-text field would echo them back.
                let password_display = view
                    .password_editor
                    .update(cx, |ed, cx| ed.snapshot(window, cx).display_snapshot.text());
                assert!(
                    !password_display.contains("s3cr3t-pw"),
                    "password field must not render its plaintext"
                );
                assert!(
                    password_display.chars().all(|c| c == '*'),
                    "password field must render as mask chars, got {password_display:?}"
                );

                let ssh_display = view
                    .ssh_password_editor
                    .update(cx, |ed, cx| ed.snapshot(window, cx).display_snapshot.text());
                assert!(
                    !ssh_display.contains("ssh-s3cr3t"),
                    "ssh password field must not render its plaintext"
                );
                assert!(
                    ssh_display.chars().all(|c| c == '*'),
                    "ssh password field must render as mask chars, got {ssh_display:?}"
                );

                // A non-secret field is unaffected: it still shows real text.
                let username_display = view
                    .username_editor
                    .update(cx, |ed, cx| ed.snapshot(window, cx).display_snapshot.text());
                assert_eq!(
                    username_display, "alice",
                    "non-secret fields must remain visible"
                );

                // The underlying value is preserved for building the config.
                let stored = ConnectionView::read_text(&view.password_editor, cx);
                assert_eq!(
                    stored, "s3cr3t-pw",
                    "masking must not alter the stored value"
                );
            })
            .unwrap();
    }

    #[test]
    fn dismiss_event_maps_to_close_item() {
        let mut events = Vec::new();
        ConnectionView::to_item_events(&DismissEvent, &mut |event| events.push(event));
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ItemEvent::CloseItem));
    }

    #[gpui::test]
    async fn clicking_the_kubernetes_checkbox_and_mode_chips_updates_the_built_config(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let window = cx.add_window(|window, cx| ConnectionView::new(window, cx));

        window
            .update(cx, |view, window, cx| {
                view.host_editor
                    .update(cx, |ed, cx| ed.set_text("localhost", window, cx));
                view.username_editor
                    .update(cx, |ed, cx| ed.set_text("root", window, cx));
            })
            .unwrap();

        let before = window
            .read_with(cx, |view, cx| view.build_config(cx))
            .unwrap()
            .expect("config should build");
        assert!(
            !before.uses_kubernetes_tunnel(),
            "a new connection must default to no Kubernetes tunnel"
        );

        let cx = &mut gpui::VisualTestContext::from_window(*window, cx);

        // A real click on the checkbox -- not a direct field assignment --
        // is what must reveal the Kubernetes fields, mirroring the existing
        // SSH/read-only checkbox tests' philosophy of driving the actual
        // event path.
        let checkbox = cx
            .debug_bounds("use-kubernetes-tunnel-checkbox")
            .expect("the Kubernetes tunnel checkbox should be rendered")
            .center();
        cx.simulate_click(checkbox, gpui::Modifiers::none());

        window
            .update(cx, |view, window, cx| {
                view.k8s_context_editor
                    .update(cx, |ed, cx| ed.set_text("prod-cluster", window, cx));
                view.k8s_target_name_editor
                    .update(cx, |ed, cx| ed.set_text("aerospike-0", window, cx));
            })
            .unwrap();

        // A real click on the "Exec" mode chip -- not a direct field
        // assignment -- must switch the mode used when the config is built.
        let exec_chip = cx
            .debug_bounds("chip-Exec (kubectl exec)")
            .expect("the Exec mode chip should be rendered")
            .center();
        cx.simulate_click(exec_chip, gpui::Modifiers::none());

        let config = window
            .read_with(cx, |view, cx| view.build_config(cx))
            .unwrap()
            .expect("config should build");
        assert!(config.uses_kubernetes_tunnel());
        assert_eq!(config.k8s_context.as_deref(), Some("prod-cluster"));
        assert_eq!(config.k8s_target_name, "aerospike-0");
        assert_eq!(
            config.k8s_tunnel_mode,
            db_client::KubernetesTunnelModeKind::Exec,
            "clicking the Exec chip must select Exec mode in the built config"
        );

        // A real click on the "Service" target-kind chip must flip the
        // target from the default Pod to Service.
        let service_chip = cx
            .debug_bounds("chip-Service")
            .expect("the Service target chip should be rendered")
            .center();
        cx.simulate_click(service_chip, gpui::Modifiers::none());

        let config = window
            .read_with(cx, |view, cx| view.build_config(cx))
            .unwrap()
            .expect("config should build");
        assert_eq!(
            config.k8s_target_kind,
            db_client::KubernetesTargetKind::Service,
            "clicking the Service chip must select the Service target kind"
        );
    }

    #[gpui::test]
    async fn kubernetes_tunnel_caveat_only_paints_for_aerospike(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|window, cx| ConnectionView::new(window, cx));

        window
            .update(cx, |view, window, cx| {
                view.use_kubernetes_tunnel = true;
                view.set_driver(DatabaseDriver::MySQL, window, cx);
            })
            .unwrap();

        let cx = &mut gpui::VisualTestContext::from_window(*window, cx);
        assert!(
            cx.debug_bounds("k8s-tunnel-caveat").is_none(),
            "MySQL has no cluster peer-discovery caveat and must not render one"
        );

        window
            .update(cx, |view, window, cx| {
                view.set_driver(DatabaseDriver::Aerospike, window, cx);
            })
            .unwrap();
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("k8s-tunnel-caveat").is_some(),
            "switching to Aerospike with the Kubernetes tunnel enabled must paint the caveat"
        );
    }
}
