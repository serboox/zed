use db_client::{ConnectionConfig, DatabaseDriver};
use editor::Editor;
use gpui::{App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, Window};
use ui::{Button, ButtonCommon, ButtonStyle, Checkbox, Label, LabelSize, ToggleState, prelude::*};
use uuid::Uuid;
use workspace::ModalView;

pub struct ConnectionModal {
    focus_handle: FocusHandle,
    selected_driver: DatabaseDriver,
    label_editor: Entity<Editor>,
    host_editor: Entity<Editor>,
    port_editor: Entity<Editor>,
    username_editor: Entity<Editor>,
    password_editor: Entity<Editor>,
    database_editor: Entity<Editor>,
    auto_connect: bool,
    use_ssh: bool,
    ssh_host_editor: Entity<Editor>,
    ssh_port_editor: Entity<Editor>,
    ssh_username_editor: Entity<Editor>,
    ssh_key_path_editor: Entity<Editor>,
    pub on_confirm: Option<Box<dyn FnOnce(ConnectionConfig, &mut App)>>,
}

impl ModalView for ConnectionModal {}

impl ConnectionModal {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let make_editor = |placeholder: &'static str,
                           initial: &str,
                           window: &mut Window,
                           cx: &mut Context<ConnectionModal>| {
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
        let database_editor = make_editor("Database (optional)", "", window, cx);
        let ssh_host_editor = make_editor("SSH Host / IP", "", window, cx);
        let ssh_port_editor = make_editor("SSH Port", "22", window, cx);
        let ssh_username_editor = make_editor("SSH Username", "", window, cx);
        let ssh_key_path_editor = make_editor("~/.ssh/id_rsa", "", window, cx);

        Self {
            focus_handle,
            selected_driver: DatabaseDriver::MySQL,
            label_editor,
            host_editor,
            port_editor,
            username_editor,
            password_editor,
            database_editor,
            auto_connect: true,
            use_ssh: false,
            ssh_host_editor,
            ssh_port_editor,
            ssh_username_editor,
            ssh_key_path_editor,
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
                           cx: &mut Context<ConnectionModal>| {
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
        let db_initial = config.database.as_deref().unwrap_or("");
        let database_editor = make_editor("Database (optional)", db_initial, window, cx);

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

        Self {
            focus_handle,
            selected_driver: config.driver,
            label_editor,
            host_editor,
            port_editor,
            username_editor,
            password_editor,
            database_editor,
            auto_connect: config.auto_connect,
            use_ssh,
            ssh_host_editor,
            ssh_port_editor,
            ssh_username_editor,
            ssh_key_path_editor,
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
        })
    }

    fn render_field(label: &'static str, editor: Entity<Editor>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
            .child(div().border_1().rounded_md().px_2().py_1().child(editor))
    }

    fn render_driver_button(
        label: &'static str,
        driver: DatabaseDriver,
        selected: DatabaseDriver,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = driver == selected;
        Button::new(SharedString::from(format!("driver-{label}")), label)
            .style(if is_selected {
                ButtonStyle::Filled
            } else {
                ButtonStyle::Subtle
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.set_driver(driver, window, cx);
            }))
    }
}

impl EventEmitter<DismissEvent> for ConnectionModal {}

impl Focusable for ConnectionModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ConnectionModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_file_based = self.selected_driver.is_file_based();
        let selected_driver = self.selected_driver;
        let use_ssh = self.use_ssh;

        div()
            .track_focus(&self.focus_handle)
            .key_context("ConnectionModal")
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .w(px(440.))
            .child(Label::new("New Database Connection"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_1()
                    .child(Self::render_driver_button(
                        "MySQL",
                        DatabaseDriver::MySQL,
                        selected_driver,
                        cx,
                    ))
                    .child(Self::render_driver_button(
                        "PostgreSQL",
                        DatabaseDriver::PostgreSQL,
                        selected_driver,
                        cx,
                    ))
                    .child(Self::render_driver_button(
                        "SQLite",
                        DatabaseDriver::SQLite,
                        selected_driver,
                        cx,
                    ))
                    .child(Self::render_driver_button(
                        "ClickHouse",
                        DatabaseDriver::ClickHouse,
                        selected_driver,
                        cx,
                    ))
                    .child(Self::render_driver_button(
                        "Redis",
                        DatabaseDriver::Redis,
                        selected_driver,
                        cx,
                    )),
            )
            .child(Self::render_field("Name", self.label_editor.clone()))
            .child(Self::render_field(
                if is_file_based { "File Path" } else { "Host" },
                self.host_editor.clone(),
            ))
            .when(!is_file_based, |el| {
                el.child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .child(Self::render_field("Port", self.port_editor.clone())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .child(Self::render_field(
                                    "Username",
                                    self.username_editor.clone(),
                                )),
                        ),
                )
                .child(Self::render_field("Password", self.password_editor.clone()))
                .child(Self::render_field("Database", self.database_editor.clone()))
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
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
                    .child(
                        Label::new("SSH Tunnel")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .when(use_ssh, |el| {
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .pl_3()
                        .border_l_2()
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .child(Self::render_field(
                                            "SSH Host",
                                            self.ssh_host_editor.clone(),
                                        )),
                                )
                                .child(
                                    div()
                                        .w(px(80.))
                                        .child(Self::render_field(
                                            "Port",
                                            self.ssh_port_editor.clone(),
                                        )),
                                ),
                        )
                        .child(Self::render_field(
                            "SSH Username",
                            self.ssh_username_editor.clone(),
                        ))
                        .child(Self::render_field(
                            "Private Key Path",
                            self.ssh_key_path_editor.clone(),
                        )),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(
                                Checkbox::new(
                                    "auto-connect",
                                    if self.auto_connect {
                                        ToggleState::Selected
                                    } else {
                                        ToggleState::Unselected
                                    },
                                )
                                .on_click(cx.listener(|this, _state, _, cx| {
                                    this.auto_connect = !this.auto_connect;
                                    cx.notify();
                                })),
                            )
                            .child(
                                Label::new("Auto-connect on startup")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                Button::new("cancel", "Cancel")
                                    .style(ButtonStyle::Subtle)
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(DismissEvent);
                                    })),
                            )
                            .child(
                                Button::new("connect", "Connect")
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
            )
    }
}
