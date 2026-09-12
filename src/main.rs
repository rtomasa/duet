#[cfg(feature = "gui")]
mod application;

#[cfg(feature = "gui")]
fn main() -> glib::ExitCode {
    application::run()
}

#[cfg(not(feature = "gui"))]
fn main() {
    eprintln!(
        "GNOME Briefcase was built without the `gui` feature.\n\
         Run: cargo run --features gui"
    );
}
