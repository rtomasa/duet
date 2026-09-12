use adw::prelude::*;
use gnome_briefcase::{
    BriefcaseService, ConflictResolution, EntryKind, PlannedOperation, ScanMode, SyncAction,
    SyncPlan,
};
use gtk::{gio, glib};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

const APP_ID: &str = "io.github.rtomasa.Briefcase";

#[derive(Default)]
struct UiState {
    briefcase_root: Option<PathBuf>,
    source_root: Option<PathBuf>,
    plan: Option<SyncPlan>,
    resolutions: BTreeMap<PathBuf, ConflictResolution>,
}

pub fn run() -> glib::ExitCode {
    let _ = gettextrs::setlocale(gettextrs::LocaleCategory::LcAll, "");
    let _ = gettextrs::bindtextdomain("gnome-briefcase", "/usr/share/locale");
    let _ = gettextrs::textdomain("gnome-briefcase");
    let app = adw::Application::builder().application_id(APP_ID).build();
    install_actions(&app);
    app.connect_activate(build_window);
    app.run()
}

fn build_window(app: &adw::Application) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Briefcase")
        .default_width(720)
        .default_height(620)
        .build();
    let toast_overlay = adw::ToastOverlay::new();
    let navigation = adw::NavigationView::new();
    toast_overlay.set_child(Some(&navigation));
    navigation.add(&home_page(&window, &navigation, &toast_overlay));
    window.set_content(Some(&toast_overlay));
    window.present();
}

fn home_page(
    window: &adw::ApplicationWindow,
    navigation: &adw::NavigationView,
    toasts: &adw::ToastOverlay,
) -> adw::NavigationPage {
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header_bar());
    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(24);
    content.set_margin_bottom(24);
    content.set_margin_start(24);
    content.set_margin_end(24);

    let status = adw::StatusPage::builder()
        .icon_name("io.github.rtomasa.Briefcase")
        .title("Briefcase")
        .description("Keep two folders synchronized explicitly and locally")
        .build();
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    actions.set_halign(gtk::Align::Center);
    let create = gtk::Button::with_mnemonic("_Create Briefcase");
    create.add_css_class("suggested-action");
    create.set_tooltip_text(Some("Choose a source and a portable destination"));
    let open = gtk::Button::with_mnemonic("_Open Briefcase");
    open.set_tooltip_text(Some("Open an existing Briefcase folder"));
    actions.append(&create);
    actions.append(&open);
    status.set_child(Some(&actions));
    content.append(&status);

    {
        let settings = settings();
        let group = adw::PreferencesGroup::builder()
            .title("Your Briefcases")
            .build();
        let mut added = false;
        for item in settings.strv("known-briefcases") {
            let root = PathBuf::from(item.as_str());
            let Ok(service) = BriefcaseService::open(&root) else {
                continue;
            };
            added = true;
            let row = adw::ActionRow::builder()
                .title(&service.manifest.name)
                .subtitle(root.to_string_lossy())
                .activatable(true)
                .build();
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            let nav = navigation.clone();
            let win = window.clone();
            let overlay = toasts.clone();
            row.connect_activated(move |_| open_briefcase(&win, &nav, &overlay, root.clone()));
            group.add(&row);
        }
        if added {
            content.append(&group);
        }
    }

    let nav = navigation.clone();
    let win = window.clone();
    let overlay = toasts.clone();
    open.connect_clicked(move |_| choose_existing(&win, &nav, &overlay));
    let nav = navigation.clone();
    let win = window.clone();
    let overlay = toasts.clone();
    create.connect_clicked(move |_| choose_source(&win, &nav, &overlay));

    toolbar.set_content(Some(&content));
    adw::NavigationPage::builder()
        .title("Briefcase")
        .child(&toolbar)
        .build()
}

fn choose_existing(
    window: &adw::ApplicationWindow,
    navigation: &adw::NavigationView,
    toasts: &adw::ToastOverlay,
) {
    let dialog = gtk::FileDialog::builder()
        .title("Open Briefcase")
        .modal(true)
        .build();
    let window = window.clone();
    let navigation = navigation.clone();
    let toasts = toasts.clone();
    glib::spawn_future_local(async move {
        if let Ok(folder) = dialog.select_folder_future(Some(&window)).await {
            if let Some(path) = folder.path() {
                open_briefcase(&window, &navigation, &toasts, path);
            }
        }
    });
}

fn choose_source(
    window: &adw::ApplicationWindow,
    navigation: &adw::NavigationView,
    toasts: &adw::ToastOverlay,
) {
    let dialog = gtk::FileDialog::builder()
        .title("Select Source Folder")
        .modal(true)
        .build();
    let window = window.clone();
    let navigation = navigation.clone();
    let toasts = toasts.clone();
    glib::spawn_future_local(async move {
        let Ok(folder) = dialog.select_folder_future(Some(&window)).await else {
            return;
        };
        let Some(source) = folder.path() else { return };
        let destination_dialog = gtk::FileDialog::builder()
            .title("Choose Briefcase Destination")
            .modal(true)
            .build();
        let Ok(destination) = destination_dialog.select_folder_future(Some(&window)).await else {
            return;
        };
        let Some(destination_parent) = destination.path() else {
            return;
        };
        let name = source
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Briefcase")
            .to_string();
        let root = destination_parent.join(&name);
        let source_for_task = source.clone();
        let root_for_task = root.clone();
        let name_for_task = name.clone();
        let result = gio::spawn_blocking(move || {
            BriefcaseService::create(&source_for_task, &root_for_task, &name_for_task)
        })
        .await;
        match result {
            Ok(Ok(_)) => open_briefcase(&window, &navigation, &toasts, root),
            Ok(Err(error)) => show_error(&toasts, &error.to_string()),
            Err(_) => show_error(&toasts, "The background operation stopped unexpectedly"),
        }
    });
}

fn open_briefcase(
    window: &adw::ApplicationWindow,
    navigation: &adw::NavigationView,
    toasts: &adw::ToastOverlay,
    root: PathBuf,
) {
    let service = match BriefcaseService::open(&root) {
        Ok(service) => service,
        Err(error) => {
            show_error(toasts, &error.to_string());
            return;
        }
    };
    remember(&root);
    let state = Rc::new(RefCell::new(UiState {
        briefcase_root: Some(root),
        source_root: Some(service.manifest.source.last_known_path.clone()),
        ..Default::default()
    }));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header_bar());
    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);

    let locations = adw::PreferencesGroup::builder()
        .title(&service.manifest.name)
        .build();
    let source_available = service.manifest.source.last_known_path.is_dir();
    let source_subtitle = if source_available {
        service
            .manifest
            .source
            .last_known_path
            .to_string_lossy()
            .to_string()
    } else {
        format!(
            "Source unavailable — {}",
            service.manifest.source.last_known_path.display()
        )
    };
    let source_row = adw::ActionRow::builder()
        .title("Source")
        .subtitle(source_subtitle)
        .build();
    locations.add(&source_row);
    locations.add(
        &adw::ActionRow::builder()
            .title("Briefcase")
            .subtitle(service.briefcase_root.to_string_lossy())
            .build(),
    );
    content.append(&locations);

    let summary = gtk::Label::new(Some("Ready to compare"));
    summary.set_xalign(0.0);
    summary.add_css_class("title-3");
    content.append(&summary);
    let changes = gtk::ListBox::new();
    changes.add_css_class("boxed-list");
    changes.set_selection_mode(gtk::SelectionMode::None);
    let scroller = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&changes)
        .build();
    content.append(&scroller);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    buttons.set_halign(gtk::Align::End);
    let compare = gtk::Button::with_mnemonic("Compare _Again");
    let sync = gtk::Button::with_mnemonic("_Update");
    sync.add_css_class("suggested-action");
    sync.set_sensitive(false);
    buttons.append(&compare);
    buttons.append(&sync);
    content.append(&buttons);
    toolbar.set_content(Some(&content));
    let page = adw::NavigationPage::builder()
        .title(&service.manifest.name)
        .child(&toolbar)
        .build();
    navigation.push(&page);

    let components = ViewComponents {
        window: window.clone(),
        toasts: toasts.clone(),
        state: state.clone(),
        summary: summary.clone(),
        list: changes.clone(),
        sync_button: sync.clone(),
    };
    let c = components.clone();
    compare.connect_clicked(move |_| run_compare(c.clone()));
    let c = components.clone();
    sync.connect_clicked(move |_| run_sync(c.clone()));
    if !source_available {
        let locate = gtk::Button::with_mnemonic("_Locate Source…");
        let c = components.clone();
        let row = source_row.clone();
        locate.connect_clicked(move |_| locate_source(c.clone(), row.clone()));
        source_row.add_suffix(&locate);
    }
    run_compare(components);
}

#[derive(Clone)]
struct ViewComponents {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    state: Rc<RefCell<UiState>>,
    summary: gtk::Label,
    list: gtk::ListBox,
    sync_button: gtk::Button,
}

fn run_compare(view: ViewComponents) {
    view.summary.set_text("Comparing folders…");
    view.sync_button.set_sensitive(false);
    let Some(root) = view.state.borrow().briefcase_root.clone() else {
        return;
    };
    let mode = if settings().string("change-detection-mode") == "verified" {
        ScanMode::Verified
    } else {
        ScanMode::Fast
    };
    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(move || {
            let service = BriefcaseService::open(&root)?;
            service.compare(mode)
        })
        .await;
        match result {
            Ok(Ok(plan)) => render_plan(&view, plan),
            Ok(Err(error)) => {
                view.summary.set_text("Comparison failed");
                show_error(&view.toasts, &error.to_string());
            }
            Err(_) => {
                view.summary.set_text("Comparison failed");
                show_error(
                    &view.toasts,
                    "The background operation stopped unexpectedly",
                );
            }
        }
    });
}

fn render_plan(view: &ViewComponents, plan: SyncPlan) {
    while let Some(child) = view.list.first_child() {
        view.list.remove(&child);
    }
    view.state.borrow_mut().resolutions.clear();
    let changes = plan.actionable_count();
    let conflicts = plan.conflicts.len();
    view.summary.set_text(&format!(
        "{} {} · {} {}",
        changes,
        if changes == 1 { "change" } else { "changes" },
        conflicts,
        if conflicts == 1 {
            "conflict"
        } else {
            "conflicts"
        },
    ));
    for op in plan
        .operations
        .iter()
        .filter(|op| op.action != SyncAction::None)
    {
        view.list.append(&operation_row(op));
    }
    for conflict in &plan.conflicts {
        view.list.append(&conflict_row(view, conflict));
    }
    if changes == 0 && conflicts == 0 {
        let row = adw::ActionRow::builder()
            .title("Synchronized")
            .subtitle("No changes found")
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("emblem-ok-symbolic"));
        view.list.append(&row);
    }
    view.sync_button.set_label(if conflicts > 0 {
        "Update Non-conflicting Files"
    } else {
        "Update"
    });
    view.sync_button.set_sensitive(changes > 0);
    view.state.borrow_mut().plan = Some(plan);
}

fn operation_row(op: &PlannedOperation) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(op.relative_path.to_string_lossy())
        .subtitle(action_label(op.action))
        .build();
    row.add_prefix(&gtk::Image::from_icon_name(match op.kind {
        EntryKind::File => "text-x-generic-symbolic",
        EntryKind::Directory => "folder-symbolic",
    }));
    row
}

fn conflict_row(view: &ViewComponents, conflict: &gnome_briefcase::Conflict) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(conflict.operation.relative_path.to_string_lossy())
        .subtitle("Both copies changed — skipped until you choose")
        .build();
    row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
    let source_deleted = conflict.source.is_none();
    let briefcase_deleted = conflict.briefcase.is_none();
    if !source_deleted {
        add_resolution_button(
            &row,
            view,
            &conflict.operation.relative_path,
            "Keep Source",
            "Will keep Source",
            ConflictResolution::KeepSource,
        );
        add_open_button(
            &row,
            view.state.borrow().source_root.as_deref(),
            &conflict.operation.relative_path,
            "Open Source Copy",
        );
    }
    if !briefcase_deleted {
        add_resolution_button(
            &row,
            view,
            &conflict.operation.relative_path,
            "Keep Briefcase",
            "Will keep Briefcase",
            ConflictResolution::KeepBriefcase,
        );
        add_open_button(
            &row,
            view.state.borrow().briefcase_root.as_deref(),
            &conflict.operation.relative_path,
            "Open Briefcase Copy",
        );
    }
    if source_deleted || briefcase_deleted {
        add_resolution_button(
            &row,
            view,
            &conflict.operation.relative_path,
            "Accept Deletion",
            "Will accept deletion",
            ConflictResolution::AcceptDeletion,
        );
    }
    row
}

fn add_resolution_button(
    row: &adw::ActionRow,
    view: &ViewComponents,
    path: &Path,
    label: &str,
    result_label: &'static str,
    resolution: ConflictResolution,
) {
    let button = gtk::Button::with_label(label);
    let path = path.to_path_buf();
    let state = view.state.clone();
    let row_copy = row.clone();
    button.connect_clicked(move |_| {
        state
            .borrow_mut()
            .resolutions
            .insert(path.clone(), resolution);
        row_copy.set_subtitle(result_label);
    });
    row.add_suffix(&button);
}

fn add_open_button(row: &adw::ActionRow, root: Option<&Path>, relative: &Path, tooltip: &str) {
    let Some(root) = root else { return };
    let path = root.join(relative);
    let button = gtk::Button::builder()
        .icon_name("document-open-symbolic")
        .tooltip_text(tooltip)
        .build();
    button.connect_clicked(move |_| {
        let file = gio::File::for_path(&path);
        let _ = gio::AppInfo::launch_default_for_uri(
            file.uri().as_str(),
            None::<&gio::AppLaunchContext>,
        );
    });
    row.add_suffix(&button);
}

fn run_sync(view: ViewComponents) {
    let plan = view.state.borrow().plan.clone();
    let root = view.state.borrow().briefcase_root.clone();
    let resolutions = view.state.borrow().resolutions.clone();
    let (Some(plan), Some(root)) = (plan, root) else {
        return;
    };
    let has_deletions = plan.has_deletions();
    let perform = move |view: ViewComponents| {
        view.summary.set_text("Updating…");
        view.sync_button.set_sensitive(false);
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || {
                let service = BriefcaseService::open(&root)?;
                service.synchronize(&plan, &resolutions)
            })
            .await;
            match result {
                Ok(Ok(outcome)) => {
                    view.toasts.add_toast(adw::Toast::new(&format!(
                        "Updated {} items",
                        outcome.applied
                    )));
                    run_compare(view);
                }
                Ok(Err(error)) => {
                    view.summary.set_text("Update failed");
                    show_error(&view.toasts, &error.to_string());
                }
                Err(_) => {
                    view.summary.set_text("Update failed");
                    show_error(
                        &view.toasts,
                        "The background operation stopped unexpectedly",
                    );
                }
            }
        });
    };
    if has_deletions && settings().boolean("confirm-deletions") {
        let dialog = adw::AlertDialog::new(Some("Confirm deletions"), Some("This update removes files from one of the folders. Deleted files cannot be restored by Briefcase."));
        dialog.add_responses(&[("cancel", "Cancel"), ("update", "Update")]);
        dialog.set_response_appearance("update", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        let view_copy = view.clone();
        glib::spawn_future_local(async move {
            if dialog.choose_future(&view_copy.window).await == "update" {
                perform(view_copy);
            }
        });
    } else {
        perform(view);
    }
}

fn locate_source(view: ViewComponents, row: adw::ActionRow) {
    let dialog = gtk::FileDialog::builder()
        .title("Locate Source Folder")
        .modal(true)
        .build();
    glib::spawn_future_local(async move {
        let Ok(folder) = dialog.select_folder_future(Some(&view.window)).await else {
            return;
        };
        let Some(source) = folder.path() else {
            return;
        };
        let Some(root) = view.state.borrow().briefcase_root.clone() else {
            return;
        };
        let source_for_task = source.clone();
        let result = gio::spawn_blocking(move || {
            let mut service = BriefcaseService::open(&root)?;
            service.rebind_source(&source_for_task)
        })
        .await;
        match result {
            Ok(Ok(())) => {
                view.state.borrow_mut().source_root = Some(source.clone());
                row.set_subtitle(&source.to_string_lossy());
                run_compare(view);
            }
            Ok(Err(error)) => show_error(&view.toasts, &error.to_string()),
            Err(_) => show_error(
                &view.toasts,
                "The background operation stopped unexpectedly",
            ),
        }
    });
}

fn action_label(action: SyncAction) -> &'static str {
    match action {
        SyncAction::SourceToBriefcase => "Changed in Source → Briefcase",
        SyncAction::BriefcaseToSource => "Changed in Briefcase → Source",
        SyncAction::DeleteSource => "Deleted in Briefcase → delete from Source",
        SyncAction::DeleteBriefcase => "Deleted in Source → delete from Briefcase",
        SyncAction::RemoveBaseline => "Deleted from both copies",
        SyncAction::Adopt => "Equal content on both sides",
        SyncAction::Conflict => "Conflict",
        SyncAction::None => "Synchronized",
    }
}

fn settings() -> gio::Settings {
    gio::Settings::new(APP_ID)
}

fn remember(root: &Path) {
    let settings = settings();
    let mut values: Vec<String> = settings
        .strv("known-briefcases")
        .iter()
        .map(|s| s.to_string())
        .collect();
    let value = root.to_string_lossy().to_string();
    if !values.contains(&value) {
        values.push(value);
    }
    let refs: Vec<&str> = values.iter().map(String::as_str).collect();
    let _ = settings.set_strv("known-briefcases", refs);
}

fn header_bar() -> adw::HeaderBar {
    let header = adw::HeaderBar::new();
    let menu = gio::Menu::new();
    menu.append(Some("Preferences"), Some("app.preferences"));
    menu.append(Some("About Briefcase"), Some("app.about"));
    let button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text("Main Menu")
        .build();
    header.pack_end(&button);
    header
}

fn install_actions(app: &adw::Application) {
    let preferences = gio::SimpleAction::new("preferences", None);
    preferences.connect_activate(glib::clone!(
        #[weak]
        app,
        move |_, _| {
            let Some(window) = app.active_window() else {
                return;
            };
            let dialog = adw::PreferencesDialog::new();
            let page = adw::PreferencesPage::new();
            let group = adw::PreferencesGroup::builder()
                .title("Synchronization")
                .build();
            let settings = settings();
            let confirm = adw::SwitchRow::builder()
                .title("Confirm deletions")
                .subtitle("Ask before an update removes files")
                .active(settings.boolean("confirm-deletions"))
                .build();
            let settings_copy = settings.clone();
            confirm.connect_active_notify(move |row| {
                let _ = settings_copy.set_boolean("confirm-deletions", row.is_active());
            });
            let modes = gtk::StringList::new(&["Fast", "Verified"]);
            let detection = adw::ComboRow::builder()
                .title("Change detection")
                .subtitle("Verified hashes every file")
                .model(&modes)
                .selected(if settings.string("change-detection-mode") == "verified" {
                    1
                } else {
                    0
                })
                .build();
            detection.connect_selected_notify(move |row| {
                let value = if row.selected() == 1 {
                    "verified"
                } else {
                    "fast"
                };
                let _ = settings.set_string("change-detection-mode", value);
            });
            group.add(&confirm);
            group.add(&detection);
            page.add(&group);
            dialog.add(&page);
            dialog.present(Some(&window));
        }
    ));
    app.add_action(&preferences);

    let about = gio::SimpleAction::new("about", None);
    about.connect_activate(glib::clone!(
        #[weak]
        app,
        move |_, _| {
            let Some(window) = app.active_window() else {
                return;
            };
            let dialog = adw::AboutDialog::builder()
                .application_name("Briefcase")
                .application_icon(APP_ID)
                .version("0.1.0")
                .developer_name("GNOME Briefcase contributors")
                .license_type(gtk::License::Gpl30)
                .website("https://github.com/rtomasa/gnome-briefcase")
                .build();
            dialog.present(Some(&window));
        }
    ));
    app.add_action(&about);

    app.set_accels_for_action("app.preferences", &["<primary>comma"]);
    app.set_accels_for_action("app.quit", &["<primary>q"]);
}

fn show_error(toasts: &adw::ToastOverlay, message: &str) {
    let toast = adw::Toast::new(message);
    toast.set_timeout(8);
    toasts.add_toast(toast);
}
