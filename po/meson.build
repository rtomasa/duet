i18n.gettext(
  'gnome-briefcase',
  preset: 'glib',
  install: true,
)

translation_catalogs = []
foreach language : ['de', 'es', 'fr', 'it', 'ja', 'pt', 'ru', 'zh_CN']
  translation_catalogs += meson.current_source_dir() / language + '.po'
endforeach

test(
  'translations-complete',
  find_program('sh'),
  args: [files('check-translations.sh'), translation_catalogs],
)
