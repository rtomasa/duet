project('gnome-briefcase', 'rust', version: '0.1.0', meson_version: '>= 0.63')

gnome = import('gnome')
i18n = import('i18n')
cargo = find_program('cargo')

app_id = 'io.github.rtomasa.Briefcase'
prefix = get_option('prefix')
datadir = prefix / get_option('datadir')

desktop = i18n.merge_file(
  input: 'data' / app_id + '.desktop.in',
  output: app_id + '.desktop',
  type: 'desktop',
  po_dir: 'po',
  install: true,
  install_dir: datadir / 'applications',
)

metainfo = i18n.merge_file(
  input: 'data' / app_id + '.metainfo.xml.in',
  output: app_id + '.metainfo.xml',
  type: 'xml',
  po_dir: 'po',
  install: true,
  install_dir: datadir / 'metainfo',
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
  'gnome-briefcase-binary',
  output: 'gnome-briefcase',
  command: [
    find_program('sh'), '-c',
    'GNOME_BRIEFCASE_LOCALEDIR="$5" CARGO_TARGET_DIR="$1" "$2" build --locked --release --features gui --manifest-path "$3" && cp "$1/release/gnome-briefcase" "$4"',
    'cargo-build', target_dir, cargo.full_path(), meson.project_source_root() / 'Cargo.toml', '@OUTPUT@', datadir / 'locale',
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

subdir('po')
