#!/bin/sh
set -eu

root=$(CDPATH= cd "$(dirname "$0")/.." && pwd -P)
data=${XDG_DATA_HOME:-${HOME:?HOME must be set}/.local/share}
link=${HOME:?HOME must be set}/.local/bin/rap
target_dir=${CARGO_TARGET_DIR:-$root/target}
case $target_dir in
    /*) ;;
    *) target_dir=$PWD/$target_dir ;;
esac
binary=$target_dir/release/rap
entry=$data/applications/rap.desktop
manifest=$data/applications/rap.install-manifest
icon=$data/icons/hicolor/scalable/apps/rap.svg
png=$data/icons/hicolor/128x128/apps/rap.png
source_icon=$root/assets/rap.svg

action=${1:-install}
case $action in
    install|--uninstall) [ "$#" -le 1 ] ;;
    *) printf 'Usage: %s [--uninstall]\n' "$0" >&2; exit 2 ;;
esac

render_entry() {
    # Exec has both desktop-entry and command-line escaping rules.
    exec_link=$(printf '%s' "$link" | sed -e 's/\\/\\\\\\\\/g' -e 's/"/\\\\"/g' -e 's/[$`]/\\\\&/g' -e 's/%/%%/g')
    try_link=$(printf '%s' "$link" | sed 's/\\/\\\\/g')
    while IFS= read -r line || [ -n "$line" ]; do
        case $line in
            'Exec=rap') printf 'Exec="%s"\n' "$exec_link" ;;
            'TryExec=rap') printf 'TryExec=%s\n' "$try_link" ;;
            *) printf '%s\n' "$line" ;;
        esac
    done < "$root/packaging/rap.desktop"
}

exists() { [ -e "$1" ] || [ -L "$1" ]; }
hash_file() { sha256sum < "$1" | cut -d ' ' -f 1; }
valid_hash() {
    [ "${#1}" -eq 64 ] || return 1
    case $1 in *[!0123456789abcdef]*) return 1 ;; esac
    return 0
}

# An existing manifest is authoritative; never adopt an unknown or malformed one.
recorded=0
if exists "$manifest"; then
    if [ -L "$manifest" ] || [ ! -f "$manifest" ] ||
        ! {
            IFS= read -r marker &&
            IFS= read -r old_binary &&
            IFS= read -r old_entry_hash &&
            IFS= read -r old_icon_hash &&
            IFS= read -r old_png_hash &&
            ! IFS= read -r extra
        } < "$manifest" ||
        [ "$marker" != rap-install-v1 ] || [ -z "$old_binary" ] ||
        ! valid_hash "$old_entry_hash" ||
        ! valid_hash "$old_icon_hash" ||
        { [ "$old_png_hash" != - ] && ! valid_hash "$old_png_hash"; }; then
        printf 'Unrecognized Rap install manifest; leaving %s\n' "$manifest" >&2
        exit 1
    fi
    recorded=1
fi

owned() {
    case $1 in
        "$link")
            [ -L "$link" ] || return 1
            if [ "$recorded" -eq 1 ]; then
                [ "$(readlink "$link")" = "$old_binary" ]
            else
                [ "$(readlink "$link")" = "$binary" ]
            fi ;;
        "$entry")
            [ -f "$entry" ] && [ ! -L "$entry" ] || return 1
            if [ "$recorded" -eq 1 ]; then
                [ "$(hash_file "$entry")" = "$old_entry_hash" ]
            else
                render_entry | cmp -s - "$entry"
            fi ;;
        "$icon")
            [ -f "$icon" ] && [ ! -L "$icon" ] || return 1
            if [ "$recorded" -eq 1 ]; then
                [ "$(hash_file "$icon")" = "$old_icon_hash" ]
            else
                cmp -s "$source_icon" "$icon"
            fi ;;
        "$png")
            [ -f "$png" ] && [ ! -L "$png" ] || return 1
            if [ "$recorded" -eq 1 ]; then
                [ "$old_png_hash" != - ] && [ "$(hash_file "$png")" = "$old_png_hash" ]
            else
                command -v rsvg-convert >/dev/null 2>&1 &&
                    rsvg-convert -w 128 -h 128 "$source_icon" | cmp -s - "$png"
            fi ;;
    esac
}

if [ "$action" = --uninstall ]; then
    failed=0
    for path in "$link" "$entry" "$icon" "$png"; do
        if exists "$path"; then
            if owned "$path"; then
                rm -- "$path"
                printf 'Removed %s\n' "$path"
            else
                printf 'Not owned by Rap; leaving %s\n' "$path" >&2
                failed=1
            fi
        fi
    done
    if [ "$failed" -eq 0 ] && [ "$recorded" -eq 1 ]; then
        rm -- "$manifest"
        printf 'Removed %s\n' "$manifest"
    fi
    exit "$failed"
fi

if [ ! -x "$binary" ]; then
    printf 'Missing executable %s; first run cargo build --release\n' "$binary" >&2
    exit 1
fi

# Check all destinations before modifying any of them.
for path in "$link" "$entry" "$icon" "$png"; do
    if exists "$path" && ! owned "$path"; then
        printf 'Not owned by Rap; refusing to overwrite %s\n' "$path" >&2
        exit 1
    fi
done

# Render everything before touching installed destinations. Backups let a failed
# replacement restore the previous manifest and its matching installed files.
mkdir -p "$(dirname "$link")" "$(dirname "$entry")" "$(dirname "$icon")"
stage=$(mktemp -d "$data/applications/.rap-stage.XXXXXX")
applied=
cleanup() {
    status=$1
    if [ "$status" -ne 0 ]; then
        for name in $applied; do
            case $name in
                link) path=$link ;;
                entry) path=$entry ;;
                icon) path=$icon ;;
                png) path=$png ;;
                manifest) path=$manifest ;;
            esac
            if exists "$stage/$name.old"; then
                mv -f -- "$stage/$name.old" "$path" || :
            else
                rm -f -- "$path" || :
            fi
        done
    fi
    rm -rf -- "$stage"
}
trap 'exit 1' HUP INT TERM
trap 'cleanup $?' EXIT

render_entry > "$stage/entry"
chmod 644 "$stage/entry"
install -m 644 "$source_icon" "$stage/icon"
if command -v rsvg-convert >/dev/null 2>&1; then
    rsvg-convert -w 128 -h 128 "$source_icon" -o "$stage/png"
fi
ln -s "$binary" "$stage/link"
png_hash=-
if [ -f "$stage/png" ]; then
    png_hash=$(hash_file "$stage/png")
elif exists "$png"; then
    png_hash=$(hash_file "$png")
fi
printf 'rap-install-v1\n%s\n%s\n%s\n%s\n' \
    "$binary" "$(hash_file "$stage/entry")" "$(hash_file "$stage/icon")" "$png_hash" > "$stage/manifest"
chmod 644 "$stage/manifest"

replace() {
    name=$1
    destination=$2
    if exists "$destination"; then
        if [ "$name" = link ]; then
            ln -s "$(readlink "$destination")" "$stage/$name.old"
        else
            cp -p -- "$destination" "$stage/$name.old"
        fi
    fi
    applied="$name $applied"
    mv -f -- "$stage/$name" "$destination"
}
if ! exists "$link" || [ "$(readlink "$link")" != "$binary" ]; then
    replace link "$link"
fi
replace entry "$entry"
replace icon "$icon"
if [ -f "$stage/png" ]; then
    mkdir -p "$(dirname "$png")"
    replace png "$png"
fi
replace manifest "$manifest"
printf 'Installed %s -> %s, %s, %s\n' "$link" "$binary" "$entry" "$icon"
