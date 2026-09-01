use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use db_client::connection::{ConnectionConfig, DatabaseDriver};
use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Task,
    Window, prelude::*,
};
use ui::{
    Button, ButtonStyle, Checkbox, Divider, Icon, IconName, Label, LabelSize, cyberpunk, prelude::*,
};
use workspace::ModalView;

use crate::widgets::{dialog_header, dialog_surface, text_field};

/// Invoked when the dump dialog is confirmed, so the owning panel can spawn the
/// run without this dialog depending on the panel type.
pub type DumpRunCallback = Arc<dyn Fn(DumpRequest, &mut Window, &mut App)>;

/// One toggle in the dump dialog. Each option maps to a driver-specific command
/// flag; `None` means the flag has no equivalent for that driver and is skipped.
#[derive(Clone, Copy)]
struct DumpOption {
    label: &'static str,
    mysql_flag: Option<&'static str>,
    postgres_flag: Option<&'static str>,
    default_on: bool,
}

const DUMP_OPTIONS: &[DumpOption] = &[
    DumpOption {
        label: "Add DROP TABLE before CREATE TABLE",
        mysql_flag: Some("--add-drop-table"),
        postgres_flag: Some("--clean"),
        default_on: true,
    },
    DumpOption {
        label: "Add DISABLE KEYS before each INSERT",
        mysql_flag: Some("--disable-keys"),
        postgres_flag: None,
        default_on: true,
    },
    DumpOption {
        label: "Add LOCK TABLES before each table dump",
        mysql_flag: Some("--lock-tables"),
        postgres_flag: None,
        default_on: true,
    },
    DumpOption {
        label: "Add DROP TRIGGER before CREATE TRIGGER",
        mysql_flag: Some("--add-drop-trigger"),
        postgres_flag: None,
        default_on: false,
    },
    DumpOption {
        label: "Export schema without data",
        mysql_flag: Some("--no-data"),
        postgres_flag: Some("--schema-only"),
        default_on: false,
    },
    DumpOption {
        label: "Export schema without tablespaces",
        mysql_flag: Some("--no-tablespaces"),
        postgres_flag: Some("--no-tablespaces"),
        default_on: false,
    },
    DumpOption {
        label: "Export without table creation",
        mysql_flag: Some("--no-create-info"),
        postgres_flag: Some("--data-only"),
        default_on: false,
    },
    DumpOption {
        label: "Include column names in each INSERT",
        mysql_flag: Some("--complete-insert"),
        postgres_flag: Some("--column-inserts"),
        default_on: false,
    },
    DumpOption {
        label: "Include all table options in CREATE TABLE",
        mysql_flag: Some("--create-options"),
        postgres_flag: None,
        default_on: true,
    },
    DumpOption {
        label: "Include stored routines in the dump",
        mysql_flag: Some("--routines"),
        postgres_flag: None,
        default_on: false,
    },
    DumpOption {
        label: "Use single INSERT for multiple rows",
        mysql_flag: Some("--extended-insert"),
        postgres_flag: None,
        default_on: true,
    },
];

fn default_executable(driver: DatabaseDriver) -> &'static str {
    match driver {
        DatabaseDriver::PostgreSQL => "pg_dump",
        _ => "mysqldump",
    }
}

/// A fully specified dump, built when the user presses Run. The output path may
/// still contain `{timestamp}`/`{data_source}`/`{database}` patterns; the caller
/// resolves them with `apply_substitutions` before spawning. The password is
/// never carried here — it is passed separately to `spawn_dump` and supplied to
/// the child process through an environment variable, never the command line.
#[derive(Debug, Clone, PartialEq)]
pub struct DumpRequest {
    pub driver: DatabaseDriver,
    pub executable: String,
    pub output_path: String,
    pub data_source: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub database: Option<String>,
    pub databases: Vec<String>,
    pub tables: Vec<String>,
    pub flags: Vec<String>,
}

pub enum NativeDumpEvent {
    Run(DumpRequest),
    Dismissed,
}

/// Replaces the supported patterns in an output path. Unknown patterns are left
/// untouched so a stray brace never corrupts the path silently.
pub fn apply_substitutions(
    path: &str,
    data_source: &str,
    database: &str,
    timestamp: &str,
) -> String {
    path.replace("{timestamp}", timestamp)
        .replace("{data_source}", data_source)
        .replace("{database}", database)
}

/// Builds the argument vector for the dump command (without the executable and
/// without the password). The selection syntax differs per driver.
pub fn build_dump_args(request: &DumpRequest, resolved_output: &str) -> Vec<String> {
    let mut args = Vec::new();
    if !request.host.is_empty() {
        args.push("-h".to_string());
        args.push(request.host.clone());
    }
    match request.driver {
        DatabaseDriver::PostgreSQL => {
            if request.port != 0 {
                args.push("-p".to_string());
                args.push(request.port.to_string());
            }
            if !request.username.is_empty() {
                args.push("-U".to_string());
                args.push(request.username.clone());
            }
            args.extend(request.flags.iter().cloned());
            args.push("-f".to_string());
            args.push(resolved_output.to_string());
            for table in &request.tables {
                args.push("-t".to_string());
                args.push(table.clone());
            }
            let database = request
                .databases
                .first()
                .or(request.database.as_ref())
                .cloned();
            if let Some(database) = database {
                args.push(database);
            }
        }
        _ => {
            if request.port != 0 {
                args.push("-P".to_string());
                args.push(request.port.to_string());
            }
            if !request.username.is_empty() {
                args.push("-u".to_string());
                args.push(request.username.clone());
            }
            args.extend(request.flags.iter().cloned());
            args.push(format!("--result-file={resolved_output}"));
            if !request.tables.is_empty() {
                let database = request.databases.first().or(request.database.as_ref());
                if let Some(database) = database {
                    args.push(database.clone());
                }
                args.extend(request.tables.iter().cloned());
            } else if !request.databases.is_empty() {
                args.push("--databases".to_string());
                args.extend(request.databases.iter().cloned());
            } else {
                args.push("--all-databases".to_string());
            }
        }
    }
    args
}

fn preview_command(request: &DumpRequest) -> String {
    let args = build_dump_args(request, &request.output_path);
    let mut parts = vec![request.executable.clone()];
    for arg in args {
        if arg.contains(' ') || arg.contains('{') {
            parts.push(format!("\"{arg}\""));
        } else {
            parts.push(arg);
        }
    }
    parts.join(" ")
}

#[derive(Debug, Clone, PartialEq)]
pub enum DumpStatus {
    Running,
    Done { output_path: String },
    Failed { message: String },
}

#[derive(Debug, Clone)]
pub struct DumpTask {
    pub id: usize,
    pub label: SharedString,
    pub status: DumpStatus,
}

/// Renders a single background dump task for the panel's status strip. Cancel is
/// owned by the panel (it holds the task handle), so it is not drawn here.
pub fn render_dump_status_row(task: &DumpTask, cx: &App) -> impl IntoElement {
    let (icon, color, detail): (IconName, Color, SharedString) = match &task.status {
        DumpStatus::Running => (IconName::ArrowCircle, Color::Accent, "Running…".into()),
        DumpStatus::Done { output_path } => {
            (IconName::Check, Color::Success, output_path.clone().into())
        }
        DumpStatus::Failed { message } => (IconName::XCircle, Color::Error, message.clone().into()),
    };
    h_flex()
        .w_full()
        .gap_2()
        .items_center()
        .px_2()
        .py_1()
        .child(Icon::new(icon).color(color))
        .child(Label::new(task.label.clone()).size(LabelSize::Small))
        .child(
            Label::new(detail)
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .bg(cx.theme().colors().elevated_surface_background)
}

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_temp_path(prefix: &str, suffix: &str) -> PathBuf {
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!("{prefix}{}-{counter}{suffix}", std::process::id());
    std::env::temp_dir().join(name)
}

/// Creates a file that only the owner can read or write. The mode is set at
/// creation time (before the password is written) so the secret is never world
/// readable, even briefly.
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn escape_option_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn write_mysql_defaults_file(password: &str) -> std::io::Result<PathBuf> {
    let path = unique_temp_path("zed-mysqldump-", ".cnf");
    let mut file = create_private_file(&path)?;
    write!(
        file,
        "[client]\npassword=\"{}\"\n",
        escape_option_value(password)
    )?;
    Ok(path)
}

fn escape_pgpass_field(value: &str) -> String {
    value.replace('\\', "\\\\").replace(':', "\\:")
}

fn write_pgpass_file(request: &DumpRequest, password: &str) -> std::io::Result<PathBuf> {
    let path = unique_temp_path("zed-pgpass-", "");
    let mut file = create_private_file(&path)?;
    let port = if request.port == 0 {
        "*".to_string()
    } else {
        request.port.to_string()
    };
    let database = request
        .databases
        .first()
        .or(request.database.as_ref())
        .map(|value| escape_pgpass_field(value))
        .unwrap_or_else(|| "*".to_string());
    let line = format!(
        "{}:{}:{}:{}:{}\n",
        escape_pgpass_field(&request.host),
        port,
        database,
        escape_pgpass_field(&request.username),
        escape_pgpass_field(password),
    );
    file.write_all(line.as_bytes())?;
    Ok(path)
}

/// Prepends the credentials-file option that mysqldump requires to be first.
/// pg_dump reads its password from `PGPASSFILE` instead, so its argv is left
/// unchanged. Either way the password never reaches argv.
fn prepend_password_file(args: &mut Vec<String>, driver: DatabaseDriver, password_file: &Path) {
    if !matches!(driver, DatabaseDriver::PostgreSQL) {
        args.insert(
            0,
            format!("--defaults-extra-file={}", password_file.display()),
        );
    }
}

/// Spawns the external dump tool. The password (if any) is staged in a private
/// 0600 temp file (`--defaults-extra-file` for mysqldump, `PGPASSFILE` for
/// pg_dump) so it never appears in argv or `ps`; the file is removed as soon as
/// the process finishes. On success the resolved output path is returned; on
/// failure the captured stderr.
pub fn spawn_dump(
    request: DumpRequest,
    password: Option<String>,
    resolved_output: String,
    cx: &App,
) -> Task<Result<String, String>> {
    cx.background_spawn(async move {
        let mut args = build_dump_args(&request, &resolved_output);
        let mut command = smol::process::Command::new(&request.executable);
        let password_file = match password.filter(|value| !value.is_empty()) {
            Some(password) => match request.driver {
                DatabaseDriver::PostgreSQL => {
                    let path = write_pgpass_file(&request, &password)
                        .map_err(|error| format!("Could not stage credentials: {error}"))?;
                    command.env("PGPASSFILE", &path);
                    Some(path)
                }
                _ => {
                    let path = write_mysql_defaults_file(&password)
                        .map_err(|error| format!("Could not stage credentials: {error}"))?;
                    prepend_password_file(&mut args, request.driver, &path);
                    Some(path)
                }
            },
            None => None,
        };
        command.args(&args);
        let output = command.output().await;
        if let Some(path) = &password_file {
            std::fs::remove_file(path).ok();
        }
        let output =
            output.map_err(|error| format!("Could not run {}: {error}", request.executable))?;
        if output.status.success() {
            Ok(resolved_output)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let message = if stderr.is_empty() {
                format!("{} exited with {}", request.executable, output.status)
            } else {
                stderr
            };
            Err(message)
        }
    })
}

fn split_list(text: &str) -> Vec<String> {
    text.split(|c| c == ',' || c == ' ')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect()
}

pub struct NativeDumpDialog {
    focus_handle: FocusHandle,
    driver: DatabaseDriver,
    data_source: String,
    host: String,
    port: u16,
    username: String,
    database: Option<String>,
    executable_editor: Entity<Editor>,
    output_editor: Entity<Editor>,
    databases_editor: Entity<Editor>,
    tables_editor: Entity<Editor>,
    option_enabled: Vec<bool>,
    on_run: Option<DumpRunCallback>,
}

impl NativeDumpDialog {
    pub fn new(
        driver: DatabaseDriver,
        config: ConnectionConfig,
        preset_databases: Vec<String>,
        preset_tables: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let executable_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(default_executable(driver), window, cx);
            editor
        });
        let output_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text("{data_source}-{timestamp}-dump.sql", window, cx);
            editor
        });
        let databases_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("All databases", window, cx);
            if !preset_databases.is_empty() {
                editor.set_text(preset_databases.join(", "), window, cx);
            }
            editor
        });
        let tables_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("All tables", window, cx);
            if !preset_tables.is_empty() {
                editor.set_text(preset_tables.join(", "), window, cx);
            }
            editor
        });
        Self {
            focus_handle: cx.focus_handle(),
            driver,
            data_source: config.label,
            host: config.host,
            port: config.port,
            username: config.username,
            database: config.database,
            executable_editor,
            output_editor,
            databases_editor,
            tables_editor,
            option_enabled: DUMP_OPTIONS
                .iter()
                .map(|option| option.default_on)
                .collect(),
            on_run: None,
        }
    }

    pub fn on_run(mut self, callback: DumpRunCallback) -> Self {
        self.on_run = Some(callback);
        self
    }

    fn enabled_flags(&self) -> Vec<String> {
        DUMP_OPTIONS
            .iter()
            .zip(&self.option_enabled)
            .filter(|(_, enabled)| **enabled)
            .filter_map(|(option, _)| match self.driver {
                DatabaseDriver::PostgreSQL => option.postgres_flag,
                _ => option.mysql_flag,
            })
            .map(str::to_string)
            .collect()
    }

    pub fn build_request(&self, cx: &App) -> DumpRequest {
        DumpRequest {
            driver: self.driver,
            executable: self.executable_editor.read(cx).text(cx).trim().to_string(),
            output_path: self.output_editor.read(cx).text(cx).trim().to_string(),
            data_source: self.data_source.clone(),
            host: self.host.clone(),
            port: self.port,
            username: self.username.clone(),
            database: self.database.clone(),
            databases: split_list(&self.databases_editor.read(cx).text(cx)),
            tables: split_list(&self.tables_editor.read(cx).text(cx)),
            flags: self.enabled_flags(),
        }
    }

    fn command_preview(&self, cx: &App) -> String {
        preview_command(&self.build_request(cx))
    }

    fn toggle_option(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(enabled) = self.option_enabled.get_mut(index) {
            *enabled = !*enabled;
            cx.notify();
        }
    }

    fn field_row(
        &self,
        label: &'static str,
        editor: &Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .child(
                div()
                    .w(px(150.))
                    .child(Label::new(label).size(LabelSize::Small)),
            )
            .child(text_field(editor, cx).flex_1())
    }
}

impl EventEmitter<NativeDumpEvent> for NativeDumpDialog {}

impl EventEmitter<DismissEvent> for NativeDumpDialog {}

impl ModalView for NativeDumpDialog {}

impl Focusable for NativeDumpDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for NativeDumpDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = format!("Export with {}…", default_executable(self.driver));
        let preview = self.command_preview(cx);

        let options = DUMP_OPTIONS
            .iter()
            .enumerate()
            .zip(self.option_enabled.iter())
            .map(|((index, option), enabled)| {
                h_flex()
                    .debug_selector(move || format!("DUMP_OPTION_{index}"))
                    .child(
                        Checkbox::new(("dump-option", index), (*enabled).into())
                            .label(option.label)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.toggle_option(index, cx)
                            })),
                    )
            })
            .collect::<Vec<_>>();

        let mut left_column = v_flex().flex_1().gap_1();
        let mut right_column = v_flex().flex_1().gap_1();
        for (index, option) in options.into_iter().enumerate() {
            if index % 2 == 0 {
                left_column = left_column.child(option);
            } else {
                right_column = right_column.child(option);
            }
        }

        dialog_surface(cx)
            .track_focus(&self.focus_handle)
            .key_context("NativeDumpDialog")
            .w(px(720.))
            .max_h(px(640.))
            .p_4()
            .gap_3()
            .flex()
            .flex_col()
            .child(dialog_header(
                title,
                "dump-close",
                cx.listener(|_, _, _, cx| {
                    cx.emit(NativeDumpEvent::Dismissed);
                    cx.emit(DismissEvent);
                }),
                cx,
            ))
            .child(self.field_row("Path to executable:", &self.executable_editor.clone(), cx))
            .child(self.field_row("Output result to:", &self.output_editor.clone(), cx))
            .child(
                Label::new("Allowed substitution patterns: {timestamp}, {data_source}, {database}")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(Divider::horizontal())
            .child(self.field_row("Databases to dump:", &self.databases_editor.clone(), cx))
            .child(self.field_row("Tables to dump:", &self.tables_editor.clone(), cx))
            .child(Divider::horizontal())
            .child(
                h_flex()
                    .w_full()
                    .gap_4()
                    .items_start()
                    .child(left_column)
                    .child(right_column),
            )
            .child(Divider::horizontal())
            .child(
                div()
                    .debug_selector(|| "DUMP_COMMAND_PREVIEW".to_string())
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_none()
                    .bg(cyberpunk::surface())
                    .border_1()
                    .border_color(cyberpunk::border_dim())
                    .child(Label::new(SharedString::from(preview)).size(LabelSize::Small)),
            )
            .child(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .child(
                        div()
                            .debug_selector(|| "DUMP_CANCEL_BTN".to_string())
                            .child(
                                Button::new("dump-cancel", "Cancel")
                                    .style(cyberpunk::Rank::Neutral.style())
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(NativeDumpEvent::Dismissed);
                                        cx.emit(DismissEvent);
                                    })),
                            ),
                    )
                    .child(
                        div().debug_selector(|| "DUMP_RUN_BTN".to_string()).child(
                            Button::new("dump-run", "Run")
                                .style(ButtonStyle::OutlinedCustom(
                                    cyberpunk::Accent::Cyan.border(),
                                ))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    let request = this.build_request(cx);
                                    if let Some(callback) = this.on_run.clone() {
                                        callback(request.clone(), window, cx);
                                    }
                                    cx.emit(NativeDumpEvent::Run(request));
                                    cx.emit(DismissEvent);
                                })),
                        ),
                    ),
            )
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

    fn sample_config() -> ConnectionConfig {
        ConnectionConfig {
            label: "qa".to_string(),
            host: "db.example.com".to_string(),
            port: 3306,
            username: "root".to_string(),
            database: Some("instruments".to_string()),
            ..Default::default()
        }
    }

    fn mysql_request() -> DumpRequest {
        DumpRequest {
            driver: DatabaseDriver::MySQL,
            executable: "mysqldump".to_string(),
            output_path: "{data_source}-{timestamp}-dump.sql".to_string(),
            data_source: "qa".to_string(),
            host: "db.example.com".to_string(),
            port: 3306,
            username: "root".to_string(),
            database: Some("instruments".to_string()),
            databases: Vec::new(),
            tables: Vec::new(),
            flags: vec!["--add-drop-table".to_string()],
        }
    }

    #[test]
    fn all_databases_when_nothing_selected() {
        let args = build_dump_args(&mysql_request(), "out.sql");
        assert!(args.contains(&"--all-databases".to_string()));
        assert!(args.contains(&"--result-file=out.sql".to_string()));
        assert!(args.contains(&"--add-drop-table".to_string()));
        assert!(args.contains(&"-u".to_string()));
    }

    #[test]
    fn databases_selection_uses_databases_flag() {
        let mut request = mysql_request();
        request.databases = vec!["a".to_string(), "b".to_string()];
        let args = build_dump_args(&request, "out.sql");
        assert!(args.contains(&"--databases".to_string()));
        assert!(args.contains(&"a".to_string()));
        assert!(args.contains(&"b".to_string()));
        assert!(!args.contains(&"--all-databases".to_string()));
    }

    #[test]
    fn tables_selection_lists_database_then_tables() {
        let mut request = mysql_request();
        request.databases = vec!["shop".to_string()];
        request.tables = vec!["orders".to_string(), "items".to_string()];
        let args = build_dump_args(&request, "out.sql");
        let shop = args
            .iter()
            .position(|a| a == "shop")
            .expect("database present");
        let orders = args
            .iter()
            .position(|a| a == "orders")
            .expect("table present");
        assert!(shop < orders);
        assert!(!args.contains(&"--databases".to_string()));
    }

    #[test]
    fn postgres_uses_pg_dump_argument_form() {
        let mut request = mysql_request();
        request.driver = DatabaseDriver::PostgreSQL;
        request.executable = "pg_dump".to_string();
        request.flags = vec!["--schema-only".to_string()];
        let args = build_dump_args(&request, "out.sql");
        assert!(args.contains(&"-U".to_string()));
        assert!(args.contains(&"-f".to_string()));
        assert!(args.contains(&"out.sql".to_string()));
        assert!(args.contains(&"--schema-only".to_string()));
        assert!(args.contains(&"instruments".to_string()));
    }

    #[test]
    fn substitutions_replace_known_patterns_only() {
        let resolved = apply_substitutions(
            "/data/{data_source}-{timestamp}-{database}.sql",
            "qa",
            "20260101",
            "noon",
        );
        assert_eq!(resolved, "/data/qa-noon-20260101.sql");
        let untouched = apply_substitutions("/data/{unknown}.sql", "qa", "db", "ts");
        assert_eq!(untouched, "/data/{unknown}.sql");
    }

    #[test]
    fn args_never_contain_a_password() {
        let args = build_dump_args(&mysql_request(), "out.sql");
        assert!(
            !args
                .iter()
                .any(|arg| arg == "-p" || arg.starts_with("--password"))
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("PGPASSWORD") || arg.contains("MYSQL_PWD"))
        );
    }

    #[test]
    fn mysql_password_is_staged_as_first_defaults_extra_file_argument() {
        let mut args = build_dump_args(&mysql_request(), "out.sql");
        let path = Path::new("/tmp/creds.cnf");
        prepend_password_file(&mut args, DatabaseDriver::MySQL, path);
        assert_eq!(
            args.first().map(String::as_str),
            Some("--defaults-extra-file=/tmp/creds.cnf")
        );
        // Postgres relies on PGPASSFILE, so its argv is untouched.
        let mut pg_args = vec!["-f".to_string(), "out.sql".to_string()];
        prepend_password_file(&mut pg_args, DatabaseDriver::PostgreSQL, path);
        assert_eq!(pg_args.first().map(String::as_str), Some("-f"));
    }

    #[test]
    fn mysql_defaults_file_is_private_and_holds_password() {
        let path = write_mysql_defaults_file("s3cr3t!\"\\").expect("file written");
        let contents = std::fs::read_to_string(&path).expect("readable");
        assert!(contents.contains("[client]"));
        assert!(contents.contains("password="));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn pgpass_file_has_five_escaped_fields() {
        let mut request = mysql_request();
        request.driver = DatabaseDriver::PostgreSQL;
        request.databases = vec!["shop".to_string()];
        let path = write_pgpass_file(&request, "pa:ss").expect("file written");
        let contents = std::fs::read_to_string(&path).expect("readable");
        assert!(contents.contains("db.example.com:3306:shop:root:pa\\:ss"));
        std::fs::remove_file(&path).ok();
    }

    #[gpui::test]
    async fn toggling_an_option_updates_the_command_preview(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|window, cx| {
            NativeDumpDialog::new(
                DatabaseDriver::MySQL,
                sample_config(),
                Vec::new(),
                Vec::new(),
                window,
                cx,
            )
        });

        let before = window
            .read_with(cx, |view, cx| view.command_preview(cx))
            .unwrap();
        assert!(before.contains("--add-drop-table"));

        window
            .update(cx, |view, _window, cx| view.toggle_option(0, cx))
            .unwrap();

        let after = window
            .read_with(cx, |view, cx| view.command_preview(cx))
            .unwrap();
        assert!(!after.contains("--add-drop-table"));
    }

    #[gpui::test]
    async fn output_field_change_is_reflected_in_preview(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|window, cx| {
            NativeDumpDialog::new(
                DatabaseDriver::MySQL,
                sample_config(),
                Vec::new(),
                Vec::new(),
                window,
                cx,
            )
        });

        window
            .update(cx, |view, window, cx| {
                view.output_editor.update(cx, |editor, cx| {
                    editor.set_text("/tmp/custom.sql", window, cx)
                });
            })
            .unwrap();

        let preview = window
            .read_with(cx, |view, cx| view.command_preview(cx))
            .unwrap();
        assert!(preview.contains("--result-file=/tmp/custom.sql"));
    }

    #[gpui::test]
    async fn run_emits_request_with_current_fields(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|window, cx| {
            NativeDumpDialog::new(
                DatabaseDriver::MySQL,
                sample_config(),
                Vec::new(),
                Vec::new(),
                window,
                cx,
            )
        });
        let view = window.root(cx).unwrap();

        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        window
            .update(cx, |_, _window, cx| {
                let events = events.clone();
                cx.subscribe(&view, move |_, _, event: &NativeDumpEvent, _| {
                    if let NativeDumpEvent::Run(request) = event {
                        events.borrow_mut().push(request.clone());
                    }
                })
                .detach();
            })
            .unwrap();

        window
            .update(cx, |view, window, cx| {
                view.databases_editor
                    .update(cx, |editor, cx| editor.set_text("shop", window, cx));
                let request = view.build_request(cx);
                cx.emit(NativeDumpEvent::Run(request));
            })
            .unwrap();
        cx.run_until_parked();

        let captured = events.borrow();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].databases, vec!["shop".to_string()]);
        assert_eq!(captured[0].executable, "mysqldump");
    }

    #[gpui::test]
    async fn clicking_an_option_label_toggles_it(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|window, cx| {
            NativeDumpDialog::new(
                DatabaseDriver::MySQL,
                sample_config(),
                Vec::new(),
                Vec::new(),
                window,
                cx,
            )
        });

        let before = window
            .read_with(cx, |view, cx| view.command_preview(cx))
            .unwrap();
        assert!(
            before.contains("--add-drop-table"),
            "the first option defaults on: {before}"
        );

        let cx = &mut gpui::VisualTestContext::from_window(*window, cx);
        let bounds = cx
            .debug_bounds("DUMP_OPTION_0")
            .expect("the first dump option row should be rendered");
        // Click well to the right of the 20px checkbox box, over the label
        // text. This only toggles the option when the label is part of the
        // control's hit target rather than a detached sibling.
        let target = gpui::point(bounds.left() + px(100.), bounds.center().y);
        cx.simulate_click(target, gpui::Modifiers::none());

        let after = window
            .read_with(cx, |view, cx| view.command_preview(cx))
            .unwrap();
        assert!(
            !after.contains("--add-drop-table"),
            "clicking the option's label must toggle it off: {after}"
        );
    }

    #[gpui::test]
    async fn cancel_button_precedes_the_primary_run_button(cx: &mut TestAppContext) {
        init_test(cx);
        let window = cx.add_window(|window, cx| {
            NativeDumpDialog::new(
                DatabaseDriver::MySQL,
                sample_config(),
                Vec::new(),
                Vec::new(),
                window,
                cx,
            )
        });

        let cx = &mut gpui::VisualTestContext::from_window(*window, cx);
        let cancel = cx
            .debug_bounds("DUMP_CANCEL_BTN")
            .expect("the Cancel button should be rendered");
        let run = cx
            .debug_bounds("DUMP_RUN_BTN")
            .expect("the Run button should be rendered");
        assert!(
            cancel.left() < run.left(),
            "the primary Run button must sit to the right of Cancel: cancel_left={:?} run_left={:?}",
            cancel.left(),
            run.left()
        );
    }
}
