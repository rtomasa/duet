project('duet', 'rust', version: '0.2.0', meson_version: '>= 0.63')

gnome = import('gnome')
cargo = find_program('cargo')

app_id = 'io.github.rtomasa.Duet'
prefix = get_option('prefix')
datadir = prefix / get_option('datadir')

install_data(
  'data' / app_id + '.desktop.in',
  install_dir: datadir / 'applications',
  rename: app_id + '.desktop',
)

install_data(
  'data' / app_id + '.metainfo.xml.in',
  install_dir: datadir / 'metainfo',
  rename: app_id + '.metainfo.xml',
)

install_data(
  'data' / app_id + '.gschema.xml',
  install_dir: datadir / 'glib-2.0' / 'schemas',
)
install_data(
  'data/icons/hicolor/scalable/apps' / app_id + '.svg',
  install_dir: datadir / 'icons/hicolor/scalable/apps',
)

target_dir = meson.current_build_dir() / 'cargo-target'
binary = custom_target(
  'duet-binary',
  output: 'duet',
  command: [
    find_program('sh'), '-c',
    'CARGO_TARGET_DIR="$1" "$2" build --locked --release --features gui --manifest-path "$3" && cp "$1/release/duet" "$4"',
    'cargo-build', target_dir, cargo.full_path(), meson.project_source_root() / 'Cargo.toml', '@OUTPUT@',
  ],
  build_by_default: true,
  install: true,
  install_dir: prefix / get_option('bindir'),
)

gnome.post_install(
  glib_compile_schemas: true,
  gtk_update_icon_cache: true,
  update_desktop_database: true,
)
