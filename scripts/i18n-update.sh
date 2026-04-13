#!/bin/bash
# i18n-update.sh — Extract translatable strings from Slint files,
# update existing .po translations, and compile to .mo for runtime use.
#
# Prerequisites:
#   cargo install slint-tr-extractor
#   sudo apt install gettext  (for msginit, msgmerge, msgfmt)
#
# Usage:
#   ./scripts/i18n-update.sh          # full update
#   ./scripts/i18n-update.sh extract  # extract .pot only
#   ./scripts/i18n-update.sh compile  # compile .mo only

set -euo pipefail
cd "$(dirname "$0")/.."

DOMAIN="quay"
POT_FILE="i18n/${DOMAIN}.pot"
LOCALES=("pt-BR")

extract_pot() {
    echo "==> Extracting @tr() strings from .slint files..."
    find ui/ -name '*.slint' | xargs slint-tr-extractor -o "$POT_FILE"
    echo "    Written: $POT_FILE ($(grep -c '^msgid ' "$POT_FILE") strings)"
}

update_po() {
    for locale in "${LOCALES[@]}"; do
        PO_DIR="i18n/${locale}/LC_MESSAGES"
        PO_FILE="${PO_DIR}/${DOMAIN}.po"
        mkdir -p "$PO_DIR"

        if [ -f "$PO_FILE" ]; then
            echo "==> Merging new strings into ${PO_FILE}..."
            msgmerge -U "$PO_FILE" "$POT_FILE"
        else
            echo "==> Initialising ${PO_FILE} from template..."
            msginit -i "$POT_FILE" -o "$PO_FILE" -l "${locale//-/_}" --no-translator
        fi
    done
}

compile_mo() {
    for locale in "${LOCALES[@]}"; do
        PO_DIR="i18n/${locale}/LC_MESSAGES"
        PO_FILE="${PO_DIR}/${DOMAIN}.po"
        MO_FILE="${PO_DIR}/${DOMAIN}.mo"

        if [ -f "$PO_FILE" ]; then
            echo "==> Compiling ${MO_FILE}..."
            msgfmt "$PO_FILE" -o "$MO_FILE"
        else
            echo "    SKIP: ${PO_FILE} does not exist yet"
        fi
    done
}

case "${1:-all}" in
    extract)
        extract_pot
        ;;
    compile)
        compile_mo
        ;;
    all)
        extract_pot
        update_po
        compile_mo
        echo ""
        echo "Done. Rust-side strings live in locales/*.yml (rust-i18n)."
        echo "Slint-side strings live in i18n/**/*.po (gettext)."
        ;;
    *)
        echo "Usage: $0 [extract|compile|all]"
        exit 1
        ;;
esac
