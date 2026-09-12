use adw::prelude::*;
use gettextrs::{gettext, ngettext};
use gnome_briefcase::{
    BriefcaseError, BriefcaseService, ConflictResolution, EntryKind, PlannedOperation, ScanMode,
    SyncAction, SyncPlan,
};
use gtk::{gio, glib};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

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
    let locale_dir = option_env!("GNOME_BRIEFCASE_LOCALEDIR").unwrap_or("/usr/share/locale");
    let _ = gettextrs::bindtextdomain("gnome-briefcase", locale_dir);
    let _ = gettextrs::textdomain("gnome-briefcase");
    let app = adw::Application::builder().application_id(APP_ID).build();
    install_actions(&app);
    app.connect_activate(build_window);
    app.run()
}

fn build_window(app: &adw::Application) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(&gettext("Briefcase"))
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
        .title(&gettext("Briefcase"))
        .description(&gettext(
            "Keep two folders synchronized explicitly and locally",
        ))
        .build();
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    actions.set_halign(gtk::Align::Center);
    let create = gtk::Button::with_mnemonic(&gettext("_Create Briefcase"));
    create.add_css_class("suggested-action");
    create.set_tooltip_text(Some(&gettext("Choose a source and a portable destination")));
    let open = gtk::Button::with_mnemonic(&gettext("_Open Briefcase"));
    open.set_tooltip_text(Some(&gettext("Open an existing Briefcase folder")));
    actions.append(&create);
    actions.append(&open);
    status.set_child(Some(&actions));
    content.append(&status);

    let group = adw::PreferencesGroup::builder()
        .title(&gettext("Your Briefcases"))
        .description(&gettext(
            "Briefcases stay listed even when a removable drive is disconnected",
        ))
        .build();
    content.append(&group);
    let known_rows = Rc::new(RefCell::new(Vec::new()));
    populate_known_briefcases(&group, &known_rows, window, navigation, toasts);
    let settings = settings();
    let settings_lifetime = settings.clone();
    let group_copy = group.clone();
    let rows_copy = known_rows.clone();
    let win = window.clone();
    let nav = navigation.clone();
    let overlay = toasts.clone();
    settings.connect_changed(Some("known-briefcases"), move |_, _| {
        let _keep_alive = &settings_lifetime;
        populate_known_briefcases(&group_copy, &rows_copy, &win, &nav, &overlay);
    });

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
        .title(&gettext("Briefcase"))
        .child(&toolbar)
        .build()
}

fn populate_known_briefcases(
    group: &adw::PreferencesGroup,
    rows: &Rc<RefCell<Vec<adw::ActionRow>>>,
    window: &adw::ApplicationWindow,
    navigation: &adw::NavigationView,
    toasts: &adw::ToastOverlay,
) {
    for row in rows.borrow_mut().drain(..) {
        group.remove(&row);
    }

    for item in settings().strv("known-briefcases") {
        let root = PathBuf::from(item.as_str());
        let available = root.is_dir();
        let service = available
            .then(|| BriefcaseService::open(&root).ok())
            .flatten();
        let name = service
            .as_ref()
            .map(|service| service.manifest.name.clone())
            .or_else(|| {
                root.file_name()
                    .and_then(|name| name.to_str())
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| gettext("Briefcase"));
        let subtitle = if available {
            root.to_string_lossy().to_string()
        } else {
            format!(
                "{} — {}",
                gettext("Folder unavailable"),
                root.to_string_lossy()
            )
        };
        let row = adw::ActionRow::builder()
            .title(&name)
            .subtitle(&subtitle)
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name(if available {
            "folder-symbolic"
        } else {
            "dialog-warning-symbolic"
        }));

        let remove = gtk::Button::builder()
            .icon_name("edit-delete-symbolic")
            .tooltip_text(&gettext("Remove from the list"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let root_to_remove = root.clone();
        remove.connect_clicked(move |_| forget(&root_to_remove));
        row.add_suffix(&remove);
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));

        let nav = navigation.clone();
        let win = window.clone();
        let overlay = toasts.clone();
        row.connect_activated(move |_| {
            if root.is_dir() {
                open_briefcase(&win, &nav, &overlay, root.clone());
            } else {
                confirm_remove_unavailable(&win, root.clone());
            }
        });
        group.add(&row);
        rows.borrow_mut().push(row);
    }
}

fn confirm_remove_unavailable(window: &adw::ApplicationWindow, root: PathBuf) {
    let message = gettext(
        "The folder {path} is not available. It may be on a disconnected drive. Do you want to remove it from the list?",
    )
    .replace("{path}", &root.to_string_lossy());
    let dialog = adw::AlertDialog::new(Some(&gettext("Briefcase unavailable")), Some(&message));
    dialog.add_responses(&[
        ("keep", &gettext("Keep")),
        ("remove", &gettext("Remove from List")),
    ]);
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("keep"));
    let window = window.clone();
    glib::spawn_future_local(async move {
        if dialog.choose_future(&window).await == "remove" {
            forget(&root);
        }
    });
}

fn choose_existing(
    window: &adw::ApplicationWindow,
    navigation: &adw::NavigationView,
    toasts: &adw::ToastOverlay,
) {
    let dialog = gtk::FileDialog::builder()
        .title(&gettext("Open Briefcase"))
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
        .title(&gettext("Select Source Folder"))
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
            .title(&gettext("Choose Briefcase Destination"))
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
            Ok(Err(error)) => show_error(&toasts, &localized_error(&error)),
            Err(_) => show_error(
                &toasts,
                &gettext("The background operation stopped unexpectedly"),
            ),
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
            show_error(toasts, &localized_error(&error));
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
            "{} — {}",
            gettext("Source unavailable"),
            service.manifest.source.last_known_path.display()
        )
    };
    let source_row = adw::ActionRow::builder()
        .title(&gettext("Source"))
        .subtitle(source_subtitle)
        .build();
    locations.add(&source_row);
    locations.add(
        &adw::ActionRow::builder()
            .title(&gettext("Briefcase"))
            .subtitle(service.briefcase_root.to_string_lossy())
            .build(),
    );
    content.append(&locations);

    let summary = gtk::Label::new(Some(&gettext("Ready to check")));
    summary.set_xalign(0.0);
    summary.add_css_class("title-3");
    content.append(&summary);
    let check_progress = gtk::ProgressBar::builder()
        .show_text(true)
        .visible(false)
        .build();
    content.append(&check_progress);
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
    let compare = gtk::Button::with_mnemonic(&gettext("_Check Again"));
    let stop = gtk::Button::with_mnemonic(&gettext("_Stop"));
    stop.add_css_class("destructive-action");
    stop.set_visible(false);
    let sync = gtk::Button::with_mnemonic(&gettext("_Synchronize"));
    sync.add_css_class("suggested-action");
    sync.set_sensitive(false);
    buttons.append(&compare);
    buttons.append(&stop);
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
        check_progress: check_progress.clone(),
        list: changes.clone(),
        compare_button: compare.clone(),
        sync_button: sync.clone(),
        stop_button: stop.clone(),
        cancellation: Rc::new(RefCell::new(None)),
        operation_rows: Rc::new(RefCell::new(BTreeMap::new())),
    };
    let c = components.clone();
    compare.connect_clicked(move |_| run_compare(c.clone()));
    let c = components.clone();
    sync.connect_clicked(move |_| run_sync(c.clone()));
    let c = components.clone();
    stop.connect_clicked(move |_| request_stop(&c));
    if !source_available {
        let locate = gtk::Button::with_mnemonic(&gettext("_Locate Source…"));
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
    check_progress: gtk::ProgressBar,
    list: gtk::ListBox,
    compare_button: gtk::Button,
    sync_button: gtk::Button,
    stop_button: gtk::Button,
    cancellation: Rc<RefCell<Option<Arc<AtomicBool>>>>,
    operation_rows: Rc<RefCell<BTreeMap<PathBuf, OperationWidgets>>>,
}

#[derive(Clone)]
struct OperationWidgets {
    row: adw::ActionRow,
    progress: gtk::ProgressBar,
}

struct SyncProgressEvent {
    path: PathBuf,
    completed: u64,
    total: u64,
    finished: bool,
}

struct ScanProgressEvent {
    completed: u64,
    total: u64,
}

fn run_compare(view: ViewComponents) {
    finish_sync_controls(&view);
    view.summary.set_text(&gettext("Checking folders…"));
    view.check_progress.set_fraction(0.0);
    view.check_progress.set_text(None);
    view.check_progress.set_visible(true);
    view.compare_button.set_sensitive(false);
    view.sync_button.set_sensitive(false);
    let Some(root) = view.state.borrow().briefcase_root.clone() else {
        return;
    };
    let mode = if settings().string("change-detection-mode") == "verified" {
        ScanMode::Verified
    } else {
        ScanMode::Fast
    };
    let cancellation = Arc::new(AtomicBool::new(false));
    *view.cancellation.borrow_mut() = Some(cancellation.clone());
    view.stop_button.set_label(&gettext("Stop"));
    view.stop_button.set_sensitive(true);
    view.stop_button.set_visible(true);
    let (sender, receiver) = mpsc::channel::<ScanProgressEvent>();
    let progress_view = view.clone();
    let progress_source = glib::timeout_add_local(Duration::from_millis(50), move || {
        while let Ok(event) = receiver.try_recv() {
            update_scan_progress(&progress_view, event);
        }
        glib::ControlFlow::Continue
    });
    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(move || {
            let service = BriefcaseService::open(&root)?;
            service.compare_with_progress_and_cancel(
                mode,
                move |completed, total| {
                    let _ = sender.send(ScanProgressEvent { completed, total });
                },
                move || cancellation.load(Ordering::Relaxed),
            )
        })
        .await;
        progress_source.remove();
        finish_sync_controls(&view);
        match result {
            Ok(Ok(plan)) => render_plan(&view, plan),
            Ok(Err(BriefcaseError::Cancelled)) => {
                view.summary.set_text(&gettext("Ready to check"));
            }
            Ok(Err(error)) => {
                view.summary.set_text(&gettext("Check failed"));
                show_error(&view.toasts, &localized_error(&error));
            }
            Err(_) => {
                view.summary.set_text(&gettext("Check failed"));
                show_error(
                    &view.toasts,
                    &gettext("The background operation stopped unexpectedly"),
                );
            }
        }
    });
}

fn request_stop(view: &ViewComponents) {
    let Some(cancellation) = view.cancellation.borrow().as_ref().cloned() else {
        return;
    };
    cancellation.store(true, Ordering::Relaxed);
    view.stop_button.set_sensitive(false);
    view.stop_button.set_label(&gettext("Stopping…"));
    view.summary.set_text(&gettext("Stopping…"));
}

fn finish_sync_controls(view: &ViewComponents) {
    view.cancellation.borrow_mut().take();
    view.stop_button.set_visible(false);
    view.stop_button.set_sensitive(true);
    view.stop_button.set_label(&gettext("Stop"));
    view.compare_button.set_sensitive(true);
    view.check_progress.set_visible(false);
    view.check_progress.set_fraction(0.0);
    view.check_progress.set_text(None);
}

fn update_scan_progress(view: &ViewComponents, event: ScanProgressEvent) {
    if event.total == 0 {
        view.check_progress.set_fraction(0.0);
        view.check_progress.pulse();
        view.check_progress
            .set_text(Some(&event.completed.to_string()));
        return;
    }
    let fraction = (event.completed as f64 / event.total as f64).clamp(0.0, 1.0);
    view.check_progress
        .set_fraction(view.check_progress.fraction().max(fraction));
    view.check_progress
        .set_text(Some(&format!("{}%", (fraction * 100.0).round() as u32)));
}

fn render_plan(view: &ViewComponents, plan: SyncPlan) {
    while let Some(child) = view.list.first_child() {
        view.list.remove(&child);
    }
    view.operation_rows.borrow_mut().clear();
    view.state.borrow_mut().resolutions.clear();
    let changes = plan.actionable_count();
    let conflicts = plan.conflicts.len();
    let changes_label = ngettext("{count} change", "{count} changes", changes as u32)
        .replace("{count}", &changes.to_string());
    let conflicts_label = ngettext("{count} conflict", "{count} conflicts", conflicts as u32)
        .replace("{count}", &conflicts.to_string());
    view.summary
        .set_text(&format!("{changes_label} · {conflicts_label}"));
    for op in plan
        .operations
        .iter()
        .filter(|op| op.action != SyncAction::None)
    {
        let widgets = operation_row(op);
        view.list.append(&widgets.row);
        view.operation_rows
            .borrow_mut()
            .insert(op.relative_path.clone(), widgets);
    }
    for conflict in &plan.conflicts {
        let widgets = conflict_row(view, conflict);
        view.list.append(&widgets.row);
        view.operation_rows
            .borrow_mut()
            .insert(conflict.operation.relative_path.clone(), widgets);
    }
    if changes == 0 && conflicts == 0 {
        let row = adw::ActionRow::builder()
            .title(&gettext("Synchronized"))
            .subtitle(&gettext("No changes found"))
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("emblem-ok-symbolic"));
        view.list.append(&row);
    }
    let sync_label = if conflicts > 0 {
        gettext("Synchronize Non-conflicting Files")
    } else {
        gettext("Synchronize")
    };
    view.sync_button.set_label(&sync_label);
    view.sync_button.set_sensitive(changes > 0 || conflicts > 0);
    view.state.borrow_mut().plan = Some(plan);
}

fn operation_row(op: &PlannedOperation) -> OperationWidgets {
    let row = adw::ActionRow::builder()
        .title(op.relative_path.to_string_lossy())
        .subtitle(action_label(op.action))
        .build();
    row.add_prefix(&gtk::Image::from_icon_name(match op.kind {
        EntryKind::File => "text-x-generic-symbolic",
        EntryKind::Directory => "folder-symbolic",
    }));
    let progress = operation_progress_bar();
    row.add_suffix(&progress);
    OperationWidgets { row, progress }
}

fn conflict_row(view: &ViewComponents, conflict: &gnome_briefcase::Conflict) -> OperationWidgets {
    let row = adw::ActionRow::builder()
        .title(conflict.operation.relative_path.to_string_lossy())
        .subtitle(&gettext("Both copies changed — skipped until you choose"))
        .build();
    row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
    let source_deleted = conflict.source.is_none();
    let briefcase_deleted = conflict.briefcase.is_none();
    if !source_deleted {
        add_resolution_button(
            &row,
            view,
            &conflict.operation.relative_path,
            &gettext("Keep Source"),
            gettext("Will keep Source"),
            ConflictResolution::KeepSource,
        );
        add_open_button(
            &row,
            view.state.borrow().source_root.as_deref(),
            &conflict.operation.relative_path,
            &gettext("Open Source Copy"),
        );
    }
    if !briefcase_deleted {
        add_resolution_button(
            &row,
            view,
            &conflict.operation.relative_path,
            &gettext("Keep Briefcase"),
            gettext("Will keep Briefcase"),
            ConflictResolution::KeepBriefcase,
        );
        add_open_button(
            &row,
            view.state.borrow().briefcase_root.as_deref(),
            &conflict.operation.relative_path,
            &gettext("Open Briefcase Copy"),
        );
    }
    if source_deleted || briefcase_deleted {
        add_resolution_button(
            &row,
            view,
            &conflict.operation.relative_path,
            &gettext("Accept Deletion"),
            gettext("Will accept deletion"),
            ConflictResolution::AcceptDeletion,
        );
    }
    let progress = operation_progress_bar();
    row.add_suffix(&progress);
    OperationWidgets { row, progress }
}

fn operation_progress_bar() -> gtk::ProgressBar {
    gtk::ProgressBar::builder()
        .width_request(140)
        .valign(gtk::Align::Center)
        .visible(false)
        .show_text(true)
        .build()
}

fn add_resolution_button(
    row: &adw::ActionRow,
    view: &ViewComponents,
    path: &Path,
    label: &str,
    result_label: String,
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
        row_copy.set_subtitle(&result_label);
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
    glib::spawn_future_local(async move {
        if let Some(plan) = prepare_deletions(&view, plan).await {
            perform_sync(view, plan, root, resolutions);
        }
    });
}

#[derive(Clone, Copy)]
enum DeletionChoice {
    Skip,
    Restore,
    Delete,
}

async fn prepare_deletions(view: &ViewComponents, mut plan: SyncPlan) -> Option<SyncPlan> {
    let policy = settings().string("deletion-action");
    let fixed_choice = match policy.as_str() {
        "skip" => Some(DeletionChoice::Skip),
        "restore" => Some(DeletionChoice::Restore),
        "delete" => Some(DeletionChoice::Delete),
        _ => None,
    };
    let mut choice_for_remaining = fixed_choice;
    for operation in plan.operations.iter_mut().filter(|operation| {
        matches!(
            operation.action,
            SyncAction::DeleteSource | SyncAction::DeleteBriefcase
        )
    }) {
        let choice = if let Some(choice) = choice_for_remaining {
            choice
        } else {
            let (choice, do_not_ask) = ask_deletion_action(&view.window, operation).await?;
            if do_not_ask {
                choice_for_remaining = Some(choice);
            }
            choice
        };
        apply_deletion_choice(operation, choice);
    }
    Some(plan)
}

async fn ask_deletion_action(
    window: &adw::ApplicationWindow,
    operation: &PlannedOperation,
) -> Option<(DeletionChoice, bool)> {
    let location = if operation.action == SyncAction::DeleteBriefcase {
        gettext("Source")
    } else {
        gettext("Briefcase")
    };
    let message =
        gettext("{path} was deleted from {location}. Choose what to do with the remaining copy.")
            .replace("{path}", &operation.relative_path.to_string_lossy())
            .replace("{location}", &location);
    let dialog = adw::AlertDialog::new(Some(&gettext("File Deleted")), Some(&message));
    dialog.add_responses(&[
        ("skip", &gettext("Skip")),
        ("restore", &gettext("Restore Deleted File")),
        ("delete", &gettext("Delete Other Copy")),
    ]);
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("skip"));
    dialog.set_close_response("cancel");
    let do_not_ask =
        gtk::CheckButton::with_label(&gettext("Do not ask again for remaining deletions"));
    dialog.set_extra_child(Some(&do_not_ask));
    let response = dialog.choose_future(window).await;
    let choice = match response.as_str() {
        "skip" => DeletionChoice::Skip,
        "restore" => DeletionChoice::Restore,
        "delete" => DeletionChoice::Delete,
        _ => return None,
    };
    Some((choice, do_not_ask.is_active()))
}

fn apply_deletion_choice(operation: &mut PlannedOperation, choice: DeletionChoice) {
    operation.action = match (choice, operation.action) {
        (DeletionChoice::Skip, _) => SyncAction::None,
        (DeletionChoice::Restore, SyncAction::DeleteBriefcase) => SyncAction::BriefcaseToSource,
        (DeletionChoice::Restore, SyncAction::DeleteSource) => SyncAction::SourceToBriefcase,
        (DeletionChoice::Delete, action) | (DeletionChoice::Restore, action) => action,
    };
}

fn perform_sync(
    view: ViewComponents,
    plan: SyncPlan,
    root: PathBuf,
    resolutions: BTreeMap<PathBuf, ConflictResolution>,
) {
    view.summary.set_text(&gettext("Synchronizing…"));
    view.compare_button.set_sensitive(false);
    view.sync_button.set_sensitive(false);
    view.stop_button.set_label(&gettext("Stop"));
    view.stop_button.set_sensitive(true);
    view.stop_button.set_visible(true);
    let cancellation = Arc::new(AtomicBool::new(false));
    *view.cancellation.borrow_mut() = Some(cancellation.clone());
    let (sender, receiver) = mpsc::channel::<SyncProgressEvent>();
    let progress_view = view.clone();
    let progress_source = glib::timeout_add_local(Duration::from_millis(50), move || {
        while let Ok(event) = receiver.try_recv() {
            update_operation_progress(&progress_view, event);
        }
        glib::ControlFlow::Continue
    });
    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(move || {
            let service = BriefcaseService::open(&root)?;
            service.synchronize_with_progress_and_cancel(
                &plan,
                &resolutions,
                move |operation, completed, total, finished| {
                    let _ = sender.send(SyncProgressEvent {
                        path: operation.relative_path.clone(),
                        completed,
                        total,
                        finished,
                    });
                },
                move || cancellation.load(Ordering::Relaxed),
            )
        })
        .await;
        progress_source.remove();
        finish_sync_controls(&view);
        match result {
            Ok(Ok(outcome)) => {
                let message = ngettext(
                    "Synchronized {count} item",
                    "Synchronized {count} items",
                    outcome.applied as u32,
                )
                .replace("{count}", &outcome.applied.to_string());
                view.toasts.add_toast(adw::Toast::new(&message));
                run_compare(view);
            }
            Ok(Err(BriefcaseError::Cancelled)) => {
                view.toasts
                    .add_toast(adw::Toast::new(&gettext("Synchronization stopped")));
                run_compare(view);
            }
            Ok(Err(error)) => {
                view.summary.set_text(&gettext("Synchronization failed"));
                show_error(&view.toasts, &localized_error(&error));
            }
            Err(_) => {
                view.summary.set_text(&gettext("Synchronization failed"));
                show_error(
                    &view.toasts,
                    &gettext("The background operation stopped unexpectedly"),
                );
            }
        }
    });
}

fn update_operation_progress(view: &ViewComponents, event: SyncProgressEvent) {
    let widgets = view.operation_rows.borrow().get(&event.path).cloned();
    let Some(widgets) = widgets else { return };
    if event.finished {
        view.list.remove(&widgets.row);
        view.operation_rows.borrow_mut().remove(&event.path);
        return;
    }
    widgets.progress.set_visible(true);
    if event.total == 0 {
        widgets.progress.pulse();
        widgets.progress.set_text(Some(&gettext("Working…")));
    } else if event.completed >= event.total {
        widgets.progress.set_fraction(0.9);
        widgets.progress.set_text(Some(&gettext("Finalizing…")));
    } else {
        let fraction = event.completed as f64 / event.total as f64;
        widgets.progress.set_fraction(fraction * 0.9);
        widgets
            .progress
            .set_text(Some(&format!("{}%", (fraction * 90.0).round() as u32)));
    }
}

fn locate_source(view: ViewComponents, row: adw::ActionRow) {
    let dialog = gtk::FileDialog::builder()
        .title(&gettext("Locate Source Folder"))
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
            Ok(Err(error)) => show_error(&view.toasts, &localized_error(&error)),
            Err(_) => show_error(
                &view.toasts,
                &gettext("The background operation stopped unexpectedly"),
            ),
        }
    });
}

fn action_label(action: SyncAction) -> String {
    match action {
        SyncAction::SourceToBriefcase => gettext("Changed in Source → Briefcase"),
        SyncAction::BriefcaseToSource => gettext("Changed in Briefcase → Source"),
        SyncAction::DeleteSource => gettext("Deleted in Briefcase → delete from Source"),
        SyncAction::DeleteBriefcase => gettext("Deleted in Source → delete from Briefcase"),
        SyncAction::RemoveBaseline => gettext("Deleted from both copies"),
        SyncAction::Adopt => gettext("Equal content on both sides"),
        SyncAction::Conflict => gettext("Conflict"),
        SyncAction::None => gettext("Synchronized"),
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

fn forget(root: &Path) {
    let settings = settings();
    let value = root.to_string_lossy();
    let values: Vec<String> = settings
        .strv("known-briefcases")
        .iter()
        .filter(|item| item.as_str() != value)
        .map(|item| item.to_string())
        .collect();
    let refs: Vec<&str> = values.iter().map(String::as_str).collect();
    let _ = settings.set_strv("known-briefcases", refs);
}

fn header_bar() -> adw::HeaderBar {
    let header = adw::HeaderBar::new();
    let menu = gio::Menu::new();
    menu.append(Some(&gettext("Preferences")), Some("app.preferences"));
    menu.append(Some(&gettext("Help")), Some("app.help"));
    menu.append(Some(&gettext("About Briefcase")), Some("app.about"));
    let button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text(&gettext("Main Menu"))
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
                .title(&gettext("Synchronization"))
                .build();
            let settings = settings();
            let ask = gettext("Ask Every Time");
            let skip = gettext("Skip");
            let restore = gettext("Restore Deleted File");
            let delete = gettext("Delete Other Copy");
            let deletion_actions = gtk::StringList::new(&[&ask, &skip, &restore, &delete]);
            let deletion = adw::ComboRow::builder()
                .title(&gettext("When a file is deleted"))
                .subtitle(&gettext("Choose the default synchronization action"))
                .model(&deletion_actions)
                .selected(match settings.string("deletion-action").as_str() {
                    "skip" => 1,
                    "restore" => 2,
                    "delete" => 3,
                    _ => 0,
                })
                .build();
            let settings_copy = settings.clone();
            deletion.connect_selected_notify(move |row| {
                let value = match row.selected() {
                    1 => "skip",
                    2 => "restore",
                    3 => "delete",
                    _ => "ask",
                };
                let _ = settings_copy.set_string("deletion-action", value);
            });
            let fast = gettext("Fast");
            let verified = gettext("Verified");
            let modes = gtk::StringList::new(&[&fast, &verified]);
            let detection = adw::ComboRow::builder()
                .title(&gettext("Change detection"))
                .subtitle(&gettext("Verified mode hashes every file"))
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
            group.add(&deletion);
            group.add(&detection);
            page.add(&group);
            dialog.add(&page);
            dialog.present(Some(&window));
        }
    ));
    app.add_action(&preferences);

    let help = gio::SimpleAction::new("help", None);
    help.connect_activate(glib::clone!(
        #[weak]
        app,
        move |_, _| {
            let Some(window) = app.active_window() else {
                return;
            };
            let dialog = adw::PreferencesDialog::new();
            dialog.set_title(&gettext("Briefcase Help"));
            let page = adw::PreferencesPage::new();

            let guide = adw::PreferencesGroup::builder()
                .title(&gettext("Quick Guide"))
                .description(&gettext(
                    "Briefcase synchronizes a Source folder with a portable Briefcase folder only when you ask it to.",
                ))
                .build();
            add_help_row(
                &guide,
                "document-new-symbolic",
                &gettext("Create a Briefcase"),
                &gettext("Choose the Source first, then a destination such as a USB drive."),
            );
            add_help_row(
                &guide,
                "document-open-symbolic",
                &gettext("Open a Briefcase"),
                &gettext("Open an existing Briefcase folder to add it to the home screen."),
            );
            add_help_row(
                &guide,
                "view-refresh-symbolic",
                &gettext("Check and Synchronize"),
                &gettext("Check previews changes. Review conflicts, then choose Synchronize to apply them."),
            );
            add_help_row(
                &guide,
                "dialog-warning-symbolic",
                &gettext("Resolve Conflicts"),
                &gettext("Choose which copy to keep, accept a deletion, or leave the item unchanged."),
            );
            page.add(&guide);

            let options = adw::PreferencesGroup::builder()
                .title(&gettext("Preferences"))
                .build();
            add_help_row(
                &options,
                "edit-delete-symbolic",
                &gettext("When a file is deleted"),
                &gettext("Ask what to do, skip it, restore the deleted file, or delete the other copy."),
            );
            add_help_row(
                &options,
                "system-search-symbolic",
                &gettext("Change detection"),
                &gettext("Fast mode uses saved file details; Verified mode hashes every file for greater certainty."),
            );
            page.add(&options);
            dialog.add(&page);
            dialog.present(Some(&window));
        }
    ));
    app.add_action(&help);

    let about = gio::SimpleAction::new("about", None);
    about.connect_activate(glib::clone!(
        #[weak]
        app,
        move |_, _| {
            let Some(window) = app.active_window() else {
                return;
            };
            let dialog = adw::AboutDialog::builder()
                .application_name(&gettext("Briefcase"))
                .application_icon(APP_ID)
                .version("0.1.0")
                .developer_name(&gettext("GNOME Briefcase contributors"))
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

fn add_help_row(group: &adw::PreferencesGroup, icon: &str, title: &str, subtitle: &str) {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    group.add(&row);
}

fn localized_error(error: &BriefcaseError) -> String {
    match error {
        BriefcaseError::Io { path, source } => gettext("Could not access {path}: {error}")
            .replace("{path}", &path.to_string_lossy())
            .replace("{error}", &source.to_string()),
        BriefcaseError::InvalidBriefcase(path) => {
            gettext("The folder is not a valid Briefcase: {path}")
                .replace("{path}", &path.to_string_lossy())
        }
        BriefcaseError::DestinationNotEmpty(path) => {
            gettext("The destination folder already exists and is not empty: {path}")
                .replace("{path}", &path.to_string_lossy())
        }
        BriefcaseError::OverlappingRoots => {
            gettext("The Source and Briefcase folders cannot contain one another")
        }
        BriefcaseError::UnsafePath(path) => {
            gettext("The path “{path}” does not remain inside the synchronized folder")
                .replace("{path}", &path.to_string_lossy())
        }
        BriefcaseError::UnsupportedSymlink(path) => {
            gettext("Symbolic links are not supported yet: {path}")
                .replace("{path}", &path.to_string_lossy())
        }
        BriefcaseError::AlreadyLocked => {
            gettext("Another synchronization is modifying this Briefcase")
        }
        BriefcaseError::Cancelled => gettext("Synchronization was stopped"),
        BriefcaseError::SourceUnavailable(path) => {
            gettext("The Source is unavailable: {path}").replace("{path}", &path.to_string_lossy())
        }
        BriefcaseError::UnresolvedConflict(path) => {
            gettext("Unresolved conflict: {path}").replace("{path}", &path.to_string_lossy())
        }
        BriefcaseError::Database(source) => {
            gettext("Database error: {error}").replace("{error}", &source.to_string())
        }
        BriefcaseError::Manifest(source) => {
            gettext("Invalid Briefcase metadata: {error}").replace("{error}", &source.to_string())
        }
        BriefcaseError::Other(source) => source.to_string(),
    }
}

fn show_error(toasts: &adw::ToastOverlay, message: &str) {
    let toast = adw::Toast::new(message);
    toast.set_timeout(8);
    toasts.add_toast(toast);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gnome_briefcase::ChangeState;

    fn deletion(action: SyncAction) -> PlannedOperation {
        PlannedOperation {
            relative_path: PathBuf::from("deleted.txt"),
            kind: EntryKind::File,
            source_state: ChangeState::Unchanged,
            briefcase_state: ChangeState::Deleted,
            action,
        }
    }

    #[test]
    fn deletion_choices_skip_restore_or_propagate_the_deletion() {
        let mut skipped = deletion(SyncAction::DeleteSource);
        apply_deletion_choice(&mut skipped, DeletionChoice::Skip);
        assert_eq!(skipped.action, SyncAction::None);

        let mut restored = deletion(SyncAction::DeleteSource);
        apply_deletion_choice(&mut restored, DeletionChoice::Restore);
        assert_eq!(restored.action, SyncAction::SourceToBriefcase);

        let mut deleted = deletion(SyncAction::DeleteSource);
        apply_deletion_choice(&mut deleted, DeletionChoice::Delete);
        assert_eq!(deleted.action, SyncAction::DeleteSource);
    }
}
