#[cfg(feature = "gui")]
mod application;

#[cfg(feature = "gui")]
fn main() -> glib::ExitCode {
    application::run()
}

#[cfg(not(feature = "gui"))]
fn main() {
    eprintln!(
        "GNOME Briefcase se compila con la característica `gui`.\n\
         Use: cargo run --features gui"
    );
}
