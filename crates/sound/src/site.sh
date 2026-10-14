#!/bin/sh
# Written by Henry for a static site. It publishes what Vercel would: the
# files a deploy leaves out (.vercelignore) are gone before the build, and
# only the output directory is served, never dotfiles or node_modules.
#
#   site.sh prune            drop .git and what .vercelignore names from /src
#   site.sh publish DIR      copy the output directory ($SITE_OUT) into DIR
#   site.sh serve            build with the app's env, publish, serve on :8080
set -eu
src=/src

prune() {
    rm -rf "$src/.git"
    [ -f "$src/.vercelignore" ] || return 0
    while IFS= read -r line || [ -n "$line" ]; do
        line=$(printf '%s' "$line" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
        # Comments, blanks and re-includes (`!`) are skipped: what a
        # re-include would keep stays out, so nothing ignored is served.
        case $line in '' | '#'* | '!'*) continue ;; esac
        pat=${line%/}
        case $pat in
            /*) match=-path pat="$src$pat" ;;
            */*) match=-path pat="$src/$pat" ;;
            *) match=-name ;;
        esac
        find "$src" -mindepth 1 -path "$src/.henry" -prune -o "$match" "$pat" -prune -exec rm -rf {} +
    done <"$src/.vercelignore"
}

publish() {
    out="$src/${SITE_OUT:-.}"
    [ -d "$out" ] || { echo "henry: the site's output directory ${SITE_OUT:-.} is missing" >&2; exit 1; }
    rm -rf "$1" && mkdir -p "$1"
    cp -a "$out/." "$1/"
    find "$1" -mindepth 1 \( -name '.*' -o -name node_modules -o -name vercel.json \) -prune -exec rm -rf {} +
}

serve() {
    cd "$src"
    # The build runs here, not in the image, so it sees the app's secrets as
    # a Vercel build sees its env, and no image ever holds them.
    sh "$src/.henry/site/build.sh"
    publish /srv
    exec caddy file-server --root /srv --listen :8080
}

case ${1:-} in
    prune) prune ;;
    publish) publish "${2:?publish needs a directory}" ;;
    serve) serve ;;
    *) echo "usage: site.sh prune | publish DIR | serve" >&2; exit 2 ;;
esac
