#!/bin/sh
set -eu

for catalog in "$@"; do
  if msgattrib --untranslated --no-obsolete "$catalog" | grep -q '^msgid '; then
    echo "Untranslated messages found in $catalog" >&2
    exit 1
  fi
  if msgattrib --only-fuzzy --no-obsolete "$catalog" | grep -q '^#, fuzzy'; then
    echo "Fuzzy translations found in $catalog" >&2
    exit 1
  fi
done
